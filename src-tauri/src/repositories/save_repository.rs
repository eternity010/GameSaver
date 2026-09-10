use crate::{
    app_state::AppState,
    domain::{
        compare_created_at, AppStore, Game, SaveFileEntry, SaveProfile, SaveRootType, SaveScope,
        SaveVersion,
    },
};
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
    size: u64,
    mtime_ms: Option<u64>,
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

        // 1. Fast path: check if absolutely nothing changed compared to latest
        if let Some(latest) = latest {
            let old_active_files: Vec<&SaveFileEntry> =
                latest.files.iter().filter(|file| !file.deleted).collect();
            if old_active_files.len() == files.len() {
                let all_unmodified = files.iter().all(|file| {
                    old_active_files
                        .iter()
                        .any(|old_entry| collected_is_unmodified(file, old_entry, app))
                });
                if all_unmodified {
                    return Ok(None);
                }
            }
        }

        let mut entries = Vec::with_capacity(files.len());
        for (index, file) in files.iter().enumerate() {
            let cached_hash = latest.and_then(|latest_ver| {
                latest_ver
                    .files
                    .iter()
                    .find(|entry| collected_is_unmodified(file, entry, app))
                    .and_then(|entry| entry.object_hash.clone())
            });

            let (hash, size) = if let Some(hash) = cached_hash {
                (hash, file.size)
            } else {
                let (bytes, size) = read_stable_file(&file.path)?;
                let hash = sha256_bytes(&bytes);
                Self::write_object_locked(app, &hash, &bytes)?;
                (hash, size)
            };

            entries.push(SaveFileEntry {
                root_type: file.root_type,
                root_path: file.root_path.clone(),
                relative_path: file.relative_path.clone(),
                object_hash: Some(hash),
                size,
                deleted: false,
                mtime_ms: file.mtime_ms,
            });
            on_progress(
                (((index + 1) * 90) / files.len().max(1)) as u8,
                &format!("正在整理存档文件 {}/{}", index + 1, files.len()),
            );
        }
        if let Some(latest) = latest {
            for old_file in latest.files.iter().filter(|file| !file.deleted) {
                if !files
                    .iter()
                    .any(|file| collected_matches_entry(file, old_file))
                    && entry_belongs_to_profile(old_file, profile)
                {
                    entries.push(SaveFileEntry {
                        root_type: old_file.root_type,
                        root_path: old_file.root_path.clone(),
                        relative_path: old_file.relative_path.clone(),
                        object_hash: None,
                        size: 0,
                        deleted: true,
                        mtime_ms: None,
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
        let total_files = groups
            .iter()
            .map(|group| group.entries.len())
            .sum::<usize>();
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
                        if let Err(rollback_error) = rollback_group(
                            &undo.root,
                            &undo.rollback,
                            &undo.installed_paths,
                            &undo.backed_up_paths,
                        ) {
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
            if let Err(error) = rollback_group(
                &undo.root,
                &undo.rollback,
                &undo.installed_paths,
                &undo.backed_up_paths,
            ) {
                rollback_errors.push(error);
            } else {
                cleanup_restore_artifacts(undo);
            }
        }
        if rollback_errors.is_empty() {
            Ok(())
        } else {
            Err(rollback_errors.join("；"))
        }
    }

    /// 启动清扫：收掉上一次存档恢复被强制中断（进程被杀 / 断电）留下的产物目录。
    ///
    /// `restore_group` 把两个工作目录建在**用户的存档根目录内**：`.gamesaver-restore-<id>`
    /// 是目标版本的完整副本，`.gamesaver-rollback-<id>` 是恢复前的原存档。正常路径由
    /// `finalize_restore` / `rollback_restore` 清理；恢复中途被杀则两者都留在用户的存档目录
    /// 里 —— 而 `collect_profile_files` 会把它们当成正常存档收进下一个版本、随后上传云端，
    /// 并且每崩一次多一份（存档管理审查 V7）。
    ///
    /// **处置顺序必须是「先归位、再清理」，不能直接删。** 崩溃若落在「把现有存档搬进
    /// rollback」这一步之后，用户存档目录里那些位置是空的，真身躺在 rollback 里，直接删掉
    /// 等于把用户的存档删了。归位不完整时保留 rollback 目录不删，下次启动继续重试 —— 宁可
    /// 留着垃圾，也不能弄丢用户自己写的文件。
    ///
    /// 逐范围根处理，单个范围出错只记录、不中断其余范围（与
    /// `GameBodyUpdateService::recover_pending_updates` 一致）。
    pub fn recover_interrupted_restores(profiles: &[SaveProfile]) -> Result<(), String> {
        let mut roots = Vec::new();
        let mut seen = HashSet::new();
        for profile in profiles {
            for scope in &profile.scopes {
                let root = strip_verbatim_prefix(Path::new(&scope.root_path));
                // 同一物理目录可能被多个范围指向，去重避免重复清扫。
                if seen.insert(normalize_path(root.to_string_lossy().as_ref())) && root.is_dir() {
                    roots.push(root);
                }
            }
        }
        let mut errors = Vec::new();
        for root in roots {
            if let Err(error) = sweep_restore_artifacts(&root) {
                errors.push(error);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("；"))
        }
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
        for prefix in fs::read_dir(&root).map_err(|err| format!("读取存档对象目录失败：{err}"))?
        {
            let prefix = prefix
                .map_err(|err| format!("读取存档对象目录失败：{err}"))?
                .path();
            if !prefix.is_dir() {
                continue;
            }
            for item in
                fs::read_dir(&prefix).map_err(|err| format!("读取存档对象目录失败：{err}"))?
            {
                let path = item
                    .map_err(|err| format!("读取存档对象失败：{err}"))?
                    .path();
                if !path.is_file() {
                    continue;
                }
                let name = path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if name.starts_with('.') || !referenced.contains(&name) {
                    fs::remove_file(&path).map_err(|err| format!("回收孤立存档对象失败：{err}"))?;
                    removed += 1;
                }
            }
            let _ = fs::remove_dir(&prefix);
        }
        Ok(removed)
    }

    /// 按 `keep_versions` 剪掉某游戏最旧的存档版本，只改内存中的 `store`，**不碰磁盘**。
    ///
    /// 顺带把 `latest_save_version_id` 指针修好（`AppStore::repair_latest_save_version_id`）：
    /// 剪枝可能删掉指针指向的那一版，留下悬垂指针，而悬垂指针会让 `commit` 丢掉比对基线，
    /// 此后每次游戏退出都无条件新建版本（存档管理审查 V5）。
    ///
    /// 修复**必须紧跟在保留动作之后**，且**无论是否真的剪掉了版本都要做**：指针悬垂的典型
    /// 来源恰恰是「保留了旧版本、指针有意指向它」+「这一轮把它剪掉」（见
    /// `repair_latest_save_version_id` 的说明），只在开头修等于没修；反过来，不剪任何版本时
    /// 也修一次，是为了让旧版本数据里已经存在的悬垂指针能借下一次剪枝自愈。把这件事绑在唯一
    /// 的剪枝入口上，调用方就不必记得各自修一次。
    ///
    /// 返回 `Some(剪枝后的完整版本清单)` 表示确实删掉了版本 —— 调用方应当把它交给
    /// [`Self::collect_garbage`] 回收不再被引用的对象文件；返回 `None` 表示无版本被删除、
    /// 无需回收（也避免了每次调用都做一次全盘扫描）。
    ///
    /// **顺序契约（不可调换）**：回收必须发生在 `GameRepository::persist` **成功之后**。
    /// `collect_garbage` 会真删磁盘上的对象文件，而 `persist` 失败时磁盘上的版本清单仍是
    /// 旧的。若先回收再持久化，一旦 `persist` 失败，磁盘清单会继续列出那些对象已被删除的
    /// 版本 —— 它们从此「在列表里看得见、点恢复却永远失败」。所以这里刻意只做内存剪枝，
    /// 把「回收」这一步留给调用方，且只能在落盘成功之后执行（见存档管理审查 V1）。
    pub(crate) fn prune_game_save_versions(
        store: &mut AppStore,
        game_uid: &str,
        keep_versions: usize,
    ) -> Option<Vec<SaveVersion>> {
        let to_remove: HashSet<String> = if keep_versions == 0 {
            HashSet::new()
        } else {
            let mut game_versions: Vec<_> = store
                .save_versions
                .iter()
                .filter(|version| version.game_uid == game_uid)
                .cloned()
                .collect();
            game_versions.sort_by(|left, right| {
                compare_created_at(&right.created_at, &left.created_at)
                    .then(right.version_id.cmp(&left.version_id))
            });
            game_versions
                .into_iter()
                .skip(keep_versions)
                .map(|version| version.version_id)
                .collect()
        };
        if !to_remove.is_empty() {
            store.save_versions.retain(|version| {
                !(version.game_uid == game_uid && to_remove.contains(&version.version_id))
            });
        }
        // 修复必须紧跟在 retain 之后（理由见函数文档），且在未剪枝时也要执行。
        store.repair_latest_save_version_id(game_uid);
        if to_remove.is_empty() {
            None
        } else {
            Some(store.save_versions.clone())
        }
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

    pub fn object_exists(app: &AppHandle, hash: &str) -> bool {
        match Self::object_path(app, hash) {
            Ok(path) => path.is_file(),
            Err(_) => false,
        }
    }

    fn write_object_locked(app: &AppHandle, hash: &str, bytes: &[u8]) -> Result<(), String> {
        let root = Self::list_objects_root(app)?;
        let directory = root.join(&hash[..2]);
        let target = directory.join(hash);
        if target.is_file() {
            let metadata =
                fs::metadata(&target).map_err(|err| format!("读取存档对象失败：{err}"))?;
            if metadata.len() == bytes.len() as u64 {
                return Ok(());
            }
            return Err(format!(
                "存档对象完整性校验失败，拒绝覆盖已有对象：{}",
                target.display()
            ));
        }
        fs::create_dir_all(&directory).map_err(|err| format!("创建存档对象目录失败：{err}"))?;
        let temporary = directory.join(format!(".{hash}.tmp-{}", Uuid::new_v4().simple()));
        let result = (|| -> Result<(), String> {
            let mut file = fs::File::create(&temporary)
                .map_err(|err| format!("创建存档对象临时文件失败：{err}"))?;
            file.write_all(bytes)
                .map_err(|err| format!("写入存档对象失败：{err}"))?;
            file.sync_all()
                .map_err(|err| format!("刷新存档对象失败：{err}"))?;
            fs::rename(&temporary, &target).map_err(|err| format!("提交存档对象失败：{err}"))?;
            Ok(())
        })();
        let _ = fs::remove_file(&temporary);
        result
    }
}

pub fn release_pending_objects(version: &SaveVersion) {
    let Some(pending) = PENDING_OBJECTS.get() else {
        return;
    };
    if let Ok(mut pending) = pending.lock() {
        for hash in version
            .files
            .iter()
            .filter_map(|file| file.object_hash.as_ref())
        {
            let hash = hash.to_ascii_lowercase();
            if let Some(count) = pending.get_mut(&hash) {
                if *count > 1 {
                    *count -= 1;
                } else {
                    pending.remove(&hash);
                }
            }
        }
    }
}

fn protect_pending_objects(version: &SaveVersion) -> Result<(), String> {
    let pending = PENDING_OBJECTS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut pending = pending
        .lock()
        .map_err(|_| "存档仓库待提交对象状态损坏".to_string())?;
    for hash in version
        .files
        .iter()
        .filter_map(|file| file.object_hash.as_ref())
    {
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

fn build_restore_groups(
    game: &Game,
    profile: &SaveProfile,
    version: &SaveVersion,
) -> Result<Vec<RestoreGroup>, String> {
    let mut groups = HashMap::<String, RestoreGroup>::new();
    for entry in &version.files {
        // 修复前的版本可能已经把恢复产物收了进去（那时没有任何地方拦得住它）。这类条目在
        // 这里直接跳过：既不回写垃圾、也不因为「条目落在哪个范围」而去报错。跳过而不是报错
        // 是刻意的 —— 一个内部工作目录不该让整个版本变成不可恢复（存档管理审查 V7）。
        // 历史数据不做迁移：无法判断当初那些文件是不是真残留，让它随剪枝自然淘汰。
        if path_is_restore_artifact(&entry.relative_path) {
            continue;
        }
        // 墓碑条目（`deleted: true`）只表达「这个文件在该版本里不存在」，它自身没有任何字节
        // 需要写回磁盘，因此**不能**对它做 root/scope 前置校验：那些校验是为「要写到哪里」
        // 把关的，而墓碑引用的目录或范围很可能早已被删除、移出存档范围。照常校验的话，一个与
        // 待恢复内容毫不相干的失效目录就能让整场恢复失败，且报错把用户指向一个他无从修复的
        // 位置（存档管理审查 V2）。改为：能安全定位到现存目录就登记该范围（好让该范围里多余
        // 的受保护文件在恢复时被清掉，覆盖「游戏把存档目录整个清空、只剩墓碑」这类版本），
        // 定位不到就安静跳过。
        if entry.deleted {
            if let Some(root) = deleted_entry_restore_root(game, profile, entry) {
                register_restore_group(game, profile, &mut groups, &root)?;
            }
            continue;
        }
        let relative = validate_relative(&entry.relative_path)?;
        let scope = find_scope_for_entry(profile, entry, &relative)?;
        let root = scope_root(game, scope);
        if root.exists() && !root.is_dir() {
            return Err(format!("存档范围不可访问：{}", root.display()));
        }
        if !root.exists() && !can_create_missing_restore_root(scope.root_type) {
            return Err(format!(
                "存档范围不存在，请先创建或重新选择：{}",
                root.display()
            ));
        }
        let key = register_restore_group(game, profile, &mut groups, &root)?;
        let hash = entry
            .object_hash
            .clone()
            .ok_or_else(|| format!("保存版本缺少对象：{}", entry.relative_path))?;
        let group = groups.get_mut(&key).expect("restore group was inserted");
        if group
            .entries
            .iter()
            .any(|(existing, existing_hash)| existing == &relative && existing_hash != &hash)
        {
            return Err(format!(
                "保存版本包含冲突的存档文件：{}",
                entry.relative_path
            ));
        }
        group.entries.push((relative, hash));
    }
    for group in groups.values_mut() {
        group.entries.sort();
        group.entries.dedup();
    }
    let mut groups = groups.into_values().collect::<Vec<_>>();
    groups.sort_by(|left, right| {
        normalize_path(left.root.to_string_lossy().as_ref())
            .cmp(&normalize_path(right.root.to_string_lossy().as_ref()))
    });
    Ok(groups)
}

/// 登记（必要时创建）某个存档根目录对应的恢复分组，返回该分组的键。
///
/// 分组的 `protected_paths` 来自**所有**根目录落在同一物理路径上的范围，因此与「哪个条目
/// 触发登记」无关 —— 这也是墓碑条目不必做 scope 校验的原因之一。
fn register_restore_group(
    game: &Game,
    profile: &SaveProfile,
    groups: &mut HashMap<String, RestoreGroup>,
    root: &Path,
) -> Result<String, String> {
    let key = normalize_path(root.to_string_lossy().as_ref());
    if !groups.contains_key(&key) {
        let mut protected_paths = HashSet::new();
        for candidate in profile.scopes.iter().filter(|candidate| {
            normalize_path(scope_root(game, candidate).to_string_lossy().as_ref()) == key
        }) {
            protected_paths.extend(collect_protected_paths(root, candidate)?);
        }
        groups.insert(
            key.clone(),
            RestoreGroup {
                root: root.to_path_buf(),
                entries: Vec::new(),
                protected_paths,
            },
        );
    }
    Ok(key)
}

/// 为一个墓碑条目（`deleted: true`）定位它所属的恢复根目录。
///
/// 墓碑对恢复的唯一可能贡献，是让「曾经存有它的那个范围」也参与本轮恢复 —— 这样该范围里
/// 当前多余的受保护文件才会被清掉。因此这里的取态是**保守**的：
///
/// - 只在「条目记录的根路径与某个范围精确吻合」时才认领；老条目没记录根路径时，仅当同类型
///   范围唯一才认领。删除/清理是有破坏性的动作，**宁可漏清也绝不猜** —— 这与读取侧
///   [`find_scope_for_read`] 的宽松取向有意相反（读旧文件无害，删错目录是数据损失）。
/// - 目标路径不存在或不是目录时直接放弃：没有目录就没有需要清理的内容。
/// - 任何不确定都返回 `None`（调用方跳过该条目），**绝不报错**。
fn deleted_entry_restore_root(
    game: &Game,
    profile: &SaveProfile,
    entry: &SaveFileEntry,
) -> Option<PathBuf> {
    let mut candidates: Vec<&SaveScope> = profile
        .scopes
        .iter()
        .filter(|scope| scope.root_type == entry.root_type)
        .collect();
    match entry.root_path.as_deref() {
        Some(recorded) => {
            candidates.retain(|scope| normalize_path(recorded) == normalize_path(&scope.root_path))
        }
        // 老条目没记录根路径：同类型范围唯一才敢认领，否则无从判断该清哪里。
        None if candidates.len() != 1 => return None,
        None => {}
    }
    let scope = pick_scope_by_root_path(&candidates, entry)?;
    let root = scope_root(game, scope);
    root.is_dir().then_some(root)
}

fn restore_group(
    app: &AppHandle,
    group: &RestoreGroup,
    mut on_progress: impl FnMut(usize, &str),
) -> Result<RestoreUndo, String> {
    let restore_id = Uuid::new_v4().simple().to_string();
    let staging = group.root.join(format!(".gamesaver-restore-{restore_id}"));
    let rollback = group.root.join(format!(".gamesaver-rollback-{restore_id}"));
    let target_paths = group
        .entries
        .iter()
        .map(|(relative, _)| relative.clone())
        .collect::<HashSet<_>>();
    let touched = group
        .protected_paths
        .union(&target_paths)
        .cloned()
        .collect::<HashSet<_>>();
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
            on_progress(
                0,
                &format!("正在校验存档对象 {}/{}", index + 1, group.entries.len()),
            );
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
                    fs::create_dir_all(parent)
                        .map_err(|err| format!("创建存档回滚目录失败：{err}"))?;
                }
                fs::rename(&destination, &backup)
                    .map_err(|err| format!("保护当前存档失败：{err}"))?;
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
            on_progress(
                1,
                &format!("正在恢复存档文件 {}/{}", index + 1, group.entries.len()),
            );
        }
        Ok(())
    })();
    if let Err(error) = result {
        let rollback_result =
            rollback_group(&group.root, &rollback, &installed_paths, &backed_up_paths);
        let _ = fs::remove_dir_all(&staging);
        if rollback_result.is_ok() {
            let _ = fs::remove_dir_all(&rollback);
        }
        return Err(append_rollback_errors(
            error,
            rollback_result.err().into_iter().collect(),
        ));
    }
    Ok(RestoreUndo {
        root: group.root.clone(),
        staging,
        rollback,
        backed_up_paths,
        installed_paths,
    })
}

fn rollback_group(
    root: &Path,
    rollback: &Path,
    installed_paths: &HashSet<String>,
    backed_up_paths: &HashSet<String>,
) -> Result<(), String> {
    let mut errors = Vec::new();
    for relative in installed_paths {
        match safe_join(root, relative) {
            Ok(destination) => {
                let result = if destination.is_dir() {
                    fs::remove_dir_all(&destination)
                } else {
                    fs::remove_file(&destination)
                };
                if let Err(error) = result {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        errors.push(format!(
                            "删除恢复文件 {} 失败：{error}",
                            destination.display()
                        ));
                    }
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
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        if !backup.exists() {
            continue;
        }
        let destination = match safe_join(root, relative) {
            Ok(path) => path,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        if let Some(parent) = destination.parent() {
            if let Err(error) = fs::create_dir_all(parent) {
                errors.push(format!("创建存档回滚目录失败：{error}"));
                continue;
            }
        }
        if let Err(error) = fs::rename(&backup, &destination) {
            errors.push(format!(
                "恢复当前存档 {} 失败：{error}",
                destination.display()
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("；"))
    }
}

fn append_rollback_errors(error: String, rollback_errors: Vec<String>) -> String {
    if rollback_errors.is_empty() {
        error
    } else {
        format!(
            "{error}；自动回滚失败，回滚副本已保留：{}",
            rollback_errors.join("；")
        )
    }
}

fn cleanup_restore_artifacts(undo: &RestoreUndo) {
    let _ = fs::remove_dir_all(&undo.staging);
    let _ = fs::remove_dir_all(&undo.rollback);
}

/// 清扫单个存档根目录下的恢复产物：先把 rollback 里的原存档归位，再删产物目录。
///
/// 只扫**顶层**（不递归）—— 产物就建在范围根下，递归进去反而容易碰到用户自己的同名目录。
/// 逐项验证后才动手：名字必须严格匹配 `.gamesaver-{restore,rollback}-<32位十六进制>`，
/// 且必须是**真目录**（符号链接 / 目录联接一律跳过，见 `is_reparse_point`）。
fn sweep_restore_artifacts(root: &Path) -> Result<(), String> {
    // 用 `BTreeMap` 而不是 `HashMap`：万一同一个范围里躺着两轮崩溃留下的产物，处理顺序
    // 至少是可复现的，不会每次启动换个结果。
    let mut staging = BTreeMap::new();
    let mut rollback = BTreeMap::new();
    for entry in fs::read_dir(root).map_err(|err| format!("读取存档目录失败：{err}"))? {
        let path = entry
            .map_err(|err| format!("读取存档目录失败：{err}"))?
            .path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let (bucket, suffix) = if let Some(suffix) = name.strip_prefix(RESTORE_STAGING_PREFIX) {
            (&mut staging, suffix)
        } else if let Some(suffix) = name.strip_prefix(RESTORE_ROLLBACK_PREFIX) {
            (&mut rollback, suffix)
        } else {
            continue;
        };
        if !is_restore_artifact_id(suffix) {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
            continue;
        }
        bucket.insert(suffix.to_string(), path);
    }

    let mut errors = Vec::new();
    for (id, rollback_dir) in &rollback {
        // 同 id 的 staging 只是目标版本副本，不含用户数据，无论归位成功与否都可以清掉。
        let staged = staging.remove(id);
        match restore_rollback_contents(root, rollback_dir) {
            Ok(()) => {
                remove_artifact_dir(rollback_dir, "存档回滚副本", &mut errors);
                if let Some(staged) = staged {
                    remove_artifact_dir(&staged, "存档恢复暂存目录", &mut errors);
                }
            }
            Err(error) => {
                errors.push(error);
                if let Some(staged) = staged {
                    remove_artifact_dir(&staged, "存档恢复暂存目录", &mut errors);
                }
            }
        }
    }
    // 剩下的 staging 都没有配对的 rollback，说明崩溃发生在「还没动用户文件」之前 ——
    // 这类残留可以直接删。
    for staged in staging.values() {
        remove_artifact_dir(staged, "存档恢复暂存目录", &mut errors);
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("；"))
    }
}

fn remove_artifact_dir(path: &Path, label: &str, errors: &mut Vec<String>) {
    match fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => errors.push(format!("清理{label}失败：{}：{error}", path.display())),
    }
}

/// 把回滚目录里的原存档按相对路径搬回存档根目录。
///
/// **全部成功才返回 `Ok`** —— 只要有一个文件没归位，调用方就不该删回滚目录，否则那个文件
/// 就永远找不回来了。
fn restore_rollback_contents(root: &Path, rollback: &Path) -> Result<(), String> {
    let mut errors = Vec::new();
    let mut moves: Vec<(PathBuf, PathBuf)> = Vec::new();
    for entry in WalkDir::new(rollback).follow_links(false) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                errors.push(format!("扫描存档回滚副本失败：{error}"));
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(rollback) else {
            errors.push(format!("存档回滚副本路径异常：{}", entry.path().display()));
            continue;
        };
        let relative = match validate_relative(relative.to_string_lossy().as_ref()) {
            Ok(relative) => relative,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        let destination = match safe_join(root, &relative) {
            Ok(destination) => destination,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        // 本次恢复可能已经把同名文件写到这个位置了，文件直接覆盖即可（std 的 rename 在
        // Windows 上带 MOVEFILE_REPLACE_EXISTING）。若是目录则不擅自删除：报错并把回滚副本
        // 留着，用户还能自己把文件捞回去。
        if destination.is_dir() {
            errors.push(format!("存档回滚目标被目录占用：{}", destination.display()));
            continue;
        }
        moves.push((entry.path().to_path_buf(), destination));
    }
    // 全部列完才开始搬：`WalkDir` 在 Windows 上走的是 `FindNextFile` 语义，边遍历边把条目
    // rename 走，可能让枚举漏掉后面的文件 —— 而漏掉的文件会连同回滚副本一起被删掉。
    for (source, destination) in moves {
        if let Some(parent) = destination.parent() {
            if let Err(error) = ensure_parent_is_directory(parent) {
                errors.push(error);
                continue;
            }
            if let Err(error) = fs::create_dir_all(parent) {
                errors.push(format!("创建存档目录失败：{error}"));
                continue;
            }
        }
        if let Err(error) = fs::rename(&source, &destination) {
            errors.push(format!(
                "恢复原存档 {} 失败：{error}",
                destination.display()
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("；"))
    }
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
        if !path.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&path).follow_links(false) {
            let entry = entry.map_err(|err| format!("扫描当前存档失败：{err}"))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let relative = normalize_relative(
                entry
                    .path()
                    .strip_prefix(root)
                    .map_err(|_| "存档文件超出保护范围".to_string())?
                    .to_string_lossy()
                    .as_ref(),
            );
            if is_protected_file(entry.path(), &relative, scope) {
                paths.insert(relative);
            }
        }
    }
    Ok(paths)
}

fn is_protected_file(path: &Path, relative: &str, scope: &SaveScope) -> bool {
    path.is_file()
        && !is_excluded(relative, scope)
        // 恢复产物不算受保护文件。它的职责是「恢复前把这些文件搬进 rollback 目录」，
        // 而把上一次的残留搬走只会让垃圾换个位置呆着，清不出存档目录（存档管理审查 V7）。
        && !path_is_restore_artifact(relative)
        && scope
            .max_file_bytes
            .map(|limit| {
                fs::metadata(path)
                    .map(|metadata| metadata.len() <= limit)
                    .unwrap_or(false)
            })
            .unwrap_or(true)
}

fn scope_matches_entry_exact(scope: &SaveScope, entry: &SaveFileEntry, relative: &str) -> bool {
    scope.root_type == entry.root_type
        && scope_includes_relative(relative, scope)
        && !is_excluded(relative, scope)
        && entry
            .root_path
            .as_deref()
            .map(|path| normalize_path(path) == normalize_path(&scope.root_path))
            .unwrap_or(false)
}

fn scope_matches_entry_loose(scope: &SaveScope, entry: &SaveFileEntry, relative: &str) -> bool {
    scope.root_type == entry.root_type
        && scope_includes_relative(relative, scope)
        && !is_excluded(relative, scope)
}

fn trailing_path_components_match(a: &str, b: &str) -> bool {
    let norm_a = normalize_path(a);
    let norm_b = normalize_path(b);
    let parts_a: Vec<&str> = norm_a.split('\\').filter(|p| !p.is_empty()).collect();
    let parts_b: Vec<&str> = norm_b.split('\\').filter(|p| !p.is_empty()).collect();
    if let (Some(last_a), Some(last_b)) = (parts_a.last(), parts_b.last()) {
        if last_a == last_b {
            return true;
        }
    }
    false
}

/// 在一组同类型候选范围中，按条目记录的 `root_path` 选出唯一匹配者。
///
/// 先按物理路径精确匹配（同机、路径未改动），再按路径尾段匹配（跨设备或存档目录被改动
/// 过）。候选只有一个时直接返回；无法唯一确定时返回 `None` —— 调用方据此报错，绝不
/// 「随便挑一个」。
fn pick_scope_by_root_path<'a>(
    candidates: &[&'a SaveScope],
    entry: &SaveFileEntry,
) -> Option<&'a SaveScope> {
    if candidates.is_empty() {
        return None;
    }
    if candidates.len() == 1 {
        return Some(candidates[0]);
    }

    let entry_root = entry.root_path.as_deref()?;
    let exact: Vec<&'a SaveScope> = candidates
        .iter()
        .copied()
        .filter(|scope| normalize_path(entry_root) == normalize_path(&scope.root_path))
        .collect();
    if exact.len() == 1 {
        return Some(exact[0]);
    }

    let trailing: Vec<&'a SaveScope> = candidates
        .iter()
        .copied()
        .filter(|scope| trailing_path_components_match(entry_root, &scope.root_path))
        .collect();
    if trailing.len() == 1 {
        return Some(trailing[0]);
    }
    None
}

fn find_scope_for_entry<'a>(
    profile: &'a SaveProfile,
    entry: &SaveFileEntry,
    relative: &str,
) -> Result<&'a SaveScope, String> {
    // 1. Tier 1: Exact physical path match (local machine, unmodified path)
    let exact_matches: Vec<&'a SaveScope> = profile
        .scopes
        .iter()
        .filter(|scope| scope_matches_entry_exact(scope, entry, relative))
        .collect();
    if exact_matches.len() == 1 {
        return Ok(exact_matches[0]);
    }

    // 2. Tier 2: Loose match by root_type + relative path (cross-device restore or modified save directory)
    let loose_matches: Vec<&'a SaveScope> = profile
        .scopes
        .iter()
        .filter(|scope| scope_matches_entry_loose(scope, entry, relative))
        .collect();

    match loose_matches.as_slice() {
        [scope] => Ok(scope),
        [] => Err(format!(
            "保存版本中的文件不属于当前存档范围：{}",
            entry.relative_path
        )),
        _ => pick_scope_by_root_path(&loose_matches, entry).ok_or_else(|| {
            format!(
                "保存版本缺少存档范围路径，无法安全恢复：{}",
                entry.relative_path
            )
        }),
    }
}

/// 为「读取条目对应的磁盘文件」挑选存档范围 —— 打包上传路径使用。
///
/// 与恢复侧的 [`find_scope_for_entry`] 有意不同：这里**不**要求条目仍落在当前启用、
/// 未被排除的范围内。用户收窄存档范围或改动存档目录之后，旧版本里那些落在范围外的文件
/// 仍应能被读取并上传（它们本来就来自这台机器）。因此只按 `root_type` 取候选，再用
/// `root_path` 消歧；候选唯一时直接返回，存在多个同类型范围且无法唯一确定时**报错而不是
/// 猜一个** —— 猜错会把不相干的文件静默打进备份。
pub(crate) fn find_scope_for_read<'a>(
    profile: &'a SaveProfile,
    entry: &SaveFileEntry,
) -> Result<&'a SaveScope, String> {
    let candidates: Vec<&SaveScope> = profile
        .scopes
        .iter()
        .filter(|scope| scope.root_type == entry.root_type)
        .collect();
    if candidates.is_empty() {
        return Err(format!("未找到匹配的作用域：{:?}", entry.root_type));
    }
    pick_scope_by_root_path(&candidates, entry).ok_or_else(|| {
        format!(
            "存在多个同类型存档范围，无法确定该文件的位置：{}",
            entry.relative_path
        )
    })
}

pub(crate) fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{}", rest))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

pub(crate) fn scope_root(game: &Game, scope: &SaveScope) -> PathBuf {
    if matches!(scope.root_type, SaveRootType::ManagedGame) {
        let managed = strip_verbatim_prefix(Path::new(&game.managed_path));
        let scope_path = strip_verbatim_prefix(Path::new(&scope.root_path));
        if scope.root_path.trim().is_empty() || scope_path == managed {
            managed
        } else if scope_path.starts_with(&managed) {
            scope_path
        } else {
            let game_folder = managed
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let scope_str = scope_path.to_string_lossy().replace('/', "\\");
            let marker = format!("\\{}\\", game_folder);
            if let Some(pos) = scope_str.rfind(&marker) {
                let sub = &scope_str[pos + marker.len()..];
                if !sub.is_empty() {
                    return managed.join(sub);
                }
            }
            if scope_path.is_dir() {
                scope_path
            } else {
                managed
            }
        }
    } else {
        strip_verbatim_prefix(Path::new(&scope.root_path))
    }
}

fn can_create_missing_restore_root(root_type: SaveRootType) -> bool {
    matches!(
        root_type,
        SaveRootType::AppData
            | SaveRootType::LocalAppData
            | SaveRootType::LocalLow
            | SaveRootType::Documents
            | SaveRootType::SavedGames
            | SaveRootType::UserProfile
    )
}

fn object_path(app: &AppHandle, hash: &str) -> Result<PathBuf, String> {
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("存档对象哈希无效：{hash}"));
    }
    Ok(SaveRepository::list_objects_root(app)?
        .join(&hash[..2])
        .join(hash))
}

