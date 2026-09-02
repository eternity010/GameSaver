use crate::{app_state::AppState, domain::{Game, SaveFileEntry, SaveProfile, SaveRootType, SaveScope, SaveVersion}};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Manager};
use uuid::Uuid;
use walkdir::WalkDir;

static REPOSITORY_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static PENDING_OBJECTS: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();

pub struct SaveRepository;

struct CollectedFile {
    path: PathBuf,
    root_type: SaveRootType,
    root_path: Option<String>,
    relative_path: String,
}

impl SaveRepository {
    pub fn commit(
        app: &AppHandle,
        game: &Game,
        profile: &SaveProfile,
        latest: Option<&SaveVersion>,
        mut on_progress: impl FnMut(u8, &str),
    ) -> Result<Option<SaveVersion>, String> {
        let lock = REPOSITORY_LOCK.get_or_init(|| Mutex::new(()));
        let _guard = lock.lock().map_err(|_| "存档仓库锁定失败".to_string())?;
        let files = collect_profile_files(profile)?;
        let mut entries = Vec::with_capacity(files.len());
        for (index, file) in files.iter().enumerate() {
            let (bytes, size) = read_stable_file(&file.path)?;
            let hash = sha256_bytes(&bytes);
            Self::write_object_locked(app, &hash, &bytes)?;
            entries.push(SaveFileEntry {
                root_type: file.root_type,
                root_path: file.root_path.clone(),
                relative_path: file.relative_path.clone(),
                object_hash: Some(hash),
                size,
                deleted: false,
            });
            on_progress(
                (((index + 1) * 90) / files.len()) as u8,
                &format!("正在整理存档文件 {}/{}", index + 1, files.len()),
            );
        }
        if let Some(latest) = latest {
            for old_file in latest.files.iter().filter(|file| !file.deleted) {
                if !files.iter().any(|file| collected_matches_entry(file, old_file)) && entry_belongs_to_profile(old_file, profile) {
                    entries.push(SaveFileEntry {
                        root_type: old_file.root_type,
                        root_path: old_file.root_path.clone(),
                        relative_path: old_file.relative_path.clone(),
                        object_hash: None,
                        size: 0,
                        deleted: true,
                    });
                }
            }
        }
        if entries.is_empty() {
            return Ok(None);
        }
        entries.sort_by(|left, right| entry_key(left).cmp(&entry_key(right)));
        if latest.is_some_and(|version| same_entries(&version.files, &entries)) {
            return Ok(None);
        }
        let version = SaveVersion::new(game.game_uid.clone(), now_iso(), entries);
        protect_pending_objects(&version)?;
        on_progress(100, "存档版本已准备完成");
        Ok(Some(version))
    }

    pub fn list_objects_root(app: &AppHandle) -> Result<PathBuf, String> {
        Ok(repository_root(app)?.join("objects").join("sha256"))
    }

    pub fn restore(
        app: &AppHandle,
        game: &Game,
        profile: &SaveProfile,
        version: &SaveVersion,
        on_progress: impl Fn(u8, &str),
    ) -> Result<RestoreReceipt, String> {
        if version.game_uid != game.game_uid {
            return Err("保存版本不属于当前游戏".to_string());
        }
        let lock = REPOSITORY_LOCK.get_or_init(|| Mutex::new(()));
        let _guard = lock.lock().map_err(|_| "存档仓库锁定失败".to_string())?;
        let groups = build_restore_groups(game, profile, version)?;
        if groups.is_empty() {
            return Err("保存版本没有可恢复的存档文件".to_string());
        }
        let total_files = groups.iter().map(|group| group.entries.len()).sum::<usize>();
        let mut completed = 0usize;
        let mut undo_groups = Vec::new();
        for group in groups {
            match restore_group(app, &group, |count, message| {
                completed += count;
                on_progress(((completed * 100) / total_files.max(1)) as u8, message);
            }) {
                Ok(undo) => undo_groups.push(undo),
                Err(error) => {
                    let mut rollback_errors = Vec::new();
                    for undo in undo_groups.iter().rev() {
                        if let Err(rollback_error) = rollback_group(&undo.root, &undo.rollback, &undo.installed_paths, &undo.backed_up_paths) {
                            rollback_errors.push(rollback_error);
                        } else {
                            cleanup_restore_artifacts(undo);
                        }
                    }
                    return Err(append_rollback_errors(error, rollback_errors));
                }
            }
        }
        on_progress(100, "存档版本恢复完成");
        Ok(RestoreReceipt { undo_groups })
    }

