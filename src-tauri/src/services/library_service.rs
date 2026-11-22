use crate::domain::{
    path_utils::{normalize_path, strip_verbatim_prefix},
    is_safe_path_segment, AppStore,
};
use std::{
    collections::HashMap,
    fs,
    path::{Component, Path, PathBuf},
};
use walkdir::WalkDir;

mod migration;
pub use migration::{LibraryMigration, MigrationOutcome};

const MANAGED_DIRECTORIES: [&str; 4] = ["games", "body-packages", "saves", "covers"];

#[derive(Debug, Clone, Default)]
pub struct LibraryUsage {
    pub games_bytes: u64,
    pub body_packages_bytes: u64,
    pub saves_bytes: u64,
    pub covers_bytes: u64,
    pub file_count: usize,
}

impl LibraryUsage {
    pub fn total_bytes(&self) -> u64 {
        self.games_bytes
            .saturating_add(self.body_packages_bytes)
            .saturating_add(self.saves_bytes)
            .saturating_add(self.covers_bytes)
    }
}

pub struct LibraryService;

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryStorageOverview {
    library_root: String,
    total_bytes: u64,
    free_bytes: Option<u64>,
    shared_save_bytes: u64,
    other_bytes: u64,
    games: Vec<GameStorageUsage>,
}

#[derive(Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct GameStorageUsage {
    game_uid: String,
    display_name: String,
    game_bytes: u64,
    package_bytes: u64,
    save_bytes: u64,
}

