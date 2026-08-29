use crate::domain::{Game, SaveFileEntry, SaveProfile, SaveRootType, SaveScope, SaveVersion};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
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
        on_progress: impl Fn(u8, &str),
    ) -> Result<Option<SaveVersion>, String> {
        let files = collect_profile_files(profile)?;
        let mut entries = Vec::with_capacity(files.len());
        for (index, file) in files.iter().enumerate() {
            let (bytes, size) = read_stable_file(&file.path)?;
            let hash = sha256_bytes(&bytes);
            Self::write_object(app, &hash, &bytes)?;
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
        let current_keys = files.iter().map(|file| collected_key(file)).collect::<HashSet<_>>();
        if let Some(latest) = latest {
            for old_file in latest.files.iter().filter(|file| !file.deleted) {
                if !current_keys.contains(&entry_key(old_file)) && entry_belongs_to_profile(old_file, profile) {
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
        on_progress(100, "存档版本已准备完成");
        Ok(Some(version))
    }

    pub fn list_objects_root(app: &AppHandle) -> Result<PathBuf, String> {
        Ok(repository_root(app)?.join("objects").join("sha256"))
    }

    fn write_object(app: &AppHandle, hash: &str, bytes: &[u8]) -> Result<(), String> {
        let lock = REPOSITORY_LOCK.get_or_init(|| Mutex::new(()));
        let _guard = lock.lock().map_err(|_| "存档仓库锁定失败".to_string())?;
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
            root_path: matches!(scope.root_type, SaveRootType::Custom).then(|| scope.root_path.clone()),
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
            && (!matches!(scope.root_type, SaveRootType::Custom)
                || entry.root_path.as_deref().is_some_and(|path| normalize_path(path) == normalize_path(&scope.root_path)))
            && scope_includes_relative(&entry.relative_path, scope)
            && !is_excluded(&entry.relative_path, scope)
    })
}

fn collected_key(file: &CollectedFile) -> String {
    normalize_path(&format!("{:?}:{}:{}", file.root_type, file.root_path.as_deref().unwrap_or_default(), file.relative_path))
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
    Ok(app.path().app_data_dir().map_err(|err| format!("解析 GameSaver 数据目录失败：{err}"))?.join("saves").join(".gamesaver-repository"))
}

fn same_entries(left: &[SaveFileEntry], right: &[SaveFileEntry]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            entry_key(left) == entry_key(right)
                && left.object_hash == right.object_hash
                && left.size == right.size
                && left.deleted == right.deleted
        })
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
    let mut buffer = [0u8; 1024 * 1024];
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
    use super::{entry_belongs_to_profile, is_excluded, same_entries, wildcard_matches};
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
}