    pub fn finalize_restore(receipt: RestoreReceipt) {
        for undo in &receipt.undo_groups {
            cleanup_restore_artifacts(undo);
        }
    }

    pub fn rollback_restore(receipt: RestoreReceipt) -> Result<(), String> {
        let mut rollback_errors = Vec::new();
        for undo in receipt.undo_groups.iter().rev() {
            if let Err(error) = rollback_group(&undo.root, &undo.rollback, &undo.installed_paths, &undo.backed_up_paths) {
                rollback_errors.push(error);
            } else {
                cleanup_restore_artifacts(undo);
            }
        }
        if rollback_errors.is_empty() { Ok(()) } else { Err(rollback_errors.join("；")) }
    }

    pub fn collect_garbage(app: &AppHandle, versions: &[SaveVersion]) -> Result<usize, String> {
        let lock = REPOSITORY_LOCK.get_or_init(|| Mutex::new(()));
        let _guard = lock.lock().map_err(|_| "存档仓库锁定失败".to_string())?;
        let root = Self::list_objects_root(app)?;
        if !root.is_dir() {
            return Ok(0);
        }
        let mut referenced = versions
            .iter()
            .flat_map(|version| version.files.iter())
            .filter_map(|file| file.object_hash.as_deref())
            .map(str::to_ascii_lowercase)
            .collect::<HashSet<_>>();
        let pending = PENDING_OBJECTS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .map_err(|_| "存档仓库待提交对象状态损坏".to_string())?;
        referenced.extend(pending.keys().cloned());
        let mut removed = 0usize;
        for prefix in fs::read_dir(&root).map_err(|err| format!("读取存档对象目录失败：{err}"))? {
            let prefix = prefix.map_err(|err| format!("读取存档对象目录失败：{err}"))?.path();
            if !prefix.is_dir() {
                continue;
            }
            for item in fs::read_dir(&prefix).map_err(|err| format!("读取存档对象目录失败：{err}"))? {
                let path = item.map_err(|err| format!("读取存档对象失败：{err}"))?.path();
                if !path.is_file() {
                    continue;
                }
                let name = path.file_name().and_then(|value| value.to_str()).unwrap_or_default().to_ascii_lowercase();
                if name.starts_with('.') || !referenced.contains(&name) {
                    fs::remove_file(&path).map_err(|err| format!("回收孤立存档对象失败：{err}"))?;
                    removed += 1;
                }
            }
            let _ = fs::remove_dir(&prefix);
        }
        Ok(removed)
    }

    pub fn write_object(app: &AppHandle, hash: &str, bytes: &[u8]) -> Result<(), String> {
        let lock = REPOSITORY_LOCK.get_or_init(|| Mutex::new(()));
        let _guard = lock.lock().map_err(|_| "存档仓库锁定失败".to_string())?;
        Self::write_object_locked(app, hash, bytes)
    }

    pub fn read_object(app: &AppHandle, hash: &str) -> Result<Vec<u8>, String> {
        let path = object_path(app, hash)?;
        if !path.is_file() {
            return Err(format!("存档对象不存在：{hash}"));
        }
        let bytes = fs::read(&path).map_err(|err| format!("读取存档对象失败：{err}"))?;
        if sha256_bytes(&bytes) != hash.to_ascii_lowercase() {
            return Err(format!("存档对象完整性校验失败：{hash}"));
        }
        Ok(bytes)
    }

    pub fn object_path(app: &AppHandle, hash: &str) -> Result<PathBuf, String> {
        object_path(app, hash)
    }