impl LibraryService {
    pub fn storage_overview(root: &Path, store: &AppStore) -> Result<LibraryStorageOverview, String> {
        fs::read_dir(root).map_err(|error| format!("无法读取游戏库目录：{error}"))?;
        let mut overview = LibraryStorageOverview {
            library_root: root.to_string_lossy().into_owned(),
            total_bytes: 0,
            free_bytes: super::disk_space::available_space(root).ok(),
            shared_save_bytes: 0,
            other_bytes: 0,
            games: store
                .games
                .iter()
                .map(|game| GameStorageUsage {
                    game_uid: game.game_uid.clone(),
                    display_name: game.display_name.clone(),
                    ..GameStorageUsage::default()
                })
                .collect(),
        };
        let game_indices: HashMap<_, _> = store
            .games
            .iter()
            .enumerate()
            .map(|(index, game)| (game.game_uid.as_str(), index))
            .collect();
        let games_root = root.join("games");
        let game_paths: HashMap<_, _> = store
            .games
            .iter()
            .enumerate()
            .filter_map(|(index, game)| {
                let path = Path::new(&game.managed_path);
                let name = path.file_name()?.to_str()?;
                if !is_safe_path_segment(name) || !same_path(path, &games_root.join(name)) {
                    return None;
                }
                Some((name.to_ascii_lowercase(), index))
            })
            .collect();
        let package_paths: HashMap<_, _> = game_indices
            .iter()
            .map(|(uid, index)| (uid.to_ascii_lowercase(), *index))
            .collect();
        let mut save_owners = HashMap::new();
        for version in &store.save_versions {
            let Some(&owner) = game_indices.get(version.game_uid.as_str()) else {
                continue;
            };
            for hash in version
                .files
                .iter()
                .filter(|file| !file.deleted)
                .filter_map(|file| file.object_hash.as_deref())
            {
                if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    continue;
                }
                save_owners
                    .entry(hash.to_ascii_lowercase())
                    .and_modify(|current| {
                        if *current != Some(owner) {
                            *current = None;
                        }
                    })
                    .or_insert(Some(owner));
            }
        }
        let object_prefix = Path::new(".gamesaver-repository/objects/sha256");
        for category in MANAGED_DIRECTORIES {
            visit_storage_files(&root.join(category), |relative, bytes| {
                overview.total_bytes = overview.total_bytes.saturating_add(bytes);
                let first_dir = |indices: &HashMap<String, usize>| {
                    let name = relative.components().next()?.as_os_str().to_str()?;
                    indices.get(&name.to_ascii_lowercase()).copied()
                };
                let bucket = match category {
                    "games" if relative.components().nth(1).is_some() => {
                        first_dir(&game_paths).map(|index| &mut overview.games[index].game_bytes)
                    }
                    "body-packages"
                        if relative.components().count() == 2
                            && relative.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("zip")) =>
                    {
                        first_dir(&package_paths)
                            .map(|index| &mut overview.games[index].package_bytes)
                    }
                    "saves" => relative
                        .strip_prefix(object_prefix)
                        .ok()
                        .and_then(|path| {
                            let hash = path.file_name()?.to_str()?;
                            let shard = path.parent()?.as_os_str();
                            if hash.len() != 64 || !shard.eq_ignore_ascii_case(hash.get(..2)?) {
                                return None;
                            }
                            save_owners.get(&hash.to_ascii_lowercase()).copied()
                        })
                        .map(|owner| match owner {
                            Some(index) => &mut overview.games[index].save_bytes,
                            None => &mut overview.shared_save_bytes,
                        }),
                    _ => None,
                };
                let bucket = bucket.unwrap_or(&mut overview.other_bytes);
                *bucket = bucket.saturating_add(bytes);
            })?;
        }
        Ok(overview)
    }

    pub fn usage(root: &Path) -> Result<LibraryUsage, String> {
        let mut usage = LibraryUsage::default();
        usage.games_bytes = collect_usage(&root.join("games"), &mut usage.file_count)?;
        usage.body_packages_bytes =
            collect_usage(&root.join("body-packages"), &mut usage.file_count)?;
        usage.saves_bytes = collect_usage(&root.join("saves"), &mut usage.file_count)?;
        usage.covers_bytes = collect_usage(&root.join("covers"), &mut usage.file_count)?;
        Ok(usage)
    }

    pub fn validate_target(source: &Path, target: &Path) -> Result<(), String> {
        if !target.is_absolute() {
            return Err("游戏库根目录必须是绝对路径".to_string());
        }
        if source
            .components()
            .chain(target.components())
            .any(|part| matches!(part, Component::ParentDir))
        {
            return Err("游戏库路径不能包含上级目录跳转".to_string());
        }
        if same_path(source, target)
            || is_descendant(target, source)
            || is_descendant(source, target)
        {
            return Err("新游戏库目录不能与当前游戏库重叠".to_string());
        }
        let real_source = Self::resolve_path(source)?;
        let real_target = Self::resolve_path(target)?;
        if same_path(&real_source, &real_target)
            || is_descendant(&real_source, &real_target)
            || is_descendant(&real_target, &real_source)
        {
            return Err("新游戏库目录不能通过链接与当前游戏库重叠".to_string());
        }
        if target.exists() && !target.is_dir() {
            return Err("新游戏库路径不是文件夹".to_string());
        }
        if target.is_dir() {
            let has_unmanaged_entries = fs::read_dir(target)
                .map_err(|error| format!("读取新游戏库目录失败：{error}"))?
                .next()
                .is_some();
            if has_unmanaged_entries {
                return Err("新游戏库目录必须为空，避免覆盖已有文件".to_string());
            }
        }
        Ok(())
    }

    pub fn resolve_path(path: &Path) -> Result<PathBuf, String> {
        if !path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            return Err("游戏库必须使用不含上级跳转的绝对路径".to_string());
        }
        let mut ancestor = path.to_path_buf();
        let mut missing = Vec::new();
        loop {
            match fs::symlink_metadata(&ancestor) {
                Ok(_) => break,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    missing.push(
                        ancestor
                            .file_name()
                            .ok_or("无法解析游戏库路径")?
                            .to_os_string(),
                    );
                    ancestor = ancestor
                        .parent()
                        .ok_or("无法解析游戏库父目录")?
                        .to_path_buf();
                }
                Err(error) => return Err(format!("读取游戏库路径失败：{error}")),
            }
        }
        let mut resolved = strip_verbatim_prefix(
            &fs::canonicalize(&ancestor)
                .map_err(|error| format!("解析游戏库真实路径失败：{error}"))?,
        );
        for component in missing.into_iter().rev() {
            resolved.push(component);
        }
        Ok(resolved)
    }

    pub(super) fn copy_contents(
        source: &Path,
        staging: &Path,
        mut on_progress: impl FnMut(u8, &str) -> Result<(), String>,
    ) -> Result<(), String> {
        let usage = Self::usage(source)?;
        for (index, directory) in MANAGED_DIRECTORIES.iter().enumerate() {
            let source_dir = source.join(directory);
            let target_dir = staging.join(directory);
            copy_tree(&source_dir, &target_dir, |message| {
                let base = (index as u8) * 20;
                on_progress(base.saturating_add(10), &message)
            })?;
            on_progress((index as u8 + 1) * 20, &format!("已复制 {directory}"))?;
        }
        let staged_usage = Self::usage(staging)?;
        if staged_usage.total_bytes() != usage.total_bytes()
            || staged_usage.file_count != usage.file_count
        {
            return Err("游戏库迁移校验失败：文件数量或大小不一致".to_string());
        }
        Ok(())
    }

    /// 就地把仓库里所有指向 `source` 的路径改写为 `target`。
    /// 供游戏库迁移在**持有 store 锁**的状态下刷新内存态使用：
    /// 迁移耗时可能很长，落盘时必须基于当前的 store 而不是迁移开始时的快照。
    pub fn rewrite_paths_in_place(
        store: &mut AppStore,
        source: &Path,
        target: &Path,
    ) -> Result<(), String> {
        rewrite_store_paths(store, source, target)
    }

    pub fn cleanup_source(source: &Path) -> Result<(), String> {
        for directory in MANAGED_DIRECTORIES {
            let path = source.join(directory);
            if fs::symlink_metadata(&path)
                .map(|metadata| metadata.file_type().is_symlink())
                .unwrap_or(false)
            {
                return Err(format!(
                    "旧游戏库目录已被替换为链接，已停止清理：{}",
                    path.display()
                ));
            }
            if path.is_dir() {
                fs::remove_dir_all(&path).map_err(|error| {
                    format!("清理旧游戏库目录失败：{}：{error}", path.display())
                })?;
            }
        }
        Ok(())
    }
}