fn validate_relative(path: &str) -> Result<String, String> {
    let path = Path::new(path);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(format!("存档相对路径无效：{}", path.display()));
    }
    Ok(normalize_relative(path.to_string_lossy().as_ref()))
}

fn safe_join(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let relative = Path::new(relative);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(format!("存档路径包含无效的上级目录：{relative:?}"));
    }
    let mut current = root.to_path_buf();
    for component in relative.components() {
        if let std::path::Component::Normal(part) = component {
            current.push(part);
            if fs::symlink_metadata(&current)
                .map(|metadata| metadata.file_type().is_symlink())
                .unwrap_or(false)
            {
                return Err(format!(
                    "存档路径包含不安全的符号链接：{}",
                    current.display()
                ));
            }
        }
    }
    Ok(root.join(relative))
}

fn ensure_parent_is_directory(path: &Path) -> Result<(), String> {
    let mut current = path;
    while !current.exists() {
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent;
    }
    if current.is_file() {
        return Err(format!("存档目标目录被文件占用：{}", current.display()));
    }
    Ok(())
}

fn collect_profile_files(profile: &SaveProfile) -> Result<Vec<CollectedFile>, String> {
    let mut files = BTreeMap::new();
    for scope in &profile.scopes {
        let path = strip_verbatim_prefix(Path::new(&scope.root_path));
        if !path.exists() {
            continue;
        }
        let root = strip_verbatim_prefix(
            &path
                .canonicalize()
                .map_err(|err| format!("解析存档范围失败：{err}"))?,
        );
        if !root.is_dir() {
            continue;
        }
        for relative in &scope.confirmed_files {
            let candidate = root.join(relative);
            if candidate.exists() {
                add_candidate(&mut files, &candidate, &root, scope)?;
            }
        }
        for relative in &scope.include_directories {
            let directory = root.join(relative);
            if !directory.is_dir() {
                continue;
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

fn add_candidate(
    files: &mut BTreeMap<String, CollectedFile>,
    path: &Path,
    root: &Path,
    scope: &SaveScope,
) -> Result<(), String> {
    if !path.is_file() {
        return Ok(());
    }
    let path = strip_verbatim_prefix(
        &path
            .canonicalize()
            .map_err(|err| format!("解析存档文件失败：{err}"))?,
    );
    let relative = path
        .strip_prefix(root)
        .map_err(|_| format!("存档文件超出保护范围：{}", path.display()))?;
    let relative_path = normalize_relative(relative.to_string_lossy().as_ref());
    if is_excluded(&relative_path, scope) {
        return Ok(());
    }
    // 恢复产物绝不进版本。它是 `restore_group` 的工作目录，正常情况下早已被清掉；能走到
    // 这里说明上一次恢复被强杀、残留还留在存档目录里。收进版本之后它还会一路被上传云端，
    // 将来恢复那一版时再把垃圾铺回存档目录，且每崩一次多一份（存档管理审查 V7）。
    //
    // 这道判定刻意不塞进 `is_excluded`：`is_excluded` 还被
    // `scope_matches_entry_exact` / `scope_matches_entry_loose` 用来判「条目属于哪个
    // 范围」，挂上去会让**历史上已被污染**的版本在恢复时找不到范围而整场失败 —— 那是
    // V2 那个失败模式，只是换了个诱因。
    if path_is_restore_artifact(&relative_path) {
        return Ok(());
    }
    let metadata = fs::metadata(&path).map_err(|err| format!("读取存档文件信息失败：{err}"))?;
    if scope
        .max_file_bytes
        .is_some_and(|limit| metadata.len() > limit)
    {
        return Ok(());
    }
    let size = metadata.len();
    let mtime_ms = mtime_millis(&metadata);
    files.insert(
        normalize_path(path.to_string_lossy().as_ref()),
        CollectedFile {
            path,
            root_type: scope.root_type,
            root_path: Some(scope.root_path.clone()),
            relative_path,
            size,
            mtime_ms,
        },
    );
    Ok(())
}

fn mtime_millis(metadata: &fs::Metadata) -> Option<u64> {
    metadata.modified().ok().and_then(|time| {
        time.duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_millis() as u64)
    })
}

fn is_excluded(relative_path: &str, scope: &SaveScope) -> bool {
    let normalized = normalize_relative(relative_path);
    let file_name = Path::new(&normalized)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    scope
        .exclude_exact
        .iter()
        .any(|value| normalized == normalize_relative(value))
        || scope.exclude_directories.iter().any(|value| {
            normalized
                .split('/')
                .any(|part| part.eq_ignore_ascii_case(value.trim_matches('/')))
        })
        || scope
            .exclude_patterns
            .iter()
            .any(|pattern| wildcard_matches(file_name, pattern))
}

/// `restore_group` 在用户存档根目录里建的两个工作目录（存档管理审查 V7）。
const RESTORE_STAGING_PREFIX: &str = ".gamesaver-restore-";
const RESTORE_ROLLBACK_PREFIX: &str = ".gamesaver-rollback-";

/// 名字后缀是不是 `restore_group` 拼出来的那个 id（`Uuid::simple()`，32 位十六进制）。
///
/// 要求「非空且全为十六进制」而不是「非空即可」：用户完全可能真有一个叫
/// `.gamesaver-restore-notes.txt` 的文件，只认前缀会把它一起当产物排除 —— 等于替用户丢掉
/// 一个存档。严格一点的代价只是极少数畸形残留要靠启动清扫之外的路径处理。
fn is_restore_artifact_id(suffix: &str) -> bool {
    !suffix.is_empty() && suffix.chars().all(|value| value.is_ascii_hexdigit())
}

/// 单个路径分量是否为恢复产物目录名。
fn is_restore_artifact_name(name: &str) -> bool {
    [RESTORE_STAGING_PREFIX, RESTORE_ROLLBACK_PREFIX]
        .iter()
        .filter_map(|prefix| name.strip_prefix(prefix))
        .any(is_restore_artifact_id)
}

/// 相对路径里**任一分量**是恢复产物即为真。
///
/// 取分量而不是只看首段：范围根可以嵌套（外层范围根里就含内层范围根），内层范围留下的
/// 产物在外层看来就是二级目录。
fn path_is_restore_artifact(relative_path: &str) -> bool {
    normalize_relative(relative_path)
        .split('/')
        .any(is_restore_artifact_name)
}

/// 目录项是不是重解析点（符号链接 / 目录联接）。
///
/// 单独抽出来是因为要动 `remove_dir_all`：目录联接能指向别的目录，顺着删就会删到范围
/// 之外去。`FileType::is_symlink` 在 Windows 上是否覆盖目录联接依赖于 std 的实现细节，
/// 所以这里额外查一次 `FILE_ATTRIBUTE_REPARSE_POINT`，两道都过才认为是真目录。
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn scope_includes_relative(relative_path: &str, scope: &SaveScope) -> bool {
    let relative_path = normalize_relative(relative_path);
    scope
        .confirmed_files
        .iter()
        .any(|value| relative_path == normalize_relative(value))
        || scope.include_directories.iter().any(|value| {
            let directory = normalize_relative(value);
            directory == "."
                || relative_path == directory
                || relative_path.starts_with(&(directory + "/"))
        })
}

fn entry_belongs_to_profile(entry: &SaveFileEntry, profile: &SaveProfile) -> bool {
    profile.scopes.iter().any(|scope| {
        scope.root_type == entry.root_type
            && entry
                .root_path
                .as_deref()
                .map(|path| normalize_path(path) == normalize_path(&scope.root_path))
                .unwrap_or(true)
            && scope_includes_relative(&entry.relative_path, scope)
            && !is_excluded(&entry.relative_path, scope)
    })
}

fn entry_key(file: &SaveFileEntry) -> String {
    normalize_path(&format!(
        "{:?}:{}:{}",
        file.root_type,
        file.root_path.as_deref().unwrap_or_default(),
        file.relative_path
    ))
}

fn normalize_relative(path: &str) -> String {
    let normalized = path
        .replace('\\', "/")
        .trim_matches('/')
        .to_ascii_lowercase();
    if normalized == "." || normalized.is_empty() {
        ".".to_string()
    } else {
        normalized.trim_start_matches("./").to_string()
    }
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
    file.read_to_end(&mut bytes)
        .map_err(|err| format!("读取存档文件失败：{err}"))?;
    let after = fs::metadata(path).map_err(|err| format!("确认存档文件状态失败：{err}"))?;
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return Err(format!("存档文件在读取期间发生变化：{}", path.display()));
    }
    Ok((bytes, before.len()))
}

fn repository_root(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .state::<AppState>()
        .saves_root()?
        .join(".gamesaver-repository"))
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
            || left.root_path.as_deref().is_some_and(|path| {
                right
                    .root_path
                    .as_deref()
                    .is_some_and(|other| normalize_path(path) == normalize_path(other))
            }))
}