    fn write_object_locked(app: &AppHandle, hash: &str, bytes: &[u8]) -> Result<(), String> {
        let root = Self::list_objects_root(app)?;
        let directory = root.join(&hash[..2]);
        let target = directory.join(hash);
        if target.is_file() {
            let metadata = fs::metadata(&target).map_err(|err| format!("读取存档对象失败：{err}"))?;
            if metadata.len() == bytes.len() as u64 && sha256_file(&target)? == hash {
                return Ok(());
            }
            return Err(format!("存档对象完整性校验失败，拒绝覆盖已有对象：{}", target.display()));
        }
        fs::create_dir_all(&directory).map_err(|err| format!("创建存档对象目录失败：{err}"))?;
        let temporary = directory.join(format!(".{hash}.tmp-{}", Uuid::new_v4().simple()));
        let result = (|| -> Result<(), String> {
            let mut file = fs::File::create(&temporary).map_err(|err| format!("创建存档对象临时文件失败：{err}"))?;
            file.write_all(bytes).map_err(|err| format!("写入存档对象失败：{err}"))?;
            file.sync_all().map_err(|err| format!("刷新存档对象失败：{err}"))?;
            fs::rename(&temporary, &target).map_err(|err| format!("提交存档对象失败：{err}"))?;
            Ok(())
        })();
        let _ = fs::remove_file(&temporary);
        result
    }
}

pub fn release_pending_objects(version: &SaveVersion) {
    let Some(pending) = PENDING_OBJECTS.get() else { return };
    if let Ok(mut pending) = pending.lock() {
        for hash in version.files.iter().filter_map(|file| file.object_hash.as_ref()) {
            let hash = hash.to_ascii_lowercase();
            if let Some(count) = pending.get_mut(&hash) {
                if *count > 1 { *count -= 1; } else { pending.remove(&hash); }
            }
        }
    }
}

fn protect_pending_objects(version: &SaveVersion) -> Result<(), String> {
    let pending = PENDING_OBJECTS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut pending = pending.lock().map_err(|_| "存档仓库待提交对象状态损坏".to_string())?;
    for hash in version.files.iter().filter_map(|file| file.object_hash.as_ref()) {
        *pending.entry(hash.to_ascii_lowercase()).or_insert(0) += 1;
    }
    Ok(())
}

struct RestoreGroup {
    root: PathBuf,
    entries: Vec<(String, String)>,
    protected_paths: HashSet<String>,
}

struct RestoreUndo {
    root: PathBuf,
    staging: PathBuf,
    rollback: PathBuf,
    backed_up_paths: HashSet<String>,
    installed_paths: HashSet<String>,
}

pub struct RestoreReceipt {
    undo_groups: Vec<RestoreUndo>,
}

fn build_restore_groups(game: &Game, profile: &SaveProfile, version: &SaveVersion) -> Result<Vec<RestoreGroup>, String> {
    let mut groups = HashMap::<String, RestoreGroup>::new();
    for entry in &version.files {
        let relative = validate_relative(&entry.relative_path)?;
        let scope = find_scope_for_entry(profile, entry, &relative)?;
        let root = scope_root(game, scope);
        if !root.is_dir() {
            return Err(format!("存档范围不可访问：{}", root.display()));
        }
        let key = normalize_path(root.to_string_lossy().as_ref());
        if !groups.contains_key(&key) {
            let mut protected_paths = HashSet::new();
            for candidate in profile.scopes.iter().filter(|candidate| normalize_path(scope_root(game, candidate).to_string_lossy().as_ref()) == key) {
                protected_paths.extend(collect_protected_paths(&root, candidate)?);
            }
            groups.insert(key.clone(), RestoreGroup {
                root: root.clone(),
                entries: Vec::new(),
                protected_paths,
            });
        }
        if !entry.deleted {
            let hash = entry.object_hash.clone().ok_or_else(|| format!("保存版本缺少对象：{}", entry.relative_path))?;
            let group = groups.get_mut(&key).expect("restore group was inserted");
            if group.entries.iter().any(|(existing, existing_hash)| existing == &relative && existing_hash != &hash) {
                return Err(format!("保存版本包含冲突的存档文件：{}", entry.relative_path));
            }
            group.entries.push((relative, hash));
        }
    }
    for group in groups.values_mut() {
        group.entries.sort();
        group.entries.dedup();
    }
    let mut groups = groups.into_values().collect::<Vec<_>>();
    groups.sort_by(|left, right| normalize_path(left.root.to_string_lossy().as_ref()).cmp(&normalize_path(right.root.to_string_lossy().as_ref())));
    Ok(groups)
}