fn visit_storage_files(root: &Path, mut visit: impl FnMut(&Path, u64)) -> Result<(), String> {
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(format!("受管存储路径不是普通目录：{}", root.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("无法访问受管存储目录：{error}")),
    }
    for entry in WalkDir::new(root).follow_links(false).follow_root_links(false) {
        let entry = entry.map_err(|error| format!("统计游戏库空间失败：{error}"))?;
        if entry.file_type().is_file() {
            let relative = entry
                .path()
                .strip_prefix(root)
                .map_err(|error| format!("无法定位受管存储文件：{error}"))?;
            let metadata = entry
                .metadata()
                .map_err(|error| format!("读取游戏库文件大小失败：{error}"))?;
            visit(relative, metadata.len());
        }
    }
    Ok(())
}

fn collect_usage(root: &Path, file_count: &mut usize) -> Result<u64, String> {
    if !root.is_dir() {
        return Ok(0);
    }
    let mut total = 0u64;
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| format!("扫描游戏库失败：{error}"))?;
        if entry.file_type().is_file() {
            *file_count = file_count.saturating_add(1);
            total = total.saturating_add(
                entry
                    .metadata()
                    .map_err(|error| format!("读取游戏库文件信息失败：{error}"))?
                    .len(),
            );
        }
    }
    Ok(total)
}

