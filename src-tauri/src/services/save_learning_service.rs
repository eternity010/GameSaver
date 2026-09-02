use crate::domain::{
    ActiveLearningSession, FileFingerprint, Game, LearningSessionView, LearningStatus,
    SaveLearningResult, SaveRootType, SaveScope, SaveScopeDraft, UnknownFilePolicy,
};
use crate::services::learning::{
    collect_related_files_by_trace, extend_tracked_process_tree, stop_etw_capture,
    try_start_etw_capture,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;
use walkdir::WalkDir;
use tauri::AppHandle;

const MAX_CANDIDATE_FILE_BYTES: u64 = 10 * 1024 * 1024;
const SAVE_EXTENSIONS: [&str; 8] = ["sav", "save", "dat", "json", "db", "sqlite", "ini", "cfg"];
const RESOURCE_EXTENSIONS: [&str; 12] = [
    "dll", "exe", "pak", "pdb", "png", "jpg", "jpeg", "webp", "ogg", "wav", "mp3", "ttf",
];
const NAME_HINTS: [&str; 7] = ["save", "slot", "profile", "userdata", "autosave", "progress", "system"];
const SAVE_DIRECTORY_HINTS: [&str; 8] = [
    "save", "savedata", "saves", "savegame", "savegames", "profile", "profiles", "userdata",
];
const DEFAULT_EXCLUDE_PATTERNS: [&str; 8] = [
    "*.tmp",
    "*.temp",
    "*.log",
    "*.dmp",
    "*.bak",
    "*.etl",
    "*.csv",
    "*.cache",
];
const DEFAULT_EXCLUDE_DIRECTORIES: [&str; 5] = [
    "logs",
    "crashdumps",
    "cache",
    "shadercache",
    "webcache",
];

pub struct SaveLearningService;

impl SaveLearningService {
    pub fn start(app: &AppHandle, game: &Game, on_progress: impl Fn(u8, &str), is_cancelled: impl Fn() -> bool) -> Result<ActiveLearningSession, String> {
        let executable_path = managed_executable_path(game)?;
        if !executable_path.is_file() {
            return Err("受管游戏的启动程序不存在，请先修复游戏本体目录".to_string());
        }
        let roots = discover_scan_roots(game)?;
        crate::logging::info(format!(
            "存档学习扫描范围：game_uid={} roots={} paths={}",
            game.game_uid,
            roots.len(),
            roots
                .iter()
                .map(|root| root.physical_path.display().to_string())
                .collect::<Vec<_>>()
                .join(" | ")
        ));
        on_progress(5, &format!("已确定 {} 个存档扫描范围", roots.len()));
        let session_id = Uuid::new_v4().to_string();
        let mut etw_start_error = None;
        let etw_capture = match try_start_etw_capture(app, &session_id) {
            Ok(handle) => {
                on_progress(56, "ETW 已启动，准备记录游戏文件操作");
                Some(handle)
            }
            Err(error) => {
                etw_start_error = Some(error);
                on_progress(56, "ETW 不可用，将使用快照差异继续学习");
                None
            }
        };
        let baseline = if etw_capture.is_some() {
            None
        } else {
            on_progress(8, "ETW 不可用，正在记录保存前快照");
            Some(collect_snapshot(&roots, |progress, message| on_progress(8 + progress / 2, message), &is_cancelled)?)
        };
        if etw_capture.is_some() {
            on_progress(57, "ETW-first 模式：跳过启动前全量快照");
        }
        if is_cancelled() {
            if let Some(handle) = etw_capture.as_ref() {
                let _ = stop_etw_capture(&handle.trace_name);
                let _ = std::fs::remove_file(&handle.etl_path);
            }
            return Err("任务已取消".to_string());
        }
        on_progress(58, "正在启动游戏");
        let parent = executable_path.parent().ok_or_else(|| "启动程序缺少工作目录".to_string())?;
        let mut command = Command::new(&executable_path);
        command.current_dir(parent);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                if let Some(handle) = etw_capture.as_ref() {
                    let _ = stop_etw_capture(&handle.trace_name);
                    let _ = std::fs::remove_file(&handle.etl_path);
                }
                return Err(format!("启动游戏失败：{err}"));
            }
        };
        let root_pid = child.id();
        let mut tracked_pid_set = HashSet::from([root_pid]);
        if let Err(error) = extend_tracked_process_tree(&mut tracked_pid_set) {
            etw_start_error.get_or_insert(format!("进程树扩展失败：{error}"));
        }
        let tracked_pids = Arc::new(Mutex::new(sorted_pids(tracked_pid_set)));
        let process_tracker_stop = Arc::new(AtomicBool::new(false));
        spawn_process_tracker(Arc::clone(&tracked_pids), Arc::clone(&process_tracker_stop));
        let view = LearningSessionView {
            session_id,
            game_uid: game.game_uid.clone(),
            root_pid,
            started_at: now_iso(),
            status: LearningStatus::Capturing,
        };
        on_progress(100, "游戏已启动，请在游戏内完成一次保存");
        Ok(ActiveLearningSession {
            view,
            roots,
            baseline,
            tracked_pids,
            process_tracker_stop,
            etw_capture,
            etw_start_error,
        })
    }

    pub fn finish(
        active: &ActiveLearningSession,
        on_progress: impl Fn(u8, &str),
        is_cancelled: impl Fn() -> bool,
    ) -> Result<SaveLearningResult, String> {
        on_progress(10, "正在读取保存后的文件状态");
        active.process_tracker_stop.store(true, Ordering::Release);
        let mut tracked_pid_set = active
            .tracked_pids
            .lock()
            .map_err(|_| "读取游戏进程树失败".to_string())?
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        let _ = extend_tracked_process_tree(&mut tracked_pid_set);
        let tracked_pids = sorted_pids(tracked_pid_set);
        let mut etw_files = HashSet::new();
        let mut etw_operations = Vec::new();
        let mut notes = Vec::new();
        let mut event_capture_mode = "snapshot".to_string();
        if let Some(capture) = active.etw_capture.as_ref() {
            notes.push("ETW-first 模式未执行启动前全量 baseline，候选范围直接依据 ETW 写入证据生成".to_string());
            match collect_related_files_by_trace(
                Some(&capture.trace_name),
                Some(&capture.etl_path.to_string_lossy()),
                &tracked_pids,
            ) {
                Ok(collection) => {
                    event_capture_mode = "etw".to_string();
                    etw_files = collection.files;
                    etw_operations = collection.operations;
                    notes.extend(collection.logs);
                }
                Err(error) => notes.push(format!("ETW 解析失败，已回退快照差异：{error}")),
            }
        } else if let Some(error) = active.etw_start_error.as_ref() {
            notes.push(format!("ETW 未启动，已使用快照差异：{error}"));
        }
        if event_capture_mode == "etw" {
            let fallback_files = discover_save_container_files(&active.roots, &is_cancelled)?;
            if !fallback_files.is_empty() {
                let existing_count = etw_files.len();
                let added_count = fallback_files
                    .iter()
                    .filter(|path| !etw_files.contains(*path))
                    .count();
                notes.push(format!(
                    "已从游戏专属目录补充 {} 个常见存档容器文件（新增 {} 个）；候选仍可编辑",
                    fallback_files.len(),
                    added_count
                ));
                etw_files.extend(fallback_files);
                crate::logging::info(format!(
                    "存档学习容器兜底：ETW 文件={}，补充文件={}，新增文件={}",
                    existing_count,
                    etw_files.len().saturating_sub(existing_count),
                    added_count
                ));
            } else {
                notes.push("ETW 未在已识别游戏目录找到常见存档容器，将仅使用 ETW 文件证据".to_string());
                crate::logging::info("存档学习容器兜底：未找到常见存档容器");
            }
        }
        let baseline = active.baseline.as_ref();
        let final_snapshot = if etw_files.is_empty() {
            collect_snapshot(&active.roots, |progress, message| on_progress(snapshot_analysis_progress(progress), message), &is_cancelled)?
        } else {
            on_progress(45, "ETW 已定位文件，正在读取目标文件状态");
            let targeted = collect_targeted_snapshot(&active.roots, &etw_files, &is_cancelled)?;
            if targeted.is_empty() {
                notes.push("ETW 文件无法直接读取，已回退完整快照差异".to_string());
                collect_snapshot(&active.roots, |progress, message| on_progress(snapshot_analysis_progress(progress), message), &is_cancelled)?
            } else {
                targeted
            }
        };
        if is_cancelled() {
            return Err("任务已取消".to_string());
        }
        on_progress(92, "正在按文件夹整理存档候选");
        let (changed_files, scope_drafts, mut inference_notes) = infer_scope_drafts(active, &final_snapshot, baseline, &etw_files);
        notes.append(&mut inference_notes);
        let transaction_summary = (!etw_operations.is_empty() || active.etw_capture.is_some())
            .then(|| crate::services::learning::analyze_save_transactions(etw_operations));
        let confidence = calculate_learning_confidence(
            &scope_drafts,
            &event_capture_mode,
            transaction_summary.as_ref(),
        );
        Ok(SaveLearningResult {
            session_id: active.view.session_id.clone(),
            changed_files,
            scope_drafts,
            confidence,
            notes,
            event_capture_mode,
            transaction_summary,
        })
    }
}