fn restore_group(app: &AppHandle, group: &RestoreGroup, mut on_progress: impl FnMut(usize, &str)) -> Result<RestoreUndo, String> {
    let restore_id = Uuid::new_v4().simple().to_string();
    let staging = group.root.join(format!(".gamesaver-restore-{restore_id}"));
    let rollback = group.root.join(format!(".gamesaver-rollback-{restore_id}"));
    let target_paths = group.entries.iter().map(|(relative, _)| relative.clone()).collect::<HashSet<_>>();
    let touched = group.protected_paths.union(&target_paths).cloned().collect::<HashSet<_>>();
    let mut backed_up_paths = HashSet::new();
    let mut installed_paths = HashSet::new();
    let result = (|| -> Result<(), String> {
        fs::create_dir_all(&staging).map_err(|err| format!("创建存档恢复暂存目录失败：{err}"))?;
        for (index, (relative, hash)) in group.entries.iter().enumerate() {
            let object = object_path(app, hash)?;
            if !object.is_file() || sha256_file(&object)? != hash.to_ascii_lowercase() {
                return Err(format!("存档对象校验失败：{hash}"));
            }
            let staged = safe_join(&staging, relative)?;
            if let Some(parent) = staged.parent() {
                fs::create_dir_all(parent).map_err(|err| format!("创建存档恢复目录失败：{err}"))?;
            }
            fs::copy(&object, &staged).map_err(|err| format!("物化存档对象失败：{err}"))?;
            on_progress(0, &format!("正在校验存档对象 {}/{}", index + 1, group.entries.len()));
        }
        for relative in &target_paths {
            let destination = safe_join(&group.root, relative)?;
            if let Some(parent) = destination.parent() {
                ensure_parent_is_directory(parent)?;
            }
        }
        for relative in &touched {
            let destination = safe_join(&group.root, relative)?;
            if destination.exists() {
                let backup = safe_join(&rollback, relative)?;
                if let Some(parent) = backup.parent() {
                    fs::create_dir_all(parent).map_err(|err| format!("创建存档回滚目录失败：{err}"))?;
                }
                fs::rename(&destination, &backup).map_err(|err| format!("保护当前存档失败：{err}"))?;
                backed_up_paths.insert(relative.clone());
            }
        }
        for (index, (relative, _)) in group.entries.iter().enumerate() {
            let staged = safe_join(&staging, relative)?;
            let destination = safe_join(&group.root, relative)?;
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|err| format!("创建存档目录失败：{err}"))?;
            }
            fs::rename(&staged, &destination).map_err(|err| format!("提交存档文件失败：{err}"))?;
            installed_paths.insert(relative.clone());
            on_progress(1, &format!("正在恢复存档文件 {}/{}", index + 1, group.entries.len()));
        }
        Ok(())
    })();
    if let Err(error) = result {
        let rollback_result = rollback_group(&group.root, &rollback, &installed_paths, &backed_up_paths);
        let _ = fs::remove_dir_all(&staging);
        if rollback_result.is_ok() {
            let _ = fs::remove_dir_all(&rollback);
        }
        return Err(append_rollback_errors(error, rollback_result.err().into_iter().collect()));
    }
    Ok(RestoreUndo { root: group.root.clone(), staging, rollback, backed_up_paths, installed_paths })
}

fn rollback_group(root: &Path, rollback: &Path, installed_paths: &HashSet<String>, backed_up_paths: &HashSet<String>) -> Result<(), String> {
    let mut errors = Vec::new();
    for relative in installed_paths {
        match safe_join(root, relative) {
            Ok(destination) => {
                let result = if destination.is_dir() { fs::remove_dir_all(&destination) } else { fs::remove_file(&destination) };
                if let Err(error) = result {
                    if error.kind() != std::io::ErrorKind::NotFound { errors.push(format!("删除恢复文件 {} 失败：{error}", destination.display())); }
                }
            }
            Err(error) => {
                errors.push(format!("解析恢复文件路径失败：{error}"));
            }
        }
    }
    for relative in backed_up_paths {
        let backup = match safe_join(rollback, relative) {
            Ok(path) => path,
            Err(error) => { errors.push(error); continue; }
        };
        if !backup.exists() { continue; }
        let destination = match safe_join(root, relative) {
            Ok(path) => path,
            Err(error) => { errors.push(error); continue; }
        };
        if let Some(parent) = destination.parent() {
            if let Err(error) = fs::create_dir_all(parent) { errors.push(format!("创建存档回滚目录失败：{error}")); continue; }
        }
        if let Err(error) = fs::rename(&backup, &destination) { errors.push(format!("恢复当前存档 {} 失败：{error}", destination.display())); }
    }
    if errors.is_empty() { Ok(()) } else { Err(errors.join("；")) }
}