fn collected_matches_entry(file: &CollectedFile, entry: &SaveFileEntry) -> bool {
    file.root_type == entry.root_type
        && normalize_relative(&file.relative_path) == normalize_relative(&entry.relative_path)
        && (entry.root_path.is_none()
            || entry.root_path.as_deref().is_some_and(|path| {
                file.root_path
                    .as_deref()
                    .is_some_and(|other| normalize_path(path) == normalize_path(other))
            }))
}

fn collected_is_unmodified(
    file: &CollectedFile,
    old_entry: &SaveFileEntry,
    app: &AppHandle,
) -> bool {
    if !collected_matches_entry(file, old_entry) {
        return false;
    }
    if old_entry.deleted || old_entry.object_hash.is_none() {
        return false;
    }
    if old_entry.size != file.size {
        return false;
    }
    match (old_entry.mtime_ms, file.mtime_ms) {
        (Some(old_mtime), Some(cur_mtime)) if old_mtime == cur_mtime => {
            let hash = old_entry.object_hash.as_ref().unwrap();
            SaveRepository::object_exists(app, hash)
        }
        _ => false,
    }
}

fn normalize_path(path: &str) -> String {
    let trimmed = path
        .trim()
        .trim_start_matches(r"\\?\UNC\")
        .trim_start_matches(r"\\?\");
    trimmed
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
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
        let read = file
            .read(&mut buffer)
            .map_err(|err| format!("读取存档对象失败：{err}"))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn now_iso() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        build_restore_groups, entry_belongs_to_profile, find_scope_for_entry, find_scope_for_read,
        is_excluded, is_restore_artifact_name, path_is_restore_artifact, same_entries,
        sweep_restore_artifacts, wildcard_matches, SaveRepository,
    };
    use crate::domain::{
        AppStore, Game, SaveFileEntry, SaveProfile, SaveRootType, SaveScope, SaveVersion,
        UnknownFilePolicy,
    };

    fn version(game_uid: &str, version_id: &str, created_at: &str) -> SaveVersion {
        SaveVersion {
            version_id: version_id.to_string(),
            game_uid: game_uid.to_string(),
            created_at: created_at.to_string(),
            files: Vec::new(),
            total_bytes: 0,
        }
    }

    /// V1 回归：剪枝只返回「存活集」，必须剔除该游戏最旧的版本、且绝不动其他游戏。
    ///
    /// 返回值是调用方在**持久化成功之后**执行 GC 的依据，所以它必须精确等于剪枝后的
    /// store 内容 —— 任何多留/少留都会让 GC 删错对象或漏删。
    #[test]
    fn prune_keeps_newest_versions_and_leaves_other_games_alone() {
        let mut store = AppStore {
            save_versions: vec![
                version("game-a", "a1", "1700000001"),
                version("game-a", "a2", "1700000002"),
                version("game-a", "a3", "1700000003"),
                version("game-b", "b1", "1700000001"),
            ],
            ..AppStore::default()
        };

        let alive = SaveRepository::prune_game_save_versions(&mut store, "game-a", 2)
            .expect("超出保留数应发生剪枝");

        let ids: Vec<&str> = alive.iter().map(|v| v.version_id.as_str()).collect();
        assert!(ids.contains(&"a3") && ids.contains(&"a2"), "应保留最新两条");
        assert!(!ids.contains(&"a1"), "最旧的 a1 必须被剔除");
        assert!(ids.contains(&"b1"), "其他游戏的版本不得被剪掉");
        assert_eq!(alive.len(), 3);
        assert_eq!(store.save_versions.len(), 3, "store 应与返回的存活集一致");
    }

    /// 没有超出保留数时不得返回存活集 —— 否则每次游戏退出都会触发一次 CAS 全盘扫描。
    #[test]
    fn prune_is_a_noop_within_the_keep_limit_or_with_zero_keep() {
        let mut store = AppStore {
            save_versions: vec![
                version("game-a", "a1", "1700000001"),
                version("game-a", "a2", "1700000002"),
            ],
            ..AppStore::default()
        };

        assert!(
            SaveRepository::prune_game_save_versions(&mut store, "game-a", 2).is_none(),
            "未超出保留数不应触发回收"
        );
        assert!(
            SaveRepository::prune_game_save_versions(&mut store, "game-a", 0).is_none(),
            "keep=0 表示不剪枝，与旧行为一致"
        );
        assert_eq!(store.save_versions.len(), 2);
    }

    /// V5 回归：剪枝若删掉 `latest_save_version_id` 指向的那一版，指针必须当场修好。
    ///
    /// 悬垂指针不会立刻报错，而是让 `commit` 丢掉比对基线 —— 此后每次游戏退出都无条件
    /// 新建版本，版本库持续无意义膨胀，且违反设计文档「没有变化时不创建新版本」。
    #[test]
    fn prune_repairs_a_latest_pointer_it_orphans() {
        let mut game = Game::new_pending("A", "C:/games/a", "a.exe");
        game.game_uid = "game-a".to_string();
        game.latest_save_version_id = Some("a1".to_string());
        let mut store = AppStore {
            games: vec![game],
            save_versions: vec![
                version("game-a", "a1", "1700000001"),
                version("game-a", "a2", "1700000002"),
            ],
            ..AppStore::default()
        };

        SaveRepository::prune_game_save_versions(&mut store, "game-a", 1)
            .expect("超出保留数应发生剪枝");

        assert_eq!(
            store.games[0].latest_save_version_id.as_deref(),
            Some("a2"),
            "指针指向的 a1 被剪掉后应回退到最新一版"
        );
    }

    /// V8 回归：保留策略必须按 `created_at` 的**时间**语义挑最新，而不是按字符串比。
    ///
    /// `"9" > "10"` 在字符串比较下成立、在时间语义下不成立。比较一旦退化成 `String::cmp`
    /// （或哪天 `created_at` 换成别的格式），这条会留下最旧的 a9、丢掉最新的 a10 ——
    /// 正是 V8 担心的「删掉最新的、留下最旧的」。
    ///
    /// 注意这条测的是**调用点**：`compare_created_at` 自己的单测守着它实现正确，
    /// 但把 `prune_game_save_versions` 里的调用换回 `String::cmp`，那些单测照样全绿。
    #[test]
    fn prune_picks_the_newest_by_time_not_by_text() {
        let mut store = AppStore {
            save_versions: vec![version("game-a", "a9", "9"), version("game-a", "a10", "10")],
            ..AppStore::default()
        };

        let alive = SaveRepository::prune_game_save_versions(&mut store, "game-a", 1)
            .expect("超出保留数应发生剪枝");

        let ids: Vec<&str> = alive.iter().map(|v| v.version_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["a10"],
            "created_at=\"10\" 比 \"9\" 新，必须保留 a10"
        );
    }

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
        let entry = SaveFileEntry {
            root_type: SaveRootType::Custom,
            root_path: Some("C:/Game".to_string()),
            relative_path: "save.dat".to_string(),
            object_hash: Some("a".to_string()),
            size: 1,
            deleted: false,
            mtime_ms: None,
        };
        assert!(same_entries(
            std::slice::from_ref(&entry),
            &[SaveFileEntry {
                root_type: SaveRootType::Custom,
                root_path: Some("c:\\game".to_string()),
                relative_path: "SAVE.DAT".to_string(),
                object_hash: Some("a".to_string()),
                size: 1,
                deleted: false,
                mtime_ms: None
            }]
        ));
        assert!(!same_entries(
            std::slice::from_ref(&entry),
            &[SaveFileEntry {
                root_type: SaveRootType::Custom,
                root_path: Some("c:\\game".to_string()),
                relative_path: "SAVE.DAT".to_string(),
                object_hash: Some("b".to_string()),
                size: 1,
                deleted: false,
                mtime_ms: None
            }]
        ));
        assert!(!same_entries(
            std::slice::from_ref(&entry),
            &[SaveFileEntry {
                root_type: SaveRootType::Custom,
                root_path: Some("c:\\game".to_string()),
                relative_path: "SAVE.DAT".to_string(),
                object_hash: None,
                size: 0,
                deleted: true,
                mtime_ms: None
            }]
        ));
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
            keep_versions: 5,
            created_at: "0".to_string(),
            updated_at: "0".to_string(),
        };
        let entry = SaveFileEntry {
            root_type: SaveRootType::Custom,
            root_path: Some("c:\\game".to_string()),
            relative_path: "SAVE.DAT".to_string(),
            object_hash: Some("a".to_string()),
            size: 1,
            deleted: false,
            mtime_ms: None,
        };
        assert!(entry_belongs_to_profile(&entry, &profile));
    }

    #[test]
    fn restore_paths_reject_absolute_and_parent_segments() {
        assert!(super::validate_relative("C:/outside.dat").is_err());
        assert!(super::validate_relative("../outside.dat").is_err());
        assert_eq!(
            super::validate_relative("slot\\save.dat").unwrap(),
            "slot/save.dat"
        );
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
            keep_versions: 5,
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
            mtime_ms: None,
        };
        assert_eq!(
            find_scope_for_entry(&profile, &entry, "save.dat")
                .unwrap()
                .root_path,
            "C:/two"
        );
        let legacy_entry = SaveFileEntry {
            root_path: None,
            ..entry
        };
        assert!(find_scope_for_entry(&profile, &legacy_entry, "save.dat").is_err());
    }

    #[test]
    fn restore_allows_missing_standard_root_on_new_machine() {
        let destination = std::env::temp_dir().join(format!(
            "gamesaver-new-machine-save-{}",
            uuid::Uuid::new_v4()
        ));
        let game = Game::new_pending("Test Game", r"E:\GameSaverGames\games\test", "game.exe");
        let profile = SaveProfile::new(
            game.game_uid.clone(),
            "hash".to_string(),
            vec![SaveScope {
                root_type: SaveRootType::LocalLow,
                root_path: destination.to_string_lossy().to_string(),
                confirmed_files: vec!["slot1.sav".to_string()],
                include_directories: Vec::new(),
                exclude_exact: Vec::new(),
                exclude_patterns: Vec::new(),
                exclude_directories: Vec::new(),
                unknown_file_policy: UnknownFilePolicy::Protect,
                max_file_bytes: None,
            }],
            100,
            "0".to_string(),
        );
        let version = SaveVersion {
            version_id: "remote-version".to_string(),
            game_uid: game.game_uid.clone(),
            created_at: "0".to_string(),
            files: vec![SaveFileEntry {
                root_type: SaveRootType::LocalLow,
                root_path: Some(r"C:\Users\OldUser\AppData\LocalLow\Studio\Game".to_string()),
                relative_path: "slot1.sav".to_string(),
                object_hash: Some("a".repeat(64)),
                size: 1,
                deleted: false,
                mtime_ms: None,
            }],
            total_bytes: 1,
        };

        assert!(!destination.exists());
        let groups = build_restore_groups(&game, &profile, &version)
            .expect("new machine root is restorable");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].root, destination);
    }

    /// V2 回归：墓碑条目（`deleted: true`）不得因「它引用的目录已不存在」而拦住整场恢复。
    ///
    /// 墓碑只表示「这个文件在该版本里不存在」，没有任何字节需要写回磁盘，却曾被当作活条目
    /// 去做 root/scope 前置校验 —— 一个早已被删除、与本次要恢复的内容毫无关系的自定义目录，
    /// 就足以让整场恢复失败。
    #[test]
    fn restore_ignores_a_tombstone_whose_root_no_longer_exists() {
        let destination = std::env::temp_dir().join(format!(
            "gamesaver-restore-tombstone-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&destination).expect("create live root");
        let game = Game::new_pending("Test Game", r"E:\GameSaverGames\games\test", "game.exe");
        let mut live_scope = SaveScope::new_manual(destination.to_string_lossy().to_string());
        live_scope.root_type = SaveRootType::Custom;
        live_scope.confirmed_files = vec!["slot1.sav".to_string()];
        // 收窄到只保护 slot1.sav：否则 new_manual 默认的 `include_directories = ["."]`
        // 会让范围匹配一切，墓碑的 scope 解析就不会失败，这条回归就失去意义了。
        live_scope.include_directories = Vec::new();
        let profile = SaveProfile::new(
            game.game_uid.clone(),
            "hash".to_string(),
            vec![live_scope],
            100,
            "0".to_string(),
        );

        let version = SaveVersion {
            version_id: "v1".to_string(),
            game_uid: game.game_uid.clone(),
            created_at: "0".to_string(),
            files: vec![
                SaveFileEntry {
                    root_type: SaveRootType::Custom,
                    root_path: Some(destination.to_string_lossy().to_string()),
                    relative_path: "slot1.sav".to_string(),
                    object_hash: Some("a".repeat(64)),
                    size: 1,
                    deleted: false,
                    mtime_ms: None,
                },
                SaveFileEntry {
                    root_type: SaveRootType::Custom,
                    // 曾存在于一个后来被删掉的自定义目录。
                    root_path: Some(r"E:\Gone\OldSaveDir".to_string()),
                    relative_path: "gone.dat".to_string(),
                    object_hash: None,
                    size: 0,
                    deleted: true,
                    mtime_ms: None,
                },
            ],
            total_bytes: 1,
        };

        let groups = build_restore_groups(&game, &profile, &version)
            .expect("墓碑引用的失效目录不得阻断恢复");
        assert_eq!(groups.len(), 1, "只应登记实际存在的那个根目录");
        assert_eq!(groups[0].root, destination);
        assert_eq!(groups[0].entries.len(), 1);
        let _ = std::fs::remove_dir_all(&destination);
    }

    /// V2 回归：墓碑所属的范围已被用户从 profile 里移走时，同样不得阻断恢复。
    #[test]
    fn restore_ignores_a_tombstone_whose_scope_was_removed() {
        let destination = std::env::temp_dir().join(format!(
            "gamesaver-restore-scope-gone-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&destination).expect("create live root");
        let game = Game::new_pending("Test Game", r"E:\GameSaverGames\games\test", "game.exe");
        let mut live_scope = SaveScope::new_manual(destination.to_string_lossy().to_string());
        live_scope.root_type = SaveRootType::Custom;
        live_scope.confirmed_files = vec!["slot1.sav".to_string()];
        live_scope.include_directories = Vec::new();
        let profile = SaveProfile::new(
            game.game_uid.clone(),
            "hash".to_string(),
            vec![live_scope],
            100,
            "0".to_string(),
        );

        let version = SaveVersion {
            version_id: "v1".to_string(),
            game_uid: game.game_uid.clone(),
            created_at: "0".to_string(),
            files: vec![
                SaveFileEntry {
                    root_type: SaveRootType::Custom,
                    root_path: Some(destination.to_string_lossy().to_string()),
                    relative_path: "slot1.sav".to_string(),
                    object_hash: Some("a".repeat(64)),
                    size: 1,
                    deleted: false,
                    mtime_ms: None,
                },
                SaveFileEntry {
                    // 墓碑所属的范围类型（Documents）已不在 profile 里。
                    root_type: SaveRootType::Documents,
                    root_path: Some(r"C:\Users\Bob\Documents\Gone".to_string()),
                    relative_path: "gone.dat".to_string(),
                    object_hash: None,
                    size: 0,
                    deleted: true,
                    mtime_ms: None,
                },
            ],
            total_bytes: 1,
        };

        let groups = build_restore_groups(&game, &profile, &version)
            .expect("墓碑所属范围被移除后不得阻断恢复");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].root, destination);
        let _ = std::fs::remove_dir_all(&destination);
    }

    /// V2 行为保留：只剩墓碑、但目标目录仍在（典型场景是游戏把存档目录整个清空）时，该范围
    /// **仍须**参与恢复 —— 否则「恢复到空」这个合法操作会被「保存版本没有可恢复的存档文件」
    /// 挡掉。这条专门钉住「墓碑不能一刀切跳过」这个边界，防止用一个更粗暴的写法把能力改没。
    #[test]
    fn restore_registers_a_root_that_only_tombstones_reference() {
        let destination = std::env::temp_dir().join(format!(
            "gamesaver-restore-all-deleted-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&destination).expect("create root");
        let game = Game::new_pending("Test Game", r"E:\GameSaverGames\games\test", "game.exe");
        let mut scope = SaveScope::new_manual(destination.to_string_lossy().to_string());
        scope.root_type = SaveRootType::Custom;
        scope.confirmed_files = vec!["slot1.sav".to_string()];
        let profile = SaveProfile::new(
            game.game_uid.clone(),
            "hash".to_string(),
            vec![scope],
            100,
            "0".to_string(),
        );

        let version = SaveVersion {
            version_id: "v1".to_string(),
            game_uid: game.game_uid.clone(),
            created_at: "0".to_string(),
            files: vec![SaveFileEntry {
                root_type: SaveRootType::Custom,
                root_path: Some(destination.to_string_lossy().to_string()),
                relative_path: "slot1.sav".to_string(),
                object_hash: None,
                size: 0,
                deleted: true,
                mtime_ms: None,
            }],
            total_bytes: 0,
        };

        let groups =
            build_restore_groups(&game, &profile, &version).expect("只剩墓碑的版本仍应指向其范围");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].root, destination);
        assert!(
            groups[0].entries.is_empty(),
            "墓碑本身不提供任何要写入的文件"
        );
        let _ = std::fs::remove_dir_all(&destination);
    }

    #[test]
    fn stat_cache_detects_unmodified_metadata() {
        let file = super::CollectedFile {
            path: std::path::PathBuf::from("C:/Game/save.dat"),
            root_type: SaveRootType::Custom,
            root_path: Some("C:/Game".to_string()),
            relative_path: "save.dat".to_string(),
            size: 1024,
            mtime_ms: Some(1700000000000),
        };
        let matching_entry = SaveFileEntry {
            root_type: SaveRootType::Custom,
            root_path: Some("c:/game".to_string()),
            relative_path: "save.dat".to_string(),
            object_hash: Some("mock_hash".to_string()),
            size: 1024,
            deleted: false,
            mtime_ms: Some(1700000000000),
        };
        let different_size = SaveFileEntry {
            size: 2048,
            ..matching_entry.clone()
        };
        let _different_mtime = SaveFileEntry {
            mtime_ms: Some(1700000000500),
            ..matching_entry.clone()
        };
        let deleted_entry = SaveFileEntry {
            deleted: true,
            ..matching_entry.clone()
        };

        assert!(super::collected_matches_entry(&file, &matching_entry));
        assert_eq!(file.size, matching_entry.size);
        assert_eq!(file.mtime_ms, matching_entry.mtime_ms);
        assert_ne!(file.size, different_size.size);
        assert!(deleted_entry.deleted);
    }

    #[test]
    fn scope_root_preserves_managed_game_subdirectories() {
        use std::path::Path;
        let game = crate::domain::Game {
            game_uid: "g1".to_string(),
            game_key: "g1".to_string(),
            display_name: "Game 1".to_string(),
            managed_path: r"D:\Games\Game1".to_string(),
            lifecycle: crate::domain::GameLifecycle::Active,
            health: crate::domain::GameHealth::Ready,
            cloud_status: crate::domain::game::CloudStatus::LocalOnly,
            launch: crate::domain::game::LaunchConfig {
                executable_relative_path: "game.exe".to_string(),
                arguments: Vec::new(),
                working_directory_relative_path: None,
            },
            cover: None,
            save_profile_id: None,
            last_played_at: None,
            latest_save_version_id: None,
            added_at: None,
        };

        // 1. scope.root_path is exactly managed_path
        let mut scope_exact = SaveScope::new_manual(r"D:\Games\Game1".to_string());
        scope_exact.root_type = SaveRootType::ManagedGame;
        assert_eq!(
            super::scope_root(&game, &scope_exact),
            Path::new(r"D:\Games\Game1")
        );

        // 2. scope.root_path is a subdirectory inside managed_path
        let mut scope_sub = SaveScope::new_manual(r"D:\Games\Game1\SaveData".to_string());
        scope_sub.root_type = SaveRootType::ManagedGame;
        assert_eq!(
            super::scope_root(&game, &scope_sub),
            Path::new(r"D:\Games\Game1\SaveData")
        );

        // 3. scope.root_path is from an old path before library relocation
        let mut scope_relocated = SaveScope::new_manual(r"C:\OldGames\Game1\SaveData".to_string());
        scope_relocated.root_type = SaveRootType::ManagedGame;
        assert_eq!(
            super::scope_root(&game, &scope_relocated),
            Path::new(r"D:\Games\Game1\SaveData")
        );
    }

    #[test]
    fn normalize_path_strips_windows_verbatim_prefix() {
        assert_eq!(
            super::normalize_path(r"\\?\C:\Games\Save"),
            r"c:\games\save"
        );
        assert_eq!(
            super::normalize_path(r"\\?\UNC\server\share\save"),
            r"server\share\save"
        );
        assert_eq!(super::normalize_path(r"C:/Games/Save/"), r"c:\games\save");
    }

    #[test]
    fn collect_profile_files_ignores_non_existent_directory() {
        let mut profile = SaveProfile::new(
            "g1".to_string(),
            "hash".to_string(),
            vec![SaveScope::new_manual(
                r"C:\NonExistent_GS_Test_Dir_XYZ".to_string(),
            )],
            100,
            "2026-01-01".to_string(),
        );
        profile.scopes[0].root_type = SaveRootType::Custom;
        let result = super::collect_profile_files(&profile);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 0);
    }

    #[test]
    fn find_scope_for_entry_resolves_cross_user_path() {
        let mut scope =
            SaveScope::new_manual(r"C:\Users\Bob\AppData\LocalLow\GameA\Saves".to_string());
        scope.root_type = SaveRootType::LocalLow;
        scope.confirmed_files = vec!["save.dat".to_string()];

        let profile = SaveProfile::new(
            "g1".to_string(),
            "hash".to_string(),
            vec![scope],
            100,
            "2026-01-01".to_string(),
        );

        let entry = SaveFileEntry {
            root_type: SaveRootType::LocalLow,
            root_path: Some(r"C:\Users\Alice\AppData\LocalLow\GameA\Saves".to_string()),
            relative_path: "save.dat".to_string(),
            object_hash: Some("mock_hash".to_string()),
            size: 100,
            deleted: false,
            mtime_ms: None,
        };

        let resolved = super::find_scope_for_entry(&profile, &entry, "save.dat");
        assert!(resolved.is_ok());
        assert_eq!(
            resolved.unwrap().root_path,
            r"C:\Users\Bob\AppData\LocalLow\GameA\Saves"
        );
    }

    #[test]
    fn find_scope_for_entry_disambiguates_multiple_scopes_by_subpath() {
        let mut scope1 = SaveScope::new_manual(r"C:\Users\Bob\Documents\StudioA\GameX".to_string());
        scope1.root_type = SaveRootType::Documents;
        scope1.confirmed_files = vec!["save.dat".to_string()];

        let mut scope2 = SaveScope::new_manual(r"C:\Users\Bob\Documents\StudioB\GameY".to_string());
        scope2.root_type = SaveRootType::Documents;
        scope2.confirmed_files = vec!["save.dat".to_string()];

        let profile = SaveProfile::new(
            "g1".to_string(),
            "hash".to_string(),
            vec![scope1, scope2],
            100,
            "2026-01-01".to_string(),
        );

        let entry = SaveFileEntry {
            root_type: SaveRootType::Documents,
            root_path: Some(r"C:\Users\Alice\Documents\StudioA\GameX".to_string()),
            relative_path: "save.dat".to_string(),
            object_hash: Some("mock_hash".to_string()),
            size: 100,
            deleted: false,
            mtime_ms: None,
        };

        let resolved = super::find_scope_for_entry(&profile, &entry, "save.dat");
        assert!(resolved.is_ok());
        assert_eq!(
            resolved.unwrap().root_path,
            r"C:\Users\Bob\Documents\StudioA\GameX"
        );
    }

    #[test]
    fn find_scope_for_read_picks_the_scope_matching_the_entry_root_path() {
        let mut scope_a =
            SaveScope::new_manual(r"C:\Users\Bob\Documents\StudioA\GameX".to_string());
        scope_a.root_type = SaveRootType::Documents;
        scope_a.confirmed_files = vec!["save.dat".to_string()];

        let mut scope_b =
            SaveScope::new_manual(r"C:\Users\Bob\Documents\StudioB\GameY".to_string());
        scope_b.root_type = SaveRootType::Documents;
        scope_b.confirmed_files = vec!["save.dat".to_string()];

        let profile = SaveProfile::new(
            "g1".to_string(),
            "hash".to_string(),
            vec![scope_a, scope_b],
            100,
            "2026-01-01".to_string(),
        );

        // 条目实际属于 B，但 A 排在前面 —— 旧实现按 root_type 取第一个会读到 A。
        let entry = SaveFileEntry {
            root_type: SaveRootType::Documents,
            root_path: Some(r"C:\Users\Bob\Documents\StudioB\GameY".to_string()),
            relative_path: "save.dat".to_string(),
            object_hash: Some("mock_hash".to_string()),
            size: 100,
            deleted: false,
            mtime_ms: None,
        };

        let resolved = find_scope_for_read(&profile, &entry).expect("应当解析出 B");
        assert_eq!(resolved.root_path, r"C:\Users\Bob\Documents\StudioB\GameY");
    }

    #[test]
    fn find_scope_for_read_refuses_to_guess_between_same_type_scopes() {
        let mut scope_a =
            SaveScope::new_manual(r"C:\Users\Bob\Documents\StudioA\GameX".to_string());
        scope_a.root_type = SaveRootType::Documents;
        let mut scope_b =
            SaveScope::new_manual(r"C:\Users\Bob\Documents\StudioB\GameY".to_string());
        scope_b.root_type = SaveRootType::Documents;

        let profile = SaveProfile::new(
            "g1".to_string(),
            "hash".to_string(),
            vec![scope_a, scope_b],
            100,
            "2026-01-01".to_string(),
        );

        // 条目没有记录 root_path（老版本条目），两个同类型范围无法区分 —— 必须报错而不是取第一个。
        let entry = SaveFileEntry {
            root_type: SaveRootType::Documents,
            root_path: None,
            relative_path: "save.dat".to_string(),
            object_hash: Some("mock_hash".to_string()),
            size: 100,
            deleted: false,
            mtime_ms: None,
        };

        assert!(find_scope_for_read(&profile, &entry).is_err());
    }

    #[test]
    fn find_scope_for_read_still_reads_files_outside_a_narrowed_scope() {
        let mut scope = SaveScope::new_manual(r"C:\Users\Bob\Documents\Game".to_string());
        scope.root_type = SaveRootType::Documents;
        scope.confirmed_files = vec!["SaveData/slot1.sav".to_string()];
        // 用户把范围收窄到只保留 SaveData，之后旧版本里落在范围外的文件仍应可读可传。
        scope.include_directories = vec!["SaveData".to_string()];

        let profile = SaveProfile::new(
            "g1".to_string(),
            "hash".to_string(),
            vec![scope],
            100,
            "2026-01-01".to_string(),
        );

        let entry = SaveFileEntry {
            root_type: SaveRootType::Documents,
            root_path: Some(r"C:\Users\Bob\Documents\Game".to_string()),
            relative_path: "Config/settings.ini".to_string(),
            object_hash: Some("mock_hash".to_string()),
            size: 100,
            deleted: false,
            mtime_ms: None,
        };

        // 打包侧（read）不因范围收窄而失败……
        assert!(find_scope_for_read(&profile, &entry).is_ok());
        // ……恢复侧（restore）仍严格拒绝落在范围外的条目。
        assert!(find_scope_for_entry(&profile, &entry, "Config/settings.ini").is_err());
    }

    fn temp_root(label: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("gamesaver-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create temp root");
        root
    }

    /// V7 回归：产物目录名必须带 `restore_group` 拼出来的那个 id（32 位十六进制）。
    ///
    /// 只认前缀是不够的 —— 用户真有一个叫 `.gamesaver-restore-notes.txt` 的存档时会被一起
    /// 当垃圾排除，等于替用户丢掉一个文件。
    #[test]
    fn restore_artifact_names_require_a_hex_id() {
        let id = "0f8b2c1d4e5a6b7c8d9e0f1a2b3c4d5e";
        assert!(is_restore_artifact_name(&format!(
            ".gamesaver-restore-{id}"
        )));
        assert!(is_restore_artifact_name(&format!(
            ".gamesaver-rollback-{id}"
        )));
        // 嵌套在子目录里同样算产物（范围根可以嵌套，内层范围的产物在外层看来是二级目录）。
        assert!(path_is_restore_artifact(&format!(
            "slot\\.gamesaver-rollback-{id}\\save.dat"
        )));

        assert!(!is_restore_artifact_name(".gamesaver-restore-notes.txt"));
        assert!(!is_restore_artifact_name(".gamesaver-restore-"));
        assert!(!is_restore_artifact_name("gamesaver-restore-0f8b"));
        assert!(!path_is_restore_artifact("saves/slot1.sav"));
    }

    /// V7 回归（防线 1）：恢复产物绝不进版本。
    ///
    /// 收进版本之后它会一路被上传云端，将来恢复那一版时再把垃圾铺回用户的存档目录，而且每
    /// 崩一次多一份。
    #[test]
    fn collect_profile_files_skips_restore_artifacts() {
        let root = temp_root("v7-collect");
        std::fs::write(root.join("slot1.sav"), b"real save").expect("write save");
        let id = "0f8b2c1d4e5a6b7c8d9e0f1a2b3c4d5e";
        let staging = root.join(format!(".gamesaver-restore-{id}"));
        std::fs::create_dir_all(&staging).expect("create staging");
        std::fs::write(staging.join("slot1.sav"), b"target copy").expect("write staged");

        // `new_manual` 默认 `include_directories = ["."]`，会递归扫整个根目录 —— 没有这道
        // 剔除，staging 里的副本就会被当成存档收进来。
        let mut scope = SaveScope::new_manual(root.to_string_lossy().to_string());
        scope.root_type = SaveRootType::Custom;
        let profile = SaveProfile::new(
            "g1".to_string(),
            "hash".to_string(),
            vec![scope],
            100,
            "2026-01-01".to_string(),
        );

        let collected = super::collect_profile_files(&profile).expect("collect");
        let relatives: Vec<&str> = collected
            .iter()
            .map(|file| file.relative_path.as_str())
            .collect();
        assert_eq!(relatives, vec!["slot1.sav"], "只应收集真实存档");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// V7 回归（防线 2）：只有 staging 残留，说明崩溃发生在「还没动用户文件」之前 ——
    /// 直接删掉残留即可，用户存档一个字节都不该动。
    #[test]
    fn sweep_deletes_a_staging_only_leftover() {
        let root = temp_root("v7-sweep-staging");
        std::fs::write(root.join("slot1.sav"), b"real save").expect("write save");
        let id = "0f8b2c1d4e5a6b7c8d9e0f1a2b3c4d5e";
        let staging = root.join(format!(".gamesaver-restore-{id}"));
        std::fs::create_dir_all(&staging).expect("create staging");
        std::fs::write(staging.join("slot1.sav"), b"target copy").expect("write staged");

        sweep_restore_artifacts(&root).expect("sweep");

        assert!(!staging.exists(), "staging 残留应被清掉");
        assert_eq!(
            std::fs::read(root.join("slot1.sav")).expect("read save"),
            b"real save",
            "用户存档不得被动过"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// V7 回归（防线 2 的核心不变量）：崩溃落在「把现有存档搬进 rollback」之后时，用户存档
    /// 目录里那些位置是空的、真身躺在 rollback 里 —— 清扫必须**先归位、再清理**。
    ///
    /// 直接删残留就会把用户的存档删掉，所以这条钉的是顺序。
    #[test]
    fn sweep_restores_the_rollback_copy_before_cleaning() {
        let root = temp_root("v7-sweep-rollback");
        let id = "0f8b2c1d4e5a6b7c8d9e0f1a2b3c4d5e";
        let rollback = root.join(format!(".gamesaver-rollback-{id}"));
        std::fs::create_dir_all(rollback.join("nested")).expect("create rollback");
        std::fs::write(rollback.join("slot1.sav"), b"original").expect("write rollback");
        std::fs::write(rollback.join("nested").join("deep.dat"), b"deep").expect("write deep");
        // 本次恢复已经装进去的文件（半成品）：归位时应当被原档覆盖。
        std::fs::write(root.join("slot1.sav"), b"half restored").expect("write half");

        sweep_restore_artifacts(&root).expect("sweep");

        assert!(!rollback.exists(), "归位完成后回滚副本应被清掉");
        assert_eq!(
            std::fs::read(root.join("slot1.sav")).expect("read save"),
            b"original",
            "原存档应覆盖半成品"
        );
        assert_eq!(
            std::fs::read(root.join("nested").join("deep.dat")).expect("read deep"),
            b"deep",
            "嵌套路径也要按原样建目录并归位"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// V7 回归（防线 2 的反面）：只要有**一个**文件没能归位，回滚副本就必须留着。
    ///
    /// 删掉它等于把用户的存档删了。这里把归位目标做成同名目录来制造失败 —— 不敢覆盖目录，
    /// 只能报错并保留副本。
    #[test]
    fn sweep_keeps_the_rollback_when_a_file_cannot_be_restored() {
        let root = temp_root("v7-sweep-keep");
        let id = "0f8b2c1d4e5a6b7c8d9e0f1a2b3c4d5e";
        let rollback = root.join(format!(".gamesaver-rollback-{id}"));
        std::fs::create_dir_all(&rollback).expect("create rollback");
        std::fs::write(rollback.join("slot1.sav"), b"original").expect("write rollback");
        std::fs::create_dir_all(root.join("slot1.sav")).expect("occupy destination");

        let error = sweep_restore_artifacts(&root).expect_err("应报错而不是默默丢档");

        assert!(rollback.is_dir(), "归位失败时回滚副本必须保留");
        assert_eq!(
            std::fs::read(rollback.join("slot1.sav")).expect("read rollback"),
            b"original"
        );
        assert!(
            error.contains("slot1.sav"),
            "错误信息应指出是哪个文件：{error}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// V7 回归：只是「长得像」产物的用户文件与目录不能被误删 —— 严格匹配 id 的意义就在这里。
    #[test]
    fn sweep_leaves_lookalike_entries_alone() {
        let root = temp_root("v7-sweep-lookalike");
        std::fs::write(root.join(".gamesaver-restore-notes.txt"), b"mine").expect("write notes");
        let fake = root.join(".gamesaver-restore-nothex");
        std::fs::create_dir_all(&fake).expect("create fake");

        sweep_restore_artifacts(&root).expect("sweep");

        assert!(
            root.join(".gamesaver-restore-notes.txt").is_file(),
            "同名用户文件不得被删"
        );
        assert!(fake.is_dir(), "后缀不是十六进制的目录不得被删");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// V7 回归：修复前已经收进了产物的历史版本，恢复时跳过那些条目 —— 既不回写垃圾，也不
    /// 因为「条目落在哪个范围」而让整个版本变成不可恢复。
    #[test]
    fn restore_skips_polluted_entries_from_an_older_version() {
        let root = temp_root("v7-polluted");
        let game = Game::new_pending("Test Game", r"E:\GameSaverGames\games\test", "game.exe");
        let mut scope = SaveScope::new_manual(root.to_string_lossy().to_string());
        scope.root_type = SaveRootType::Custom;
        // 收窄到只认 slot1.sav：否则默认的 `include_directories = ["."]` 会匹配一切，
        // 产物条目也能解析到范围，这条回归就失去意义了。
        scope.confirmed_files = vec!["slot1.sav".to_string()];
        scope.include_directories = Vec::new();
        let profile = SaveProfile::new(
            game.game_uid.clone(),
            "hash".to_string(),
            vec![scope],
            100,
            "0".to_string(),
        );

        let id = "0f8b2c1d4e5a6b7c8d9e0f1a2b3c4d5e";
        let root_path = root.to_string_lossy().to_string();
        let version = SaveVersion {
            version_id: "v1".to_string(),
            game_uid: game.game_uid.clone(),
            created_at: "0".to_string(),
            files: vec![
                SaveFileEntry {
                    root_type: SaveRootType::Custom,
                    root_path: Some(root_path.clone()),
                    relative_path: "slot1.sav".to_string(),
                    object_hash: Some("a".repeat(64)),
                    size: 1,
                    deleted: false,
                    mtime_ms: None,
                },
                SaveFileEntry {
                    root_type: SaveRootType::Custom,
                    root_path: Some(root_path),
                    relative_path: format!(".gamesaver-restore-{id}/slot1.sav"),
                    object_hash: Some("b".repeat(64)),
                    size: 1,
                    deleted: false,
                    mtime_ms: None,
                },
            ],
            total_bytes: 2,
        };

        let groups = build_restore_groups(&game, &profile, &version)
            .expect("历史污染不得让版本变成不可恢复");
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].entries.len(),
            1,
            "产物条目应被跳过，只恢复真实存档"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