fn snapshot_analysis_progress(scan_progress: u8) -> u8 {
    10 + ((u16::from(scan_progress) * 4 / 5) as u8)
}

fn sorted_pids(pids: HashSet<u32>) -> Vec<u32> {
    let mut values = pids.into_iter().collect::<Vec<_>>();
    values.sort_unstable();
    values
}

fn spawn_process_tracker(tracked_pids: Arc<Mutex<Vec<u32>>>, stop: Arc<AtomicBool>) {
    thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            if let Ok(mut current) = tracked_pids.lock() {
                let mut process_set = current.iter().copied().collect::<HashSet<_>>();
                let _ = extend_tracked_process_tree(&mut process_set);
                *current = sorted_pids(process_set);
            }
            thread::sleep(Duration::from_secs(1));
        }
    });
}

fn managed_executable_path(game: &Game) -> Result<PathBuf, String> {
    let root = Path::new(&game.managed_path);
    let relative = Path::new(&game.launch.executable_relative_path);
    if relative.is_absolute() || relative.components().any(|component| matches!(component, std::path::Component::ParentDir)) {
        return Err("启动程序相对路径无效".to_string());
    }
    Ok(root.join(relative))
}

fn discover_scan_roots(game: &Game) -> Result<Vec<crate::domain::ScanRoot>, String> {
    let managed_path = PathBuf::from(&game.managed_path).canonicalize().map_err(|err| format!("解析受管游戏目录失败：{err}"))?;
    let mut roots = vec![crate::domain::ScanRoot {
        root_type: SaveRootType::ManagedGame,
        physical_path: managed_path,
    }];
    let hints = game_name_hints(game);
    let known_roots = [
        ("APPDATA", SaveRootType::AppData),
        ("LOCALAPPDATA", SaveRootType::LocalAppData),
    ];
    for (variable, root_type) in known_roots {
        let Some(raw_root) = env::var_os(variable) else { continue };
        let root = PathBuf::from(raw_root);
        if !root.is_dir() { continue; }
        for path in find_candidate_directories(&root, &hints) {
            roots.push(crate::domain::ScanRoot { root_type, physical_path: path });
        }
    }
    if let Ok(profile) = env::var("USERPROFILE") {
        let profile_root = PathBuf::from(profile);
        let documents = profile_root.join("Documents");
        if documents.is_dir() {
            for path in find_candidate_directories(&documents, &hints) {
                roots.push(crate::domain::ScanRoot { root_type: SaveRootType::Documents, physical_path: path });
            }
        }
        let saved_games = profile_root.join("Saved Games");
        if saved_games.is_dir() {
            for path in find_candidate_directories(&saved_games, &hints) {
                roots.push(crate::domain::ScanRoot { root_type: SaveRootType::SavedGames, physical_path: path });
            }
        }
        let local_low = profile_root.join("AppData").join("LocalLow");
        if local_low.is_dir() {
            for path in find_candidate_directories(&local_low, &hints) {
                roots.push(crate::domain::ScanRoot { root_type: SaveRootType::LocalLow, physical_path: path });
            }
        }
    }
    let mut seen = HashSet::new();
    roots.retain(|root| seen.insert(normalize_path(&root.physical_path)));
    Ok(roots)
}