fn append_rollback_errors(error: String, rollback_errors: Vec<String>) -> String {
    if rollback_errors.is_empty() { error } else { format!("{error}；自动回滚失败，回滚副本已保留：{}", rollback_errors.join("；")) }
}

fn cleanup_restore_artifacts(undo: &RestoreUndo) {
    let _ = fs::remove_dir_all(&undo.staging);
    let _ = fs::remove_dir_all(&undo.rollback);
}

fn collect_protected_paths(root: &Path, scope: &SaveScope) -> Result<HashSet<String>, String> {
    let mut paths = HashSet::new();
    for relative in &scope.confirmed_files {
        let relative = validate_relative(relative)?;
        let path = safe_join(root, &relative)?;
        if is_protected_file(&path, &relative, scope) {
            paths.insert(relative);
        }
    }
    for relative in &scope.include_directories {
        let directory = validate_relative(relative)?;
        let path = safe_join(root, &directory)?;
        if !path.is_dir() { continue; }
        for entry in WalkDir::new(&path).follow_links(false) {
            let entry = entry.map_err(|err| format!("扫描当前存档失败：{err}"))?;
            if !entry.file_type().is_file() { continue; }
            let relative = normalize_relative(entry.path().strip_prefix(root).map_err(|_| "存档文件超出保护范围".to_string())?.to_string_lossy().as_ref());
            if is_protected_file(entry.path(), &relative, scope) { paths.insert(relative); }
        }
    }
    Ok(paths)
}

fn is_protected_file(path: &Path, relative: &str, scope: &SaveScope) -> bool {
    path.is_file() && !is_excluded(relative, scope) && scope.max_file_bytes.map(|limit| fs::metadata(path).map(|metadata| metadata.len() <= limit).unwrap_or(false)).unwrap_or(true)
}

fn scope_matches_entry(scope: &SaveScope, entry: &SaveFileEntry, relative: &str) -> bool {
    scope.root_type == entry.root_type
        && entry.root_path.as_deref().map(|path| normalize_path(path) == normalize_path(&scope.root_path)).unwrap_or(true)
        && scope_includes_relative(relative, scope)
        && !is_excluded(relative, scope)
}

fn find_scope_for_entry<'a>(profile: &'a SaveProfile, entry: &SaveFileEntry, relative: &str) -> Result<&'a SaveScope, String> {
    let matches = profile.scopes.iter().filter(|scope| scope_matches_entry(scope, entry, relative)).collect::<Vec<_>>();
    match matches.as_slice() {
        [scope] => Ok(scope),
        [] => Err(format!("保存版本中的文件不属于当前存档范围：{}", entry.relative_path)),
        _ => Err(format!("保存版本缺少存档范围路径，无法安全恢复：{}", entry.relative_path)),
    }
}

fn scope_root(game: &Game, scope: &SaveScope) -> PathBuf {
    if matches!(scope.root_type, SaveRootType::ManagedGame) { PathBuf::from(&game.managed_path) } else { PathBuf::from(&scope.root_path) }
}

fn object_path(app: &AppHandle, hash: &str) -> Result<PathBuf, String> {
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) { return Err(format!("存档对象哈希无效：{hash}")); }
    Ok(SaveRepository::list_objects_root(app)?.join(&hash[..2]).join(hash))
}

fn validate_relative(path: &str) -> Result<String, String> {
    let path = Path::new(path);
    if path.is_absolute() || path.components().any(|component| matches!(component, std::path::Component::ParentDir)) { return Err(format!("存档相对路径无效：{}", path.display())); }
    Ok(normalize_relative(path.to_string_lossy().as_ref()))
}

fn safe_join(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let relative = Path::new(relative);
    if relative.is_absolute() || relative.components().any(|component| matches!(component, std::path::Component::ParentDir)) { return Err(format!("存档路径包含无效的上级目录：{relative:?}")); }
    let mut current = root.to_path_buf();
    for component in relative.components() {
        if let std::path::Component::Normal(part) = component {
            current.push(part);
            if fs::symlink_metadata(&current).map(|metadata| metadata.file_type().is_symlink()).unwrap_or(false) {
                return Err(format!("存档路径包含不安全的符号链接：{}", current.display()));
            }
        }
    }
    Ok(root.join(relative))
}