fn copy_tree(
    source: &Path,
    target: &Path,
    mut on_file: impl FnMut(String) -> Result<(), String>,
) -> Result<(), String> {
    if !source.exists() {
        return Ok(());
    }
    for entry in WalkDir::new(source).follow_links(false) {
        let entry = entry.map_err(|error| format!("扫描待迁移数据失败：{error}"))?;
        if entry.file_type().is_symlink() {
            return Err(format!(
                "游戏库迁移不支持符号链接，已保留原目录：{}",
                entry.path().display()
            ));
        }
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|error| format!("计算迁移相对路径失败：{error}"))?;
        let destination = target.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&destination)
                .map_err(|error| format!("创建迁移目录失败：{error}"))?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("创建迁移文件目录失败：{error}"))?;
            }
            fs::copy(entry.path(), &destination)
                .map_err(|error| format!("复制游戏库文件失败：{error}"))?;
            on_file(format!("正在迁移 {}", relative.display()))?;
        }
    }
    Ok(())
}

fn rewrite_store_paths(store: &mut AppStore, source: &Path, target: &Path) -> Result<(), String> {
    for game in &mut store.games {
        game.managed_path = rewrite_path(&game.managed_path, source, target)?;
    }
    for version in &mut store.body_versions {
        version.archive_path = rewrite_path(&version.archive_path, source, target)?;
        if let Some(path) = version.package_path.as_mut() {
            *path = rewrite_path(path, source, target)?;
        }
    }
    for profile in &mut store.save_profiles {
        for scope in &mut profile.scopes {
            scope.root_path = rewrite_path(&scope.root_path, source, target)?;
        }
        for binding in &mut profile.root_bindings {
            binding.current_path = rewrite_path(&binding.current_path, source, target)?;
            binding.previous_path = rewrite_path(&binding.previous_path, source, target)?;
        }
    }
    for version in &mut store.save_versions {
        for file in &mut version.files {
            if let Some(path) = file.root_path.as_mut() {
                *path = rewrite_path(path, source, target)?;
            }
        }
    }
    Ok(())
}

fn rewrite_path(value: &str, source: &Path, target: &Path) -> Result<String, String> {
    let clean = strip_verbatim_prefix(Path::new(value.trim()));
    let clean = clean.to_string_lossy().replace('/', "\\");
    let source = normalized(source);
    let target = target
        .to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_string();
    if clean.to_ascii_lowercase() == source {
        return Ok(target);
    }
    let prefix = format!("{source}\\");
    if !clean.to_ascii_lowercase().starts_with(&prefix) {
        return Ok(value.to_string());
    }
    Ok(format!("{}\\{}", target, &clean[prefix.len()..]))
}

fn normalized(path: &Path) -> String {
    normalize_path(path)
}

fn same_path(left: &Path, right: &Path) -> bool {
    normalized(left) == normalized(right)
}

fn is_descendant(path: &Path, parent: &Path) -> bool {
    let path = normalized(path);
    let parent = normalized(parent);
    path.starts_with(&(parent + "\\"))
}

#[cfg(test)]
mod tests {
    use super::{rewrite_path, LibraryService};
    use crate::{
        domain::{AppStore, Game, SaveFileEntry, SaveRootType, SaveVersion},
        test_support::TempWorkspace,
    };
    use std::{fs, path::Path};