fn find_candidate_directories(root: &Path, hints: &[String]) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let mut frontier = vec![root.to_path_buf()];
    for depth in 0..3 {
        let mut next = Vec::new();
        for parent in frontier {
            let Ok(entries) = fs::read_dir(parent) else { continue; };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                let Ok(file_type) = entry.file_type() else { continue; };
                if !file_type.is_dir() || is_scan_noise_directory(&path) { continue; }
                if hints.iter().any(|hint| directory_matches_hint(&path, hint)) {
                    candidates.push(path);
                } else if depth < 2 {
                    next.push(path);
                }
            }
        }
        frontier = next;
    }
    candidates
}

fn is_scan_noise_directory(path: &Path) -> bool {
    let name = path.file_name().and_then(|value| value.to_str()).unwrap_or_default().to_ascii_lowercase();
    ["temp", "cache", "logs", "crashdumps", "shadercache", "packages", "microsoft", "google", "mozilla", "nvidia"].contains(&name.as_str())
}

fn game_name_hints(game: &Game) -> Vec<String> {
    let mut hints = game.display_name.split(|character: char| !character.is_ascii_alphanumeric()).filter(|item| item.len() >= 3).map(|item| item.to_ascii_lowercase()).collect::<Vec<_>>();
    let compact_name = game.display_name.chars().filter(|character| character.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
    if compact_name.len() >= 3 && !hints.contains(&compact_name) { hints.push(compact_name); }
    if let Some(stem) = Path::new(&game.launch.executable_relative_path).file_stem().and_then(|value| value.to_str()) {
        let stem = stem.to_ascii_lowercase();
        let compact_stem = stem.chars().filter(|character| character.is_ascii_alphanumeric()).collect::<String>();
        if stem.len() >= 3 && !hints.contains(&stem) { hints.push(stem); }
        if compact_stem.len() >= 3 && !hints.contains(&compact_stem) { hints.push(compact_stem); }
    }
    hints
}

fn directory_matches_hint(path: &Path, hint: &str) -> bool {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let compact_name = name
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect::<String>();
    name == hint || compact_name == hint || (hint.len() >= 4 && compact_name.contains(hint))
}

fn discover_save_container_files(
    roots: &[crate::domain::ScanRoot],
    is_cancelled: &impl Fn() -> bool,
) -> Result<HashSet<String>, String> {
    let mut files = HashSet::new();
    for root in roots {
        if is_cancelled() {
            return Err("任务已取消".to_string());
        }
        if !root.physical_path.is_dir() {
            continue;
        }
        let mut containers = Vec::new();
        if is_save_container_directory(&root.physical_path) {
            containers.push(root.physical_path.clone());
        } else if root.root_type == SaveRootType::ManagedGame {
            let Ok(entries) = fs::read_dir(&root.physical_path) else { continue; };
            containers.extend(entries.filter_map(Result::ok).filter_map(|entry| {
                let path = entry.path();
                entry.file_type().ok().filter(|kind| kind.is_dir()).and_then(|_| is_save_container_directory(&path).then_some(path))
            }));
        } else {
            for entry in WalkDir::new(&root.physical_path).follow_links(false).max_depth(3) {
                let Ok(entry) = entry else { continue; };
                if entry.file_type().is_dir() && is_save_container_directory(entry.path()) {
                    containers.push(entry.path().to_path_buf());
                }
            }
        }
        for container in containers {
            for entry in WalkDir::new(container).follow_links(false) {
                if is_cancelled() {
                    return Err("任务已取消".to_string());
                }
                let Ok(entry) = entry else { continue; };
                if !entry.file_type().is_file() || is_noise_path(entry.path()) {
                    continue;
                }
                let Ok(metadata) = entry.metadata() else { continue; };
                if metadata.len() > MAX_CANDIDATE_FILE_BYTES {
                    continue;
                }
                files.insert(normalize_path(entry.path()));
            }
        }
    }
    Ok(files)
}

fn is_save_container_directory(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    SAVE_DIRECTORY_HINTS.contains(&name.as_str())
}

fn collect_snapshot(
    roots: &[crate::domain::ScanRoot],
    on_progress: impl Fn(u8, &str),
    is_cancelled: &impl Fn() -> bool,
) -> Result<HashMap<String, FileFingerprint>, String> {
    let mut files = HashMap::new();
    for (root_index, root) in roots.iter().enumerate() {
        if !root.physical_path.is_dir() { continue; }
        for entry in WalkDir::new(&root.physical_path).follow_links(false) {
            if is_cancelled() { return Err("任务已取消".to_string()); }
            let entry = entry.map_err(|err| format!("扫描存档目录失败：{err}"))?;
            if !entry.file_type().is_file() || is_noise_path(entry.path()) { continue; }
            let metadata = entry.metadata().map_err(|err| format!("读取存档文件信息失败：{err}"))?;
            files.insert(normalize_path(entry.path()), FileFingerprint { size: metadata.len(), modified_unix: modified_unix(&metadata) });
        }
        on_progress(((root_index + 1) * 100 / roots.len().max(1)) as u8, &format!("已扫描第 {} 个范围", root_index + 1));
    }
    Ok(files)
}

fn resolve_scope_root_and_relative(
    file_path: &Path,
    scan_root_path: &Path,
) -> Option<(PathBuf, String)> {
    let parent = file_path.parent()?;
    let mut current = parent;
    let mut chosen_root = None;
    let scan_root_norm = normalize_path(scan_root_path);

    loop {
        if is_save_container_directory(current) {
            chosen_root = Some(current.to_path_buf());
        }
        let current_norm = normalize_path(current);
        if current_norm == scan_root_norm || !current_norm.starts_with(&scan_root_norm) {
            break;
        }
        match current.parent() {
            Some(p) => current = p,
            None => break,
        }
    }

    let root = chosen_root.unwrap_or_else(|| parent.to_path_buf());
    let root_norm = normalize_path(&root);
    let file_norm = normalize_path(file_path);

    let relative = if file_norm.starts_with(&root_norm) {
        let remainder = file_norm[root_norm.len()..].trim_start_matches('\\');
        remainder.replace('\\', "/")
    } else {
        file_path.file_name()?.to_string_lossy().replace('\\', "/")
    };

    Some((root, relative))
}

fn infer_scope_drafts(
    active: &ActiveLearningSession,
    final_snapshot: &HashMap<String, FileFingerprint>,
    baseline: Option<&HashMap<String, FileFingerprint>>,
    etw_files: &HashSet<String>,
) -> (Vec<String>, Vec<SaveScopeDraft>, Vec<String>) {
    let mut changed = Vec::new();
    if let Some(baseline) = baseline {
        for (path, fingerprint) in final_snapshot {
            if baseline.get(path) != Some(fingerprint) {
                changed.push(path.clone());
            }
        }
        for path in baseline.keys() {
            if !final_snapshot.contains_key(path) {
                changed.push(path.clone());
            }
        }
    } else {
        for path in etw_files {
            if final_snapshot.contains_key(path) {
                changed.push(path.clone());
            }
        }
    }
    changed.sort();
    changed.dedup();

    struct ScopeGroup {
        physical_path: PathBuf,
        count: usize,
        files: Vec<String>,
        noise_exact: Vec<String>,
        root_type: SaveRootType,
    }

    let mut groups: BTreeMap<String, ScopeGroup> = BTreeMap::new();
    let mut unhandled_noise: Vec<(String, &FileFingerprint)> = Vec::new();

    for path in &changed {
        if !etw_files.is_empty() && !etw_files.contains(path) {
            continue;
        }
        let fingerprint = final_snapshot
            .get(path)
            .or_else(|| baseline.and_then(|b| b.get(path)));
        let Some(fingerprint) = fingerprint else {
            continue;
        };

        let candidate_by_etw = !etw_files.is_empty() && is_etw_candidate(path);
        let candidate_by_snapshot = etw_files.is_empty() && is_save_candidate(path);
        let is_candidate = fingerprint.size <= MAX_CANDIDATE_FILE_BYTES
            && (candidate_by_etw || candidate_by_snapshot);

        let Some(scan_root) = active
            .roots
            .iter()
            .filter(|root| path_is_within_root(path, &root.physical_path))
            .max_by_key(|root| root.physical_path.components().count())
        else {
            continue;
        };

        let file_path = Path::new(path);
        let Some((scope_root, relative)) =
            resolve_scope_root_and_relative(file_path, &scan_root.physical_path)
        else {
            continue;
        };

        if is_candidate {
            let key = normalize_path(&scope_root);
            let entry = groups.entry(key).or_insert_with(|| ScopeGroup {
                physical_path: scope_root,
                count: 0,
                files: Vec::new(),
                noise_exact: Vec::new(),
                root_type: scan_root.root_type,
            });
            entry.count += 1;
            if final_snapshot.contains_key(path) {
                entry.files.push(relative);
            }
        } else {
            unhandled_noise.push((path.clone(), fingerprint));
        }
    }

    for (noise_path_str, _) in unhandled_noise {
        let noise_path = Path::new(&noise_path_str);
        let noise_norm = normalize_path(noise_path);
        for group in groups.values_mut() {
            let group_norm = normalize_path(&group.physical_path);
            if noise_norm.starts_with(&group_norm) {
                let rel = noise_norm[group_norm.len()..].trim_start_matches('\\').replace('\\', "/");
                if !rel.is_empty() && !group.files.contains(&rel) {
                    group.noise_exact.push(rel);
                }
            }
        }
    }

    let mut drafts = Vec::new();
    for (_, mut group) in groups {
        group.files.sort();
        group.files.dedup();
        group.noise_exact.sort();
        group.noise_exact.dedup();

        let protects_container = is_save_container_directory(&group.physical_path);
        if group.files.is_empty() && !protects_container {
            continue;
        }

        let mut exclude_patterns: Vec<String> = DEFAULT_EXCLUDE_PATTERNS
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut exclude_directories: Vec<String> = DEFAULT_EXCLUDE_DIRECTORIES
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut exclude_exact: Vec<String> = Vec::new();

        for noise in group.noise_exact {
            if group.files.contains(&noise) {
                continue;
            }
            if let Some((dir, _)) = noise.split_once('/') {
                let dir_norm = dir.to_ascii_lowercase();
                if !exclude_directories.iter().any(|d| d.to_ascii_lowercase() == dir_norm) {
                    exclude_directories.push(dir.to_string());
                }
            }
            if let Some(ext) = Path::new(&noise).extension().and_then(|e| e.to_str()) {
                let ext_pattern = format!("*.{}", ext.to_ascii_lowercase());
                if !exclude_patterns.contains(&ext_pattern) {
                    exclude_patterns.push(ext_pattern);
                }
            }
            if !exclude_exact.contains(&noise) {
                exclude_exact.push(noise);
            }
        }

        let unknown_file_policy = if protects_container || group.root_type == SaveRootType::SavedGames {
            UnknownFilePolicy::Protect
        } else {
            UnknownFilePolicy::Ignore
        };

        let physical_root_str = group.physical_path.to_string_lossy().replace('/', "\\");

        drafts.push(SaveScopeDraft {
            scope: SaveScope {
                root_type: group.root_type,
                root_path: physical_root_str,
                confirmed_files: group.files.clone(),
                include_directories: protects_container.then(|| ".".to_string()).into_iter().collect(),
                exclude_exact,
                exclude_patterns,
                exclude_directories,
                unknown_file_policy,
                max_file_bytes: Some(MAX_CANDIDATE_FILE_BYTES),
            },
            changed_files: group.files,
            confidence: if group.count >= 2 { 80 } else { 65 },
        });
    }

    let mut notes = vec![(if etw_files.is_empty() {
        "当前使用快照差异按文件夹归类，已自动注入标准排除规则与伴生噪音过滤。"
    } else {
        "当前优先使用 ETW 写入证据按文件夹归类，已自动注入标准排除规则与伴生噪音过滤。"
    }).to_string()];
    if drafts.is_empty() {
        notes.push("没有发现符合存档特征的变化，请确认游戏内完成了一次保存，或手动添加存档目录。".to_string());
    } else {
        notes.push("默认只保护 10 MB 以内的存档候选，大文件、日志与噪音缓存已自动排除。".to_string());
    }
    (changed, drafts, notes)
}

fn calculate_learning_confidence(
    scope_drafts: &[SaveScopeDraft],
    event_capture_mode: &str,
    transaction_summary: Option<&crate::domain::SaveTransactionSummary>,
) -> u8 {
    if scope_drafts.is_empty() {
        return 0;
    }

    let max_draft_confidence = scope_drafts
        .iter()
        .map(|draft| draft.confidence)
        .max()
        .unwrap_or(65);

    let has_named_save_container = scope_drafts.iter().any(|draft| {
        is_save_container_directory(Path::new(&draft.scope.root_path))
    });
    let container_bonus = if has_named_save_container { 5 } else { 0 };

    let mode_adjustment: i16 = match (event_capture_mode, transaction_summary) {
        ("etw", Some(txn)) if txn.status == "completed" => {
            let txn_ratio = (txn.confidence as f32 / 100.0).clamp(0.0, 1.0);
            10 + (txn_ratio * 5.0).round() as i16
        }
        ("etw", Some(txn)) if txn.status == "candidate" => 5,
        ("etw", _) => 2,
        _ => 0,
    };

    let total = (max_draft_confidence as i16) + (container_bonus as i16) + mode_adjustment;
    total.clamp(30, 95) as u8
}

fn collect_targeted_snapshot(
    roots: &[crate::domain::ScanRoot],
    etw_files: &HashSet<String>,
    is_cancelled: &impl Fn() -> bool,
) -> Result<HashMap<String, FileFingerprint>, String> {
    let mut files = HashMap::new();
    for path in etw_files {
        if is_cancelled() {
            return Err("任务已取消".to_string());
        }
        let candidate = PathBuf::from(path);
        if !roots.iter().any(|root| {
            let candidate_path = normalize_path(&candidate);
            let root_path = normalize_path(&root.physical_path);
            candidate_path == root_path || candidate_path.starts_with(&(root_path + "\\"))
        }) {
            continue;
        }
        if !candidate.is_file() || is_noise_path(&candidate) {
            continue;
        }
        let Ok(metadata) = fs::metadata(&candidate) else {
            continue;
        };
        files.insert(
            normalize_path(&candidate),
            FileFingerprint {
                size: metadata.len(),
                modified_unix: modified_unix(&metadata),
            },
        );
    }
    Ok(files)
}

fn is_save_candidate(path: &str) -> bool {
    let path_obj = Path::new(path);
    if is_noise_path(path_obj) {
        return false;
    }
    let path_lower = path.to_ascii_lowercase();
    if path_lower.contains("\\analytics\\") || path_lower.contains("/analytics/") {
        return false;
    }
    let extension = path_obj.extension().and_then(|value| value.to_str()).unwrap_or_default();
    let ext_lower = extension.to_ascii_lowercase();
    if ext_lower == "log" || ext_lower == "tmp" || ext_lower == "dmp" || RESOURCE_EXTENSIONS.contains(&ext_lower.as_str()) {
        return false;
    }
    SAVE_EXTENSIONS.contains(&ext_lower.as_str())
        || NAME_HINTS.iter().any(|hint| path_obj.file_stem().and_then(|value| value.to_str()).unwrap_or_default().to_ascii_lowercase().contains(hint))
        || path_lower.contains("\\savedata\\")
        || path_lower.contains("/savedata/")
}

fn is_etw_candidate(path: &str) -> bool {
    if is_noise_path(Path::new(path)) {
        return false;
    }
    let path_lower = path.to_ascii_lowercase();
    if path_lower.contains("\\analytics\\") || path_lower.contains("/analytics/") {
        return false;
    }
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let extension = extension.to_ascii_lowercase();
    extension != "log" && !RESOURCE_EXTENSIONS.contains(&extension.as_str())
}

fn path_is_within_root(path: &str, root: &Path) -> bool {
    let normalized_path = normalize_path(Path::new(path));
    let normalized_root = normalize_path(root);
    normalized_path == normalized_root
        || normalized_path.starts_with(&(normalized_root + "\\"))
}

fn is_noise_path(path: &Path) -> bool {
    let text = path.to_string_lossy().to_ascii_lowercase();
    [
        "com.gamesaver.desktop",
        "com.gamesaver.next",
        "\\cache\\",
        "\\logs\\",
        "\\temp\\",
        "\\crashdumps\\",
        "\\shadercache\\",
        "/cache/",
        "/logs/",
    ]
    .iter()
    .any(|item| text.contains(item))
}

fn modified_unix(metadata: &fs::Metadata) -> u64 {
    metadata.modified().ok().and_then(|value| value.duration_since(UNIX_EPOCH).ok()).map(|value| value.as_secs()).unwrap_or_default()
}

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_ascii_lowercase()
}