fn ensure_parent_is_directory(path: &Path) -> Result<(), String> {
    let mut current = path;
    while !current.exists() {
        let Some(parent) = current.parent() else { break; };
        current = parent;
    }
    if current.is_file() { return Err(format!("存档目标目录被文件占用：{}", current.display())); }
    Ok(())
}

fn collect_profile_files(profile: &SaveProfile) -> Result<Vec<CollectedFile>, String> {
    let mut files = BTreeMap::new();
    for scope in &profile.scopes {
        let root = PathBuf::from(&scope.root_path)
            .canonicalize()
            .map_err(|err| format!("解析存档范围失败：{err}"))?;
        if !root.is_dir() {
            return Err(format!("存档范围不可访问：{}", root.display()));
        }
        for relative in &scope.confirmed_files {
            add_candidate(&mut files, &root.join(relative), &root, scope)?;
        }
        for relative in &scope.include_directories {
            let directory = root.join(relative);
            if !directory.is_dir() {
                return Err(format!("存档目录不可访问：{}", directory.display()));
            }
            for entry in WalkDir::new(&directory).follow_links(false) {
                let entry = entry.map_err(|err| format!("扫描存档目录失败：{err}"))?;
                if entry.file_type().is_file() {
                    add_candidate(&mut files, entry.path(), &root, scope)?;
                }
            }
        }
    }
    Ok(files.into_values().collect())
}

fn add_candidate(files: &mut BTreeMap<String, CollectedFile>, path: &Path, root: &Path, scope: &SaveScope) -> Result<(), String> {
    if !path.is_file() {
        return Ok(());
    }
    let path = path.canonicalize().map_err(|err| format!("解析存档文件失败：{err}"))?;
    let relative = path.strip_prefix(root).map_err(|_| format!("存档文件超出保护范围：{}", path.display()))?;
    let relative_path = normalize_relative(relative.to_string_lossy().as_ref());
    if is_excluded(&relative_path, scope) {
        return Ok(());
    }
    let metadata = fs::metadata(&path).map_err(|err| format!("读取存档文件信息失败：{err}"))?;
    if scope.max_file_bytes.is_some_and(|limit| metadata.len() > limit) {
        return Ok(());
    }
    files.insert(
        normalize_path(path.to_string_lossy().as_ref()),
        CollectedFile {
            path,
            root_type: scope.root_type,
            root_path: Some(scope.root_path.clone()),
            relative_path,
        },
    );
    Ok(())
}

fn is_excluded(relative_path: &str, scope: &SaveScope) -> bool {
    let normalized = normalize_relative(relative_path);
    let file_name = Path::new(&normalized).file_name().and_then(|value| value.to_str()).unwrap_or_default();
    scope.exclude_exact.iter().any(|value| normalized == normalize_relative(value))
        || scope.exclude_directories.iter().any(|value| normalized.split('/').any(|part| part.eq_ignore_ascii_case(value.trim_matches('/'))))
        || scope.exclude_patterns.iter().any(|pattern| wildcard_matches(file_name, pattern))
}

fn scope_includes_relative(relative_path: &str, scope: &SaveScope) -> bool {
    let relative_path = normalize_relative(relative_path);
    scope.confirmed_files.iter().any(|value| relative_path == normalize_relative(value))
        || scope.include_directories.iter().any(|value| {
            let directory = normalize_relative(value);
            directory == "." || relative_path == directory || relative_path.starts_with(&(directory + "/"))
        })
}

fn entry_belongs_to_profile(entry: &SaveFileEntry, profile: &SaveProfile) -> bool {
    profile.scopes.iter().any(|scope| {
        scope.root_type == entry.root_type
            && entry.root_path.as_deref().map(|path| normalize_path(path) == normalize_path(&scope.root_path)).unwrap_or(true)
            && scope_includes_relative(&entry.relative_path, scope)
            && !is_excluded(&entry.relative_path, scope)
    })
}