    #[test]
    fn storage_overview_counts_shared_objects_once_and_accounts_for_unassigned_files() {
        let root = TempWorkspace::new("library-storage-overview");
        let make_game = |name: &str| {
            Game::new_pending(name, root.join("games").join(name).to_string_lossy(), "game.exe")
        };
        let first = make_game("first");
        let second = make_game("second");
        let write = |relative: &str, size: usize| {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, vec![0; size]).unwrap();
        };
        write("games/first/bin/game.exe", 20);
        write("games/.versions/stale/file", 11);
        write(&format!("body-packages/{}/version.ZIP", first.game_uid), 10);
        write(&format!("body-packages/{}/download.part", first.game_uid), 7);
        write("covers/cover.png", 3);
        let exclusive = "a".repeat(64);
        let shared = "b".repeat(64);
        let orphan = "c".repeat(64);
        for (hash, size) in [(&exclusive, 4), (&shared, 6), (&orphan, 2)] {
            write(&format!("saves/.gamesaver-repository/objects/sha256/{}/{hash}", &hash[..2]), size);
        }
        let version = |game: &Game, hashes: &[&str]| {
            SaveVersion::new(
                game.game_uid.clone(),
                "1".to_string(),
                hashes.iter().map(|hash| SaveFileEntry {
                    root_type: SaveRootType::Custom,
                    root_path: None,
                    relative_path: "test.sav".to_string(),
                    object_hash: Some((*hash).to_string()),
                    size: 1024,
                    deleted: false,
                    mtime_ms: None,
                }).collect(),
            )
        };
        let store = AppStore {
            save_versions: vec![
                version(&first, &[&exclusive, &shared]),
                version(&first, &[&exclusive]),
                version(&second, &[&shared, &"d".repeat(64)]),
            ],
            games: vec![first, second],
            ..AppStore::default()
        };
        let overview = LibraryService::storage_overview(&root, &store).unwrap();
        let first = &overview.games[0];
        assert_eq!((first.game_bytes, first.package_bytes, first.save_bytes), (20, 10, 4));
        assert_eq!(overview.games[1].save_bytes, 0);
        assert_eq!((overview.total_bytes, overview.shared_save_bytes, overview.other_bytes), (63, 6, 23));
    }

    #[test]
    fn storage_overview_distinguishes_a_missing_library_from_an_empty_one() {
        let root = TempWorkspace::new("library-storage-empty");
        let store = AppStore::default();
        assert_eq!(LibraryService::storage_overview(&root, &store).unwrap().total_bytes, 0);
        assert!(LibraryService::storage_overview(&root.join("offline"), &store).is_err());
    }

    #[test]
    fn rejects_overlapping_library_roots() {
        assert!(LibraryService::validate_target(
            Path::new(r"E:\Games"),
            Path::new(r"E:\Games\new")
        )
        .is_err());
        assert!(
            LibraryService::validate_target(Path::new(r"E:\Games"), Path::new(r"E:\Games"))
                .is_err()
        );
    }

    #[test]
    fn leaves_external_unc_and_unicode_paths_unchanged() {
        for path in [
            r"\\?\UNC\server\share\存档",
            r"\\server\share\存档",
            r"C:\用户\存档",
        ] {
            assert_eq!(
                rewrite_path(path, Path::new(r"E:\Library"), Path::new(r"F:\Library")).unwrap(),
                path
            );
        }
        assert_eq!(
            rewrite_path(
                r"\\?\UNC\server\old\games\游戏",
                Path::new(r"\\server\old"),
                Path::new(r"F:\Library")
            )
            .unwrap(),
            r"F:\Library\games\游戏"
        );
    }

    #[test]
    fn rejects_parent_traversal_into_source_before_copying() {
        let root = TempWorkspace::new("library-overlap");
        std::fs::create_dir_all(root.join("source/games")).unwrap();
        std::fs::create_dir_all(root.join("bridge")).unwrap();
        assert!(LibraryService::validate_target(
            &root.join("source"),
            &root.join("bridge/../source/games/nested")
        )
        .is_err());
    }

    #[cfg(windows)]
    #[test]
    fn rejects_junction_alias_into_source() {
        use std::os::windows::process::CommandExt;
        let root = TempWorkspace::new("library-junction");
        let source = root.join("source");
        let alias = root.join("alias");
        std::fs::create_dir_all(source.join("games")).unwrap();
        let output = std::process::Command::new("cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(&alias)
            .arg(source.join("games"))
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "junction fixture could not be created"
        );
        let rejected = LibraryService::validate_target(&source, &alias.join("nested")).is_err();
        std::fs::remove_dir(&alias).unwrap();
        assert!(rejected);
    }
}