fn now_iso() -> String {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|value| value.as_secs().to_string()).unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::{
        calculate_learning_confidence, directory_matches_hint, discover_save_container_files,
        infer_scope_drafts, is_etw_candidate, is_save_container_directory, normalize_path,
        path_is_within_root, snapshot_analysis_progress, MAX_CANDIDATE_FILE_BYTES,
    };
    use crate::domain::{
        ActiveLearningSession, FileFingerprint, LearningSessionView, LearningStatus, SaveRootType,
        SaveScope, SaveScopeDraft, SaveTransactionSummary, ScanRoot, UnknownFilePolicy,
    };
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    use std::sync::{atomic::AtomicBool, Arc, Mutex};

    #[test]
    fn snapshot_progress_does_not_overflow_at_completion() {
        assert_eq!(snapshot_analysis_progress(0), 10);
        assert_eq!(snapshot_analysis_progress(64), 61);
        assert_eq!(snapshot_analysis_progress(100), 90);
    }

    #[test]
    fn etw_candidates_accept_non_resource_files() {
        assert!(is_etw_candidate(r"C:\GameSaver\save\slot.xml"));
        assert!(is_etw_candidate(r"C:\GameSaver\Data\profile.bin"));
    }

    #[test]
    fn etw_candidates_reject_resources_and_noise() {
        assert!(!is_etw_candidate(r"C:\GameSaver\Game.exe"));
        assert!(!is_etw_candidate(r"C:\GameSaver\cache\profile.bin"));
        assert!(!is_etw_candidate(r"C:\GameSaver\logs\session.dat"));
        assert!(!is_etw_candidate(
            r"C:\Users\Player\AppData\Roaming\com.gamesaver.next\events\trace.etl"
        ));
    }

    #[test]
    fn recognizes_common_save_container_names() {
        assert!(is_save_container_directory(Path::new(r"C:\\Users\\Player\\SaveData")));
        assert!(is_save_container_directory(Path::new(r"C:\\Users\\Player\\Save Games")));
        assert!(!is_save_container_directory(Path::new(r"C:\\Users\\Player\\Analytics")));
    }

    #[test]
    fn scan_root_matching_does_not_match_arbitrary_parent_path_text() {
        assert!(directory_matches_hint(Path::new(r"C:\\Users\\Player\\AppData\\LocalLow\\ApplePie\\MonsterBlackMarket"), "blackmarket"));
        assert!(!directory_matches_hint(Path::new(r"C:\\Users\\Player\\AppData\\LocalLow\\BlackMarket\\UnrelatedPublisher"), "blackmarket"));
    }

    #[test]
    fn fallback_collects_files_from_a_game_specific_save_container() {
        let root = std::env::current_dir()
            .expect("resolve test working directory")
            .join(format!("gamesaver-save-container-{}", uuid::Uuid::new_v4()));
        let save_data = root.join("SaveData");
        fs::create_dir_all(&save_data).expect("create SaveData directory");
        fs::write(save_data.join("PlayerData0.sav"), b"save").expect("write save file");
        fs::write(save_data.join("large-resource.bin"), vec![0; (MAX_CANDIDATE_FILE_BYTES + 1) as usize])
            .expect("write oversized file");
        fs::write(root.join("Player.log"), b"log").expect("write loose log");

        let files = discover_save_container_files(
            &[ScanRoot {
                root_type: SaveRootType::LocalAppData,
                physical_path: root.clone(),
            }],
            &|| false,
        )
        .expect("discover save container files");

        assert_eq!(files.len(), 1);
        assert!(files.iter().any(|path| path.ends_with(r"\savedata\playerdata0.sav")));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn path_matching_is_case_insensitive_for_windows_roots() {
        assert!(path_is_within_root(
            r"c:\users\player\gamesaver\save.dat",
            Path::new(r"C:\Users\Player\GameSaver")
        ));
        assert!(!path_is_within_root(
            r"c:\users\player\gamesaver-old\save.dat",
            Path::new(r"C:\Users\Player\GameSaver")
        ));
    }

    #[test]
    fn saved_games_root_type_serializes_and_matches() {
        assert_eq!(
            serde_json::to_string(&SaveRootType::SavedGames).unwrap(),
            "\"saved_games\""
        );
        let deserialized: SaveRootType = serde_json::from_str("\"saved_games\"").unwrap();
        assert_eq!(deserialized, SaveRootType::SavedGames);
    }

    #[test]
    fn local_low_root_type_serializes_and_matches() {
        assert_eq!(
            serde_json::to_string(&SaveRootType::LocalLow).unwrap(),
            "\"local_low\""
        );
        let deserialized: SaveRootType = serde_json::from_str("\"local_low\"").unwrap();
        assert_eq!(deserialized, SaveRootType::LocalLow);
    }

    #[test]
    fn learning_confidence_returns_zero_when_no_drafts() {
        assert_eq!(calculate_learning_confidence(&[], "etw", None), 0);
        assert_eq!(calculate_learning_confidence(&[], "snapshot", None), 0);
    }

    #[test]
    fn learning_confidence_scales_with_evidence_and_transactions() {
        let dummy_scope = SaveScope {
            root_type: SaveRootType::AppData,
            root_path: r"C:\Users\Player\AppData\LocalLow\GameStudio".to_string(),
            confirmed_files: vec!["profile.sav".to_string()],
            include_directories: vec![],
            exclude_exact: vec![],
            exclude_patterns: vec![],
            exclude_directories: vec![],
            unknown_file_policy: UnknownFilePolicy::Protect,
            max_file_bytes: Some(10_000_000),
        };
        let single_file_draft = SaveScopeDraft {
            scope: dummy_scope.clone(),
            changed_files: vec!["profile.sav".to_string()],
            confidence: 65,
        };
        // 纯快照单文件 -> 65%
        assert_eq!(
            calculate_learning_confidence(&[single_file_draft.clone()], "snapshot", None),
            65
        );

        let container_scope = SaveScope {
            root_type: SaveRootType::SavedGames,
            root_path: r"C:\Users\Player\Saved Games\MyGame\SaveData".to_string(),
            confirmed_files: vec!["slot1.sav".to_string(), "slot2.sav".to_string()],
            include_directories: vec![".".to_string()],
            exclude_exact: vec![],
            exclude_patterns: vec![],
            exclude_directories: vec![],
            unknown_file_policy: UnknownFilePolicy::Protect,
            max_file_bytes: Some(10_000_000),
        };
        let container_draft = SaveScopeDraft {
            scope: container_scope,
            changed_files: vec!["slot1.sav".to_string(), "slot2.sav".to_string()],
            confidence: 80,
        };
        // 快照模式多文件 + SaveData 容器命名奖励 -> 85%
        assert_eq!(
            calculate_learning_confidence(&[container_draft.clone()], "snapshot", None),
            85
        );

        let completed_txn = SaveTransactionSummary {
            status: "completed".to_string(),
            confidence: 90,
            transaction_count: 1,
            affected_files: vec!["slot1.sav".to_string()],
            affected_directories: vec!["SaveData".to_string()],
            started_at: Some("1000".to_string()),
            ended_at: Some("1500".to_string()),
            operation_count: 3,
            notes: vec![],
        };
        // ETW 完整事务 + 命名容器 -> 95% (顶格封顶)
        assert_eq!(
            calculate_learning_confidence(
                &[container_draft],
                "etw",
                Some(&completed_txn)
            ),
            95
        );
    }

    #[test]
    fn bidirectional_diff_captures_deleted_and_created_files() {
        let active = ActiveLearningSession {
            view: LearningSessionView {
                session_id: "test-sess".to_string(),
                game_uid: "game-1".to_string(),
                root_pid: 100,
                started_at: "0".to_string(),
                status: LearningStatus::Capturing,
            },
            roots: vec![ScanRoot {
                root_type: SaveRootType::SavedGames,
                physical_path: PathBuf::from(r"c:\users\player\saved games\mygame"),
            }],
            baseline: None,
            tracked_pids: Arc::new(Mutex::new(vec![100])),
            process_tracker_stop: Arc::new(AtomicBool::new(false)),
            etw_capture: None,
            etw_start_error: None,
        };

        let old_save = r"c:\users\player\saved games\mygame\save_old.sav".to_string();
        let new_save = r"c:\users\player\saved games\mygame\save_new.sav".to_string();

        let mut baseline = HashMap::new();
        baseline.insert(old_save.clone(), FileFingerprint { size: 1024, modified_unix: 100 });

        let mut final_snapshot = HashMap::new();
        final_snapshot.insert(new_save.clone(), FileFingerprint { size: 2048, modified_unix: 200 });

        let etw_empty = HashSet::new();
        let (changed, drafts, _) = infer_scope_drafts(&active, &final_snapshot, Some(&baseline), &etw_empty);

        // 双向 diff 必须同时捕获被删除的 old_save 和新建的 new_save
        assert_eq!(changed.len(), 2);
        assert!(changed.contains(&old_save));
        assert!(changed.contains(&new_save));

        // 生成的 scope draft confirmed_files 只包含当前实际存活的 new_save
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].scope.confirmed_files, vec!["save_new.sav".to_string()]);
        // 自动注入了标准排除规则
        assert!(drafts[0].scope.exclude_patterns.contains(&"*.log".to_string()));
        assert!(drafts[0].scope.exclude_patterns.contains(&"*.tmp".to_string()));
        assert!(drafts[0].scope.exclude_directories.contains(&"logs".to_string()));
    }

    #[test]
    fn smart_container_lift_clusters_nested_slots_and_injects_preset_exclusions() {
        let active = ActiveLearningSession {
            view: LearningSessionView {
                session_id: "test-sess-lift".to_string(),
                game_uid: "game-lift".to_string(),
                root_pid: 100,
                started_at: "0".to_string(),
                status: LearningStatus::Capturing,
            },
            roots: vec![ScanRoot {
                root_type: SaveRootType::LocalLow,
                physical_path: PathBuf::from(r"C:\Users\Player\AppData\LocalLow\Company\MyGame"),
            }],
            baseline: None,
            tracked_pids: Arc::new(Mutex::new(vec![100])),
            process_tracker_stop: Arc::new(AtomicBool::new(false)),
            etw_capture: None,
            etw_start_error: None,
        };

        let slot1 = r"c:\users\player\appdata\locallow\company\mygame\savedata\slot1\save.dat".to_string();
        let slot2 = r"c:\users\player\appdata\locallow\company\mygame\savedata\slot2\save.dat".to_string();
        let log_file = r"c:\users\player\appdata\locallow\company\mygame\savedata\player.log".to_string();

        let mut final_snapshot = HashMap::new();
        final_snapshot.insert(slot1.clone(), FileFingerprint { size: 1024, modified_unix: 200 });
        final_snapshot.insert(slot2.clone(), FileFingerprint { size: 1024, modified_unix: 200 });
        final_snapshot.insert(log_file.clone(), FileFingerprint { size: 512, modified_unix: 200 });

        let etw_empty = HashSet::new();
        let (_, drafts, _) = infer_scope_drafts(&active, &final_snapshot, Some(&HashMap::new()), &etw_empty);

        // 两个槽位必须自动聚类提升至同一个 SaveData 顶层容器
        assert_eq!(drafts.len(), 1);
        let draft = &drafts[0];
        assert_eq!(normalize_path(Path::new(&draft.scope.root_path)), r"c:\users\player\appdata\locallow\company\mygame\savedata");
        assert_eq!(draft.scope.confirmed_files, vec!["slot1/save.dat".to_string(), "slot2/save.dat".to_string()]);
        assert_eq!(draft.scope.include_directories, vec![".".to_string()]);
        assert_eq!(draft.scope.unknown_file_policy, UnknownFilePolicy::Protect);

        // 同目录发现的 Player.log 伴生噪音必须自动转为精确排除
        assert!(draft.scope.exclude_exact.contains(&"player.log".to_string()));
        // 必须自动带有通用排除模式
        assert!(draft.scope.exclude_patterns.contains(&"*.tmp".to_string()));
        assert!(draft.scope.exclude_patterns.contains(&"*.log".to_string()));
    }
}