fn entry_key(file: &SaveFileEntry) -> String {
    normalize_path(&format!("{:?}:{}:{}", file.root_type, file.root_path.as_deref().unwrap_or_default(), file.relative_path))
}

fn normalize_relative(path: &str) -> String {
    let normalized = path.replace('\\', "/").trim_matches('/').to_ascii_lowercase();
    if normalized == "." || normalized.is_empty() { ".".to_string() } else { normalized.trim_start_matches("./").to_string() }
}

fn wildcard_matches(value: &str, pattern: &str) -> bool {
    let value = value.to_ascii_lowercase();
    let pattern = pattern.to_ascii_lowercase();
    if pattern == "*" {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix("*") {
        return value.ends_with(suffix);
    }
    if let Some(prefix) = pattern.strip_suffix("*") {
        return value.starts_with(prefix);
    }
    value == pattern
}

fn read_stable_file(path: &Path) -> Result<(Vec<u8>, u64), String> {
    let before = fs::metadata(path).map_err(|err| format!("读取存档文件失败：{err}"))?;
    let mut file = fs::File::open(path).map_err(|err| format!("打开存档文件失败：{err}"))?;
    let mut bytes = Vec::with_capacity(before.len().min(8 * 1024 * 1024) as usize);
    file.read_to_end(&mut bytes).map_err(|err| format!("读取存档文件失败：{err}"))?;
    let after = fs::metadata(path).map_err(|err| format!("确认存档文件状态失败：{err}"))?;
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return Err(format!("存档文件在读取期间发生变化：{}", path.display()));
    }
    Ok((bytes, before.len()))
}

fn repository_root(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app.state::<AppState>().saves_root()?.join(".gamesaver-repository"))
}

fn same_entries(left: &[SaveFileEntry], right: &[SaveFileEntry]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            same_entry_location(left, right)
                && left.object_hash == right.object_hash
                && left.size == right.size
                && left.deleted == right.deleted
        })
}

fn same_entry_location(left: &SaveFileEntry, right: &SaveFileEntry) -> bool {
    left.root_type == right.root_type
        && normalize_relative(&left.relative_path) == normalize_relative(&right.relative_path)
        && (left.root_path.is_none()
            || right.root_path.is_none()
            || left.root_path.as_deref().is_some_and(|path| right.root_path.as_deref().is_some_and(|other| normalize_path(path) == normalize_path(other))))
}

fn collected_matches_entry(file: &CollectedFile, entry: &SaveFileEntry) -> bool {
    file.root_type == entry.root_type
        && normalize_relative(&file.relative_path) == normalize_relative(&entry.relative_path)
        && (entry.root_path.is_none()
            || entry.root_path.as_deref().is_some_and(|path| file.root_path.as_deref().is_some_and(|other| normalize_path(path) == normalize_path(other))))
}

