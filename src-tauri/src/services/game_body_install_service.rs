use crate::{
    domain::{
        game::{CloudStatus, LaunchConfig},
        is_safe_path_segment,
        path_utils::normalize_path,
        AppStore, Game, GameHealth, GameLifecycle, SaveProfile,
    },
    repositories::store_file::atomic_replace,
    services::{
        cloud_manifest_service::{CloudBodyManifest, CloudBodyManifestVersion, CloudGameCatalog},
        RemoteFile,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

pub struct GameBodyInstallService;

#[derive(Clone)]
pub struct CloudInstallTarget {
    pub game_uid: String,
    pub game_key: String,
    pub managed_path: PathBuf,
    pub existing: Option<Game>,
    pub replace_existing: bool,
}

#[derive(Serialize, Deserialize)]
struct InstallJournal {
    game_uid: String,
    transaction_id: String,
    version_id: String,
    had_managed_directory: bool,
}

struct InstallPaths {
    managed: PathBuf,
    staging: PathBuf,
    rollback: PathBuf,
    package: PathBuf,
    journal: PathBuf,
}

impl GameBodyInstallService {
    pub fn resolve_target(
        store: &AppStore,
        games_root: &Path,
        remote_uid: &str,
        game_key: &str,
        replace_existing: bool,
    ) -> Result<CloudInstallTarget, String> {
        if !is_safe_path_segment(remote_uid) || !is_safe_path_segment(game_key) {
            return Err("云端游戏标识无效".to_string());
        }
        let mut matches = store.games.iter().filter(|game| game.game_key == game_key);
        let existing = matches.next().cloned();
        if matches.next().is_some() {
            return Err("本地存在重复 gameKey，请先处理重复游戏记录".to_string());
        }
        if store
            .games
            .iter()
            .any(|game| game.game_uid == remote_uid && game.game_key != game_key)
        {
            return Err("云端游戏 UID 与本地其他游戏冲突，未修改本地游戏".to_string());
        }
        if existing.as_ref().is_some_and(|game| {
            !matches!(
                game.lifecycle,
                GameLifecycle::Active | GameLifecycle::NeedsRepair
            )
        }) {
            return Err("本地游戏尚未完成设置，请先完成或取消添加".to_string());
        }
        let local_uid = existing
            .as_ref()
            .map(|game| game.game_uid.as_str())
            .unwrap_or(remote_uid);
        if !is_safe_path_segment(local_uid) {
            return Err("本地游戏标识无效".to_string());
        }
        let managed_path = games_root.join(local_uid);
        if games_root
            .join(format!(".{local_uid}.cloud-install.json"))
            .exists()
        {
            return Err("该游戏有尚未恢复的安装事务，请重启应用完成恢复后重试".to_string());
        }
        if games_root
            .join(format!(".{local_uid}.cloud-installing"))
            .exists()
        {
            return Err("该游戏存在未完成的安装暂存目录，请重启应用清理后重试".to_string());
        }
        if existing.as_ref().is_some_and(|game| {
            Path::new(&game.managed_path)
                .components()
                .any(|part| matches!(part, Component::ParentDir))
                || normalize_path(Path::new(&game.managed_path)) != normalize_path(&managed_path)
        }) {
            return Err("本地游戏目录不在当前受管游戏位置，已停止安装".to_string());
        }
        match fs::symlink_metadata(&managed_path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err("游戏目标位置不是普通目录，已停止安装".to_string());
                }
                if existing.is_none() {
                    return Err("游戏目标目录已存在但没有对应记录，未覆盖现有文件".to_string());
                }
                if !replace_existing {
                    return Err("本地游戏目录已存在，请确认后重新安装或切换版本".to_string());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("读取游戏目标目录失败：{error}")),
        }
        Ok(CloudInstallTarget {
            game_uid: local_uid.to_string(),
            game_key: game_key.to_string(),
            managed_path,
            existing,
            replace_existing,
        })
    }

    pub fn package_entry(
        manifest: &CloudBodyManifest,
        remote: &RemoteFile,
        game_key: &str,
    ) -> Result<CloudBodyManifestVersion, String> {
        if manifest.game_key != game_key {
            return Err("云端版本清单不属于当前游戏".to_string());
        }
        let mut matches = manifest
            .versions
            .iter()
            .filter(|version| version.package_path == remote.path);
        let entry = matches
            .next()
            .ok_or_else(|| "云端本体包未登记在版本清单中，请先修复清单".to_string())?;
        if matches.next().is_some()
            || entry.package_fs_id != remote.fs_id
            || entry.package_size != remote.size
            || !is_safe_path_segment(&entry.version_id)
        {
            return Err("云端本体包与版本清单不一致，请刷新商店或修复清单后重试".to_string());
        }
        if entry.package_sha256.as_ref().is_none_or(|hash| {
            hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            return Err("云端本体包缺少有效的 SHA-256，请修复清单或重新上传".to_string());
        }
        if entry.file_count == 0 {
            return Err("云端本体包文件数量无效".to_string());
        }
        Ok(entry.clone())
    }

    pub fn installed_game(target: &CloudInstallTarget, catalog: &CloudGameCatalog) -> Game {
        let mut game = target.existing.clone().unwrap_or_else(|| {
            Game::new_pending(
                &catalog.display_name,
                target.managed_path.to_string_lossy(),
                &catalog.executable_relative_path,
            )
        });
        game.game_uid = target.game_uid.clone();
        game.game_key = target.game_key.clone();
        game.managed_path = target.managed_path.to_string_lossy().into_owned();
        game.lifecycle = GameLifecycle::Active;
        game.health = GameHealth::NeedsAttention;
        game.cloud_status = CloudStatus::Synced;
        game.launch = LaunchConfig {
            executable_relative_path: catalog.executable_relative_path.clone(),
            arguments: catalog.arguments.clone(),
            working_directory_relative_path: catalog.working_directory_relative_path.clone(),
        };
        game
    }

    pub fn install_package_path(
        packages_root: &Path,
        game_uid: &str,
        transaction_id: &str,
    ) -> Result<PathBuf, String> {
        if !is_safe_path_segment(transaction_id) {
            return Err("云端安装事务标识无效".to_string());
        }
        super::BodyPackageService::package_path(
            packages_root,
            game_uid,
            &format!("install-{transaction_id}"),
        )
    }

    pub fn preserve_managed_saves(
        target: &CloudInstallTarget,
        profiles: &[SaveProfile],
        staging: &Path,
        is_cancelled: impl Fn() -> bool,
    ) -> Result<(), String> {
        let Some(game) = target
            .existing
            .as_ref()
            .filter(|_| target.managed_path.is_dir())
        else {
            return Ok(());
        };
        let managed = target
            .managed_path
            .canonicalize()
            .map_err(|error| format!("读取当前游戏目录失败：{error}"))?;
        let mut copied = HashSet::new();
        for profile in profiles.iter().filter(|profile| {
            profile.enabled
                && profile.game_uid == game.game_uid
                && game.save_profile_id.as_deref() == Some(profile.profile_id.as_str())
        }) {
            for scope in &profile.scopes {
                let root = crate::repositories::save_repository::scope_root(game, scope);
                let root = match root.canonicalize() {
                    Ok(root) => root,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(format!("读取受保护存档目录失败：{error}")),
                };
                if !root.starts_with(&managed) {
                    continue;
                }
                for relative in
                    crate::repositories::save_repository::collect_protected_paths(&root, scope)?
                {
                    if is_cancelled() {
                        return Err("任务已取消".to_string());
                    }
                    let source = root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
                    let source = source
                        .canonicalize()
                        .map_err(|error| format!("读取受保护存档失败：{error}"))?;
                    let relative = source
                        .strip_prefix(&managed)
                        .map_err(|_| "受保护存档链接超出游戏目录，已停止安装".to_string())?;
                    if !copied.insert(normalize_path(relative)) {
                        continue;
                    }
                    let destination = staging.join(relative);
                    if let Some(parent) = destination.parent() {
                        fs::create_dir_all(parent)
                            .map_err(|error| format!("创建存档保留目录失败：{error}"))?;
                    }
                    let mut input = fs::File::open(&source)
                        .map_err(|error| format!("读取当前存档失败：{error}"))?;
                    let mut output = fs::File::create(&destination)
                        .map_err(|error| format!("保留当前存档失败：{error}"))?;
                    let mut buffer = vec![0u8; 1024 * 1024];
                    loop {
                        if is_cancelled() {
                            return Err("任务已取消".to_string());
                        }
                        let count = input
                            .read(&mut buffer)
                            .map_err(|error| format!("读取当前存档失败：{error}"))?;
                        if count == 0 {
                            break;
                        }
                        output
                            .write_all(&buffer[..count])
                            .map_err(|error| format!("保留当前存档失败：{error}"))?;
                    }
                    output
                        .sync_all()
                        .map_err(|error| format!("刷新保留存档失败：{error}"))?;
                }
            }
        }
        Ok(())
    }

    pub fn commit_install(
        games_root: &Path,
        packages_root: &Path,
        target: &CloudInstallTarget,
        transaction_id: &str,
        version_id: &str,
        persist: impl FnOnce() -> Result<(), String>,
    ) -> Result<bool, String> {
        let journal = InstallJournal {
            game_uid: target.game_uid.clone(),
            transaction_id: transaction_id.to_string(),
            version_id: version_id.to_string(),
            had_managed_directory: target.managed_path.exists(),
        };
        let paths = install_paths(games_root, packages_root, &journal)?;
        if paths.journal.exists() || paths.rollback.exists() {
            return Err("该游戏有未完成的安装事务，请重启应用恢复后重试".to_string());
        }
        if normalize_path(&target.managed_path) != normalize_path(&paths.managed)
            || (journal.had_managed_directory
                && (!target.replace_existing || target.existing.is_none()))
        {
            return Err("游戏安装目标已变化，未覆盖现有文件".to_string());
        }
        ensure_plain_directory(&paths.staging)?;
        if journal.had_managed_directory {
            ensure_plain_directory(&paths.managed)?;
        }
        let bytes = serde_json::to_vec(&journal)
            .map_err(|error| format!("序列化云端安装事务失败：{error}"))?;
        atomic_replace(&paths.journal, &bytes, "云端安装事务")?;
        let result = (|| {
            if journal.had_managed_directory {
                fs::rename(&paths.managed, &paths.rollback)
                    .map_err(|error| format!("暂存当前游戏本体失败：{error}"))?;
            }
            fs::rename(&paths.staging, &paths.managed)
                .map_err(|error| format!("提交云端游戏安装失败：{error}"))?;
            persist()
        })();
        if let Err(error) = result {
            return match rollback_install(&paths, &journal) {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(format!(
                    "{error}；安装回滚未完成，已保留恢复事务：{rollback_error}"
                )),
            };
        }
        match finalize_install(&paths) {
            Ok(()) => Ok(false),
            Err(error) => {
                crate::logging::warn(format!("游戏已安装，旧本体清理将在重启后继续：{error}"));
                Ok(true)
            }
        }
    }

    pub fn recover_pending_installs(
        games_root: &Path,
        packages_root: &Path,
        store: &AppStore,
    ) -> Result<usize, String> {
        if !games_root.is_dir() {
            return Ok(0);
        }
        let mut recovered = 0;
        let mut errors = Vec::new();
        for entry in
            fs::read_dir(games_root).map_err(|error| format!("读取云端安装事务失败：{error}"))?
        {
            let entry = entry.map_err(|error| format!("读取云端安装事务失败：{error}"))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with('.') || !name.ends_with(".cloud-install.json") {
                continue;
            }
            let result = (|| {
                let raw = fs::read(entry.path())
                    .map_err(|error| format!("读取云端安装事务失败：{error}"))?;
                let journal: InstallJournal = serde_json::from_slice(&raw)
                    .map_err(|error| format!("解析云端安装事务失败：{error}"))?;
                let paths = install_paths(games_root, packages_root, &journal)?;
                if normalize_path(&entry.path()) != normalize_path(&paths.journal) {
                    return Err("云端安装事务与文件名不一致，已保留现场".to_string());
                }
                let committed = store
                    .games
                    .iter()
                    .any(|game| game.game_uid == journal.game_uid)
                    && store.body_versions.iter().any(|version| {
                        version.game_uid == journal.game_uid
                            && version.version_id == journal.version_id
                            && version.package_path.as_ref().is_some_and(|path| {
                                normalize_path(Path::new(path)) == normalize_path(&paths.package)
                            })
                    });
                if committed {
                    if !paths.managed.exists() && paths.staging.is_dir() {
                        fs::rename(&paths.staging, &paths.managed)
                            .map_err(|error| format!("恢复已提交的游戏安装失败：{error}"))?;
                    }
                    ensure_plain_directory(&paths.managed)?;
                    finalize_install(&paths)
                } else {
                    rollback_install(&paths, &journal)
                }
            })();
            match result {
                Ok(()) => recovered += 1,
                Err(error) => errors.push(format!("{name}：{error}")),
            }
        }
        if errors.is_empty() {
            Ok(recovered)
        } else {
            Err(errors.join("；"))
        }
    }

    pub fn pending_install_game_uids(games_root: &Path) -> Result<Vec<String>, String> {
        if !games_root.is_dir() {
            return Ok(Vec::new());
        }
        let mut pending = Vec::new();
        for entry in
            fs::read_dir(games_root).map_err(|error| format!("读取待恢复安装失败：{error}"))?
        {
            let entry = entry.map_err(|error| format!("读取待恢复安装失败：{error}"))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(uid) = name
                .strip_prefix('.')
                .and_then(|name| name.strip_suffix(".cloud-install.json"))
            {
                if is_safe_path_segment(uid) {
                    pending.push(uid.to_string());
                }
            }
        }
        Ok(pending)
    }
}

fn install_paths(
    games_root: &Path,
    packages_root: &Path,
    journal: &InstallJournal,
) -> Result<InstallPaths, String> {
    if !is_safe_path_segment(&journal.game_uid)
        || !is_safe_path_segment(&journal.version_id)
        || uuid::Uuid::parse_str(&journal.transaction_id).is_err()
    {
        return Err("云端安装事务包含无效标识".to_string());
    }
    let uid = &journal.game_uid;
    Ok(InstallPaths {
        managed: games_root.join(uid),
        staging: games_root.join(format!(".{uid}.cloud-installing")),
        rollback: games_root.join(format!(".{uid}.cloud-rollback-{}", journal.transaction_id)),
        package: GameBodyInstallService::install_package_path(
            packages_root,
            uid,
            &journal.transaction_id,
        )?,
        journal: games_root.join(format!(".{uid}.cloud-install.json")),
    })
}

fn ensure_plain_directory(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("读取安装目录失败（{}）：{error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!("安装路径不是普通目录：{}", path.display()));
    }
    Ok(())
}

fn remove_directory(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            ensure_plain_directory(path)?;
            fs::remove_dir_all(path)
                .map_err(|error| format!("清理安装目录失败（{}）：{error}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("读取安装目录失败：{error}")),
    }
}

fn rollback_install(paths: &InstallPaths, journal: &InstallJournal) -> Result<(), String> {
    if journal.had_managed_directory && paths.rollback.exists() {
        ensure_plain_directory(&paths.rollback)?;
        if paths.managed.exists() {
            ensure_plain_directory(&paths.managed)?;
            if paths.staging.exists() {
                return Err("安装目录与暂存目录同时存在，已保留现场".to_string());
            }
            fs::rename(&paths.managed, &paths.staging)
                .map_err(|error| format!("暂存未提交的游戏本体失败：{error}"))?;
        }
        fs::rename(&paths.rollback, &paths.managed)
            .map_err(|error| format!("恢复原游戏本体失败：{error}"))?;
    } else if !journal.had_managed_directory && !paths.staging.exists() {
        remove_directory(&paths.managed)?;
    }
    remove_directory(&paths.staging)?;
    match fs::remove_file(&paths.package) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("清理未提交的安装 ZIP 失败：{error}")),
    }
    fs::remove_file(&paths.journal).map_err(|error| format!("清理云端安装事务失败：{error}"))
}

fn finalize_install(paths: &InstallPaths) -> Result<(), String> {
    remove_directory(&paths.rollback)?;
    remove_directory(&paths.staging)?;
    fs::remove_file(&paths.journal).map_err(|error| format!("清理云端安装事务失败：{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{GameBodyVersion, SaveScope},
        test_support::TempWorkspace,
    };

    struct Fixture {
        _workspace: TempWorkspace,
        games: PathBuf,
        packages: PathBuf,
        store: AppStore,
        target: CloudInstallTarget,
        journal: InstallJournal,
    }

    impl Fixture {
        fn new(replace: bool) -> Self {
            let workspace = TempWorkspace::new("cloud-install-transaction");
            let games = workspace.join("games");
            let packages = workspace.join("body-packages");
            fs::create_dir_all(&games).expect("games");
            fs::create_dir_all(&packages).expect("packages");
            let mut store = AppStore::default();
            if replace {
                let managed = games.join("local-game");
                fs::create_dir_all(managed.join("SaveData")).expect("original saves");
                fs::write(managed.join("game.exe"), b"original-game").expect("original game");
                fs::write(managed.join("SaveData/slot.dat"), b"current-save")
                    .expect("original save");
                let mut game =
                    Game::new_pending("Local title", managed.to_string_lossy(), "game.exe");
                game.game_uid = "local-game".to_string();
                game.game_key = "stable-key".to_string();
                game.lifecycle = GameLifecycle::Active;
                store.games.push(game);
            }
            let target = GameBodyInstallService::resolve_target(
                &store,
                &games,
                "remote-game",
                "stable-key",
                replace,
            )
            .expect("target");
            let journal = InstallJournal {
                game_uid: target.game_uid.clone(),
                transaction_id: uuid::Uuid::new_v4().to_string(),
                version_id: "version-1".to_string(),
                had_managed_directory: replace,
            };
            let paths = install_paths(&games, &packages, &journal).expect("paths");
            fs::create_dir_all(&paths.staging).expect("staging");
            fs::write(paths.staging.join("game.exe"), b"new-game").expect("new game");
            fs::create_dir_all(paths.package.parent().expect("package parent")).expect("cache");
            fs::write(&paths.package, b"downloaded-zip").expect("zip");
            Self {
                _workspace: workspace,
                games,
                packages,
                store,
                target,
                journal,
            }
        }

        fn paths(&self) -> InstallPaths {
            install_paths(&self.games, &self.packages, &self.journal).expect("paths")
        }

        fn write_journal(&self) {
            atomic_replace(
                &self.paths().journal,
                &serde_json::to_vec(&self.journal).expect("journal json"),
                "test install",
            )
            .expect("journal");
        }

        fn commit(&self, persist: impl FnOnce() -> Result<(), String>) -> Result<bool, String> {
            GameBodyInstallService::commit_install(
                &self.games,
                &self.packages,
                &self.target,
                &self.journal.transaction_id,
                &self.journal.version_id,
                persist,
            )
        }

        fn recover(&self, store: &AppStore) -> Result<usize, String> {
            GameBodyInstallService::recover_pending_installs(&self.games, &self.packages, store)
        }

        fn swap_without_persisting(&self) {
            self.write_journal();
            let paths = self.paths();
            if self.journal.had_managed_directory {
                fs::rename(&paths.managed, &paths.rollback).expect("move original");
            }
            fs::rename(&paths.staging, &paths.managed).expect("move new game");
        }

        fn version(&self) -> GameBodyVersion {
            GameBodyVersion {
                version_id: self.journal.version_id.clone(),
                game_uid: self.target.game_uid.clone(),
                created_at: "1".to_string(),
                archive_path: String::new(),
                file_count: 1,
                total_bytes: 8,
                package_path: Some(self.paths().package.to_string_lossy().into_owned()),
                sha256: Some("ab".repeat(32)),
                excluded_items: Vec::new(),
                upload_status: Some("synced".to_string()),
                remote_path: Some(
                    "/apps/GameSaver/games/stable-key/body/version-1.zip".to_string(),
                ),
                remote_fs_id: Some(1),
                remote_size: Some(14),
            }
        }
    }

    fn catalog() -> CloudGameCatalog {
        CloudGameCatalog {
            format_version: 1,
            game_key: "stable-key".to_string(),
            game_uid: "remote-game".to_string(),
            display_name: "Renamed Game / 新名字".to_string(),
            executable_relative_path: "game.exe".to_string(),
            arguments: Vec::new(),
            working_directory_relative_path: None,
            has_cover: false,
        }
    }

    #[test]
    fn a_new_install_uses_the_remote_key_even_when_the_title_changed() {
        let fixture = Fixture::new(false);
        let game = GameBodyInstallService::installed_game(&fixture.target, &catalog());
        assert_eq!(game.game_key, "stable-key");
        assert_eq!(game.display_name, catalog().display_name);
        assert_eq!(game.game_uid, "remote-game");
    }

    #[test]
    fn a_matching_key_preserves_the_local_uid_and_local_metadata() {
        let fixture = Fixture::new(true);
        let game = GameBodyInstallService::installed_game(&fixture.target, &catalog());
        assert_eq!(game.game_uid, "local-game");
        assert_eq!(game.game_key, "stable-key");
        assert_eq!(game.display_name, "Local title");
    }

    #[test]
    fn an_unregistered_directory_is_never_overwritten() {
        let root = TempWorkspace::new("cloud-install-unknown-directory");
        fs::create_dir_all(root.join("remote-game")).expect("unknown directory");
        assert!(GameBodyInstallService::resolve_target(
            &AppStore::default(),
            &root,
            "remote-game",
            "stable-key",
            true
        )
        .is_err());
        assert!(root.join("remote-game").is_dir());
    }

    #[test]
    fn repairing_a_broken_registered_directory_requires_confirmation() {
        let fixture = Fixture::new(true);
        let paths = fixture.paths();
        fs::remove_dir_all(&paths.staging).expect("remove test staging");
        fs::remove_file(paths.managed.join("game.exe")).expect("break executable");
        assert!(GameBodyInstallService::resolve_target(
            &fixture.store,
            &fixture.games,
            "remote-game",
            "stable-key",
            false
        )
        .is_err());
        assert!(GameBodyInstallService::resolve_target(
            &fixture.store,
            &fixture.games,
            "remote-game",
            "stable-key",
            true
        )
        .is_ok());
    }

    #[test]
    fn conflicting_uid_or_duplicate_key_cannot_adopt_another_game() {
        let mut fixture = Fixture::new(true);
        fs::remove_dir_all(fixture.paths().staging).expect("staging");
        fixture.store.games[0].game_uid = "remote-game".to_string();
        fixture.store.games[0].game_key = "different-key".to_string();
        assert!(GameBodyInstallService::resolve_target(
            &fixture.store,
            &fixture.games,
            "remote-game",
            "stable-key",
            true
        )
        .is_err());
        fixture.store.games[0].game_key = "stable-key".to_string();
        fixture.store.games.push(fixture.store.games[0].clone());
        assert!(GameBodyInstallService::resolve_target(
            &fixture.store,
            &fixture.games,
            "remote-game",
            "stable-key",
            true
        )
        .is_err());
    }

    #[test]
    fn a_registered_path_outside_the_managed_location_is_rejected() {
        let mut fixture = Fixture::new(true);
        fixture.store.games[0].managed_path = fixture.packages.to_string_lossy().into_owned();
        assert!(GameBodyInstallService::resolve_target(
            &fixture.store,
            &fixture.games,
            "remote-game",
            "stable-key",
            true
        )
        .is_err());
    }

    #[test]
    fn cloud_entries_require_matching_identity_size_and_a_real_sha256() {
        let remote = RemoteFile {
            path: "/games/stable-key/body/v1.zip".to_string(),
            fs_id: 7,
            size: 15,
            md5: None,
            is_dir: false,
            server_mtime: None,
        };
        let entry = CloudBodyManifestVersion {
            version_id: "v1".to_string(),
            created_at: "1".to_string(),
            package_path: remote.path.clone(),
            package_fs_id: remote.fs_id,
            package_size: remote.size,
            package_sha256: Some("ab".repeat(32)),
            file_count: 1,
            total_bytes: 8,
        };
        let manifest = CloudBodyManifest {
            format_version: 1,
            game_key: "stable-key".to_string(),
            game_uid: "remote-game".to_string(),
            updated_at: "1".to_string(),
            versions: vec![entry],
        };
        assert!(GameBodyInstallService::package_entry(&manifest, &remote, "stable-key").is_ok());
        for variant in 0..6 {
            let mut bad = manifest.clone();
            match variant {
                0 => bad.versions[0].package_fs_id += 1,
                1 => bad.versions[0].package_size += 1,
                2 => bad.versions[0].package_sha256 = None,
                3 => bad.versions[0].package_sha256 = Some("not-a-hash".to_string()),
                4 => bad.game_key = "other-key".to_string(),
                _ => bad.versions.push(bad.versions[0].clone()),
            }
            assert!(
                GameBodyInstallService::package_entry(&bad, &remote, "stable-key").is_err(),
                "variant {variant}"
            );
        }
    }

    #[test]
    fn successful_install_cleans_the_transaction_without_removing_the_zip() {
        let fixture = Fixture::new(false);
        let paths = fixture.paths();
        assert!(!fixture
            .commit(|| {
                assert_eq!(
                    fs::read(paths.managed.join("game.exe")).expect("installed"),
                    b"new-game"
                );
                assert!(paths.journal.is_file());
                Ok(())
            })
            .expect("commit"));
        assert!(!paths.journal.exists());
        assert!(!paths.staging.exists());
        assert!(paths.package.is_file());
    }

    #[test]
    fn record_failure_rolls_back_new_and_replacement_installs() {
        for replace in [false, true] {
            let fixture = Fixture::new(replace);
            let paths = fixture.paths();
            let previous_zip = paths.package.with_file_name("previous-version.zip");
            fs::write(&previous_zip, b"previous-zip").expect("previous zip");
            let error = fixture
                .commit(|| Err("simulated store failure".to_string()))
                .expect_err("persist failure");
            assert!(error.contains("simulated store failure"));
            if replace {
                assert_eq!(
                    fs::read(paths.managed.join("game.exe")).expect("original"),
                    b"original-game"
                );
                assert_eq!(
                    fs::read(paths.managed.join("SaveData/slot.dat")).expect("save"),
                    b"current-save"
                );
            } else {
                assert!(!paths.managed.exists());
            }
            assert!(!paths.package.exists());
            assert!(!paths.journal.exists());
            assert!(!paths.rollback.exists());
            assert!(!paths.staging.exists());
            assert_eq!(
                fs::read(previous_zip).expect("previous cache remains"),
                b"previous-zip"
            );
        }
    }

    #[test]
    fn malformed_journals_are_preserved_and_identify_the_game_to_block() {
        let fixture = Fixture::new(true);
        let paths = fixture.paths();
        fs::write(&paths.journal, b"broken journal").expect("malformed journal");
        assert!(fixture.recover(&fixture.store).is_err());
        assert_eq!(
            GameBodyInstallService::pending_install_game_uids(&fixture.games)
                .expect("pending games"),
            vec!["local-game"]
        );
        assert!(paths.staging.is_dir());
        assert!(paths.package.is_file());
        assert_eq!(
            fs::read(paths.managed.join("game.exe")).expect("original game"),
            b"original-game"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_locked_old_file_defers_cleanup_without_rolling_back_a_committed_game() {
        use std::os::windows::fs::OpenOptionsExt;
        let mut fixture = Fixture::new(true);
        let paths = fixture.paths();
        let locked = std::cell::RefCell::new(None);
        let pending = fixture
            .commit(|| {
                let file = fs::OpenOptions::new()
                    .read(true)
                    .share_mode(0)
                    .open(paths.rollback.join("game.exe"))
                    .expect("lock old executable");
                locked.replace(Some(file));
                Ok(())
            })
            .expect("committed installation");
        assert!(pending);
        assert!(paths.journal.is_file());
        assert_eq!(
            fs::read(paths.managed.join("game.exe")).expect("new game"),
            b"new-game"
        );
        fixture.store.body_versions.push(fixture.version());
        drop(locked.into_inner());
        fixture
            .recover(&fixture.store)
            .expect("finish deferred cleanup");
        assert!(!paths.journal.exists());
        assert!(!paths.rollback.exists());
        assert!(paths.package.is_file());
    }

    #[test]
    fn interrupted_installs_recover_before_and_after_directory_swap() {
        for phase in 0..3 {
            let fixture = Fixture::new(true);
            let paths = fixture.paths();
            fixture.write_journal();
            if phase >= 1 {
                fs::rename(&paths.managed, &paths.rollback).expect("old moved");
            }
            if phase == 2 {
                fs::rename(&paths.staging, &paths.managed).expect("new moved");
            }
            assert_eq!(fixture.recover(&fixture.store).expect("recover"), 1);
            assert_eq!(
                fs::read(paths.managed.join("game.exe")).expect("original"),
                b"original-game"
            );
            assert_eq!(
                fs::read(paths.managed.join("SaveData/slot.dat")).expect("save"),
                b"current-save"
            );
            assert!(!paths.package.exists());
            assert!(!paths.journal.exists());
            assert!(!paths.staging.exists());
            assert!(!paths.rollback.exists());
        }
    }

    #[test]
    fn an_interrupted_new_install_does_not_leave_an_unregistered_game() {
        let fixture = Fixture::new(false);
        fixture.swap_without_persisting();
        fixture.recover(&fixture.store).expect("recover");
        let paths = fixture.paths();
        assert!(!paths.managed.exists());
        assert!(!paths.package.exists());
        assert!(!paths.journal.exists());
        assert_eq!(
            fixture
                .recover(&fixture.store)
                .expect("idempotent recovery"),
            0
        );
    }

    #[test]
    fn committed_install_recovery_keeps_the_new_game_and_cleans_the_old_body() {
        let mut fixture = Fixture::new(true);
        fixture.swap_without_persisting();
        fixture.store.body_versions.push(fixture.version());
        fixture.recover(&fixture.store).expect("recover committed");
        let paths = fixture.paths();
        assert_eq!(
            fs::read(paths.managed.join("game.exe")).expect("new game"),
            b"new-game"
        );
        assert!(paths.package.is_file());
        assert!(!paths.rollback.exists());
        assert!(!paths.journal.exists());
    }

    #[test]
    fn an_older_record_for_the_same_version_is_not_a_new_commit() {
        let mut fixture = Fixture::new(true);
        let mut old_version = fixture.version();
        old_version.package_path = Some(
            fixture
                .packages
                .join("old.zip")
                .to_string_lossy()
                .into_owned(),
        );
        fixture.store.body_versions.push(old_version);
        fixture.swap_without_persisting();
        fixture
            .recover(&fixture.store)
            .expect("roll back uncommitted reinstall");
        assert_eq!(
            fs::read(fixture.paths().managed.join("game.exe")).expect("old game"),
            b"original-game"
        );
    }

    #[test]
    fn replacing_a_game_keeps_only_its_configured_protected_saves() {
        let mut fixture = Fixture::new(true);
        let paths = fixture.paths();
        let mut scope = SaveScope::new_manual(
            paths
                .managed
                .join("SaveData")
                .to_string_lossy()
                .into_owned(),
        );
        scope.confirmed_files = vec!["slot.dat".to_string()];
        let profile = SaveProfile::new(
            fixture.target.game_uid.clone(),
            String::new(),
            vec![scope],
            100,
            "1".to_string(),
        );
        fixture
            .target
            .existing
            .as_mut()
            .expect("existing game")
            .save_profile_id = Some(profile.profile_id.clone());
        fs::write(paths.managed.join("SaveData/debug.log"), b"noise").expect("noise");
        GameBodyInstallService::preserve_managed_saves(
            &fixture.target,
            &[profile],
            &paths.staging,
            || false,
        )
        .expect("preserve saves");
        assert_eq!(
            fs::read(paths.staging.join("SaveData/slot.dat")).expect("staged save"),
            b"current-save"
        );
        assert!(!paths.staging.join("SaveData/debug.log").exists());
        fixture.commit(|| Ok(())).expect("replace");
        assert_eq!(
            fs::read(paths.managed.join("game.exe")).expect("new game"),
            b"new-game"
        );
        assert_eq!(
            fs::read(paths.managed.join("SaveData/slot.dat")).expect("current save"),
            b"current-save"
        );
        assert!(!paths.rollback.exists());
    }

    #[test]
    fn invalid_recovery_identifiers_never_form_external_paths() {
        let mut fixture = Fixture::new(false);
        fixture.journal.game_uid = "../outside".to_string();
        assert!(install_paths(&fixture.games, &fixture.packages, &fixture.journal).is_err());
        fixture.journal.game_uid = "remote-game".to_string();
        fixture.journal.transaction_id = "not-a-uuid".to_string();
        assert!(install_paths(&fixture.games, &fixture.packages, &fixture.journal).is_err());
    }
}