fn normalize_path(path: &str) -> String {
    path.replace('/', "\\").trim_end_matches('\\').to_ascii_lowercase()
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    hex::encode(digest.finalize())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|err| format!("读取存档对象失败：{err}"))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|err| format!("读取存档对象失败：{err}"))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn now_iso() -> String {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|value| value.as_secs().to_string()).unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use super::{entry_belongs_to_profile, find_scope_for_entry, is_excluded, same_entries, wildcard_matches};
    use crate::domain::{SaveFileEntry, SaveProfile, SaveRootType, SaveScope, UnknownFilePolicy};

    #[test]
    fn wildcard_patterns_match_common_exclusions() {
        assert!(wildcard_matches("notes.tmp", "*.tmp"));
        assert!(wildcard_matches("cache.bin", "cache*"));
        assert!(!wildcard_matches("save.dat", "*.tmp"));
    }

    #[test]
    fn exclusions_match_files_and_directories() {
        let scope = crate::domain::SaveScope {
            root_type: SaveRootType::Custom,
            root_path: "C:/Game".to_string(),
            confirmed_files: Vec::new(),
            include_directories: vec![".".to_string()],
            exclude_exact: vec!["settings.ini".to_string()],
            exclude_patterns: vec!["*.tmp".to_string()],
            exclude_directories: vec!["cache".to_string()],
            unknown_file_policy: UnknownFilePolicy::Protect,
            max_file_bytes: None,
        };
        assert!(is_excluded("settings.ini", &scope));
        assert!(is_excluded("cache/save.dat", &scope));
        assert!(is_excluded("save.tmp", &scope));
        assert!(!is_excluded("cache_root/save.dat", &scope));
    }

    #[test]
    fn entries_compare_by_path_hash_and_size() {
        let entry = SaveFileEntry { root_type: SaveRootType::Custom, root_path: Some("C:/Game".to_string()), relative_path: "save.dat".to_string(), object_hash: Some("a".to_string()), size: 1, deleted: false };
        assert!(same_entries(std::slice::from_ref(&entry), &[SaveFileEntry { root_type: SaveRootType::Custom, root_path: Some("c:\\game".to_string()), relative_path: "SAVE.DAT".to_string(), object_hash: Some("a".to_string()), size: 1, deleted: false }]));
        assert!(!same_entries(std::slice::from_ref(&entry), &[SaveFileEntry { root_type: SaveRootType::Custom, root_path: Some("c:\\game".to_string()), relative_path: "SAVE.DAT".to_string(), object_hash: Some("b".to_string()), size: 1, deleted: false }]));
        assert!(!same_entries(std::slice::from_ref(&entry), &[SaveFileEntry { root_type: SaveRootType::Custom, root_path: Some("c:\\game".to_string()), relative_path: "SAVE.DAT".to_string(), object_hash: None, size: 0, deleted: true }]));
    }

    #[test]
    fn deleted_entry_is_kept_when_it_still_belongs_to_profile() {
        let profile = SaveProfile {
            profile_id: "profile".to_string(),
            game_uid: "game".to_string(),
            executable_hash: "hash".to_string(),
            scopes: vec![SaveScope {
                root_type: SaveRootType::Custom,
                root_path: "C:/Game".to_string(),
                confirmed_files: vec!["save.dat".to_string()],
                include_directories: Vec::new(),
                exclude_exact: Vec::new(),
                exclude_patterns: Vec::new(),
                exclude_directories: Vec::new(),
                unknown_file_policy: UnknownFilePolicy::Protect,
                max_file_bytes: None,
            }],
            detection_evidence: Vec::new(),
            confidence: 100,
            enabled: true,
            created_at: "0".to_string(),
            updated_at: "0".to_string(),
        };
        let entry = SaveFileEntry { root_type: SaveRootType::Custom, root_path: Some("c:\\game".to_string()), relative_path: "SAVE.DAT".to_string(), object_hash: Some("a".to_string()), size: 1, deleted: false };
        assert!(entry_belongs_to_profile(&entry, &profile));
    }

    #[test]
    fn restore_paths_reject_absolute_and_parent_segments() {
        assert!(super::validate_relative("C:/outside.dat").is_err());
        assert!(super::validate_relative("../outside.dat").is_err());
        assert_eq!(super::validate_relative("slot\\save.dat").unwrap(), "slot/save.dat");
    }

    #[test]
    fn restore_uses_recorded_root_and_rejects_ambiguous_legacy_entries() {
        let scope = |root: &str| SaveScope {
            root_type: SaveRootType::Custom,
            root_path: root.to_string(),
            confirmed_files: vec!["save.dat".to_string()],
            include_directories: Vec::new(),
            exclude_exact: Vec::new(),
            exclude_patterns: Vec::new(),
            exclude_directories: Vec::new(),
            unknown_file_policy: UnknownFilePolicy::Protect,
            max_file_bytes: None,
        };
        let profile = SaveProfile {
            profile_id: "profile".to_string(),
            game_uid: "game".to_string(),
            executable_hash: "hash".to_string(),
            scopes: vec![scope("C:/one"), scope("C:/two")],
            detection_evidence: Vec::new(),
            confidence: 100,
            enabled: true,
            created_at: "0".to_string(),
            updated_at: "0".to_string(),
        };
        let entry = SaveFileEntry {
            root_type: SaveRootType::Custom,
            root_path: Some("C:/two".to_string()),
            relative_path: "save.dat".to_string(),
            object_hash: Some("a".to_string()),
            size: 1,
            deleted: false,
        };
        assert_eq!(find_scope_for_entry(&profile, &entry, "save.dat").unwrap().root_path, "C:/two");
        let legacy_entry = SaveFileEntry { root_path: None, ..entry };
        assert!(find_scope_for_entry(&profile, &legacy_entry, "save.dat").is_err());
    }
}
