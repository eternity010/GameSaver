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

pub struct SaveLearningService;

impl SaveLearningService {
    pub fn start(app: &AppHandle, game: &Game, on_progress: impl Fn(u8, &str), is_cancelled: impl Fn() -> bool) -> Result<ActiveLearningSession, String> {
        let executable_path = managed_executable_path(game)?;
        if !executable_path.is_file() {
            return Err("受管游戏的启动程序不存在，请先修复游戏本体目录".to_string());
        }
        let roots = discover_scan_roots(game)?;
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
        let baseline = active.baseline.as_ref();
        let final_snapshot = if etw_files.is_empty() {
            collect_snapshot(&active.roots, |progress, message| on_progress(10 + progress * 4 / 5, message), &is_cancelled)?
        } else {
            on_progress(45, "ETW 已定位文件，正在读取目标文件状态");
            let targeted = collect_targeted_snapshot(&active.roots, &etw_files, &is_cancelled)?;
            if targeted.is_empty() {
                notes.push("ETW 文件无法直接读取，已回退完整快照差异".to_string());
                collect_snapshot(&active.roots, |progress, message| on_progress(10 + progress * 4 / 5, message), &is_cancelled)?
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
        let confidence = if scope_drafts.is_empty() { 0 } else { 70 };
        Ok(SaveLearningResult {
            session_id: active.view.session_id.clone(),
            changed_files,
            scope_drafts,
            confidence,
            notes,
            event_capture_mode,
            transaction_summary: (!etw_operations.is_empty() || active.etw_capture.is_some())
                .then(|| crate::services::learning::analyze_save_transactions(etw_operations)),
        })
    }
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
                roots.push(crate::domain::ScanRoot { root_type: SaveRootType::Documents, physical_path: path });
            }
        }
        let local_low = profile_root.join("AppData").join("LocalLow");
        if local_low.is_dir() {
            for path in find_candidate_directories(&local_low, &hints) {
                roots.push(crate::domain::ScanRoot { root_type: SaveRootType::LocalAppData, physical_path: path });
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
                let text = path.to_string_lossy().to_ascii_lowercase();
                if hints.iter().any(|hint| text.contains(hint)) { candidates.push(path.clone()); }
                if depth < 2 { next.push(path); }
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
    if let Some(stem) = Path::new(&game.launch.executable_relative_path).file_stem().and_then(|value| value.to_str()) {
        let stem = stem.to_ascii_lowercase();
        if stem.len() >= 3 && !hints.contains(&stem) { hints.push(stem); }
    }
    hints
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

fn infer_scope_drafts(active: &ActiveLearningSession, final_snapshot: &HashMap<String, FileFingerprint>, baseline: Option<&HashMap<String, FileFingerprint>>, etw_files: &HashSet<String>) -> (Vec<String>, Vec<SaveScopeDraft>, Vec<String>) {
    let mut changed = Vec::new();
    if let Some(baseline) = baseline {
        for (path, fingerprint) in final_snapshot {
            if baseline.get(path) != Some(fingerprint) { changed.push(path.clone()); }
        }
    } else {
        for path in etw_files {
            if final_snapshot.contains_key(path) { changed.push(path.clone()); }
        }
    }
    changed.sort();
    let mut groups: BTreeMap<String, (usize, Vec<String>, SaveRootType)> = BTreeMap::new();
    for path in &changed {
        if !etw_files.is_empty() && !etw_files.contains(path) { continue; }
        let Some(fingerprint) = final_snapshot.get(path) else { continue; };
        if fingerprint.size > MAX_CANDIDATE_FILE_BYTES || !is_save_candidate(path) { continue; }
        let Some(scan_root) = active.roots.iter().filter(|root| Path::new(path).starts_with(&root.physical_path)).max_by_key(|root| root.physical_path.components().count()) else { continue; };
        let file_path = Path::new(path);
        let Some(parent) = file_path.parent() else { continue; };
        let key = normalize_path(parent);
        let relative = file_path.file_name().map(|name| name.to_string_lossy().replace('\\', "/"));
        let Some(relative) = relative else { continue; };
        let entry = groups.entry(key).or_insert((0, Vec::new(), scan_root.root_type));
        entry.0 += 1;
        entry.1.push(relative);
    }
    let mut drafts = Vec::new();
    for (physical_root, (count, mut files, root_type)) in groups {
        files.sort();
        files.dedup();
        drafts.push(SaveScopeDraft {
            scope: SaveScope {
                root_type,
                root_path: physical_root,
                confirmed_files: files.clone(),
                include_directories: Vec::new(),
                exclude_exact: Vec::new(),
                exclude_patterns: Vec::new(),
                exclude_directories: Vec::new(),
                unknown_file_policy: UnknownFilePolicy::Protect,
                max_file_bytes: Some(MAX_CANDIDATE_FILE_BYTES),
            },
            changed_files: files,
            confidence: if count >= 2 { 80 } else { 65 },
        });
    }
    let mut notes = vec!["当前使用快照差异按文件夹归类，候选范围保存前仍可编辑。".to_string()];
    if drafts.is_empty() { notes.push("没有发现符合存档特征的变化，请确认游戏内完成了一次保存，或手动添加存档目录。".to_string()); }
    else { notes.push("默认只保护 10 MB 以内的存档候选，大文件和游戏资源不会自动加入。".to_string()); }
    (changed, drafts, notes)
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
    let path_lower = path.to_ascii_lowercase();
    let extension = Path::new(path).extension().and_then(|value| value.to_str()).unwrap_or_default();
    if RESOURCE_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()) { return false; }
    SAVE_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()) || NAME_HINTS.iter().any(|hint| Path::new(path).file_stem().and_then(|value| value.to_str()).unwrap_or_default().to_ascii_lowercase().contains(hint)) || path_lower.contains("\\savedata\\") || path_lower.contains("/savedata/")
}

fn is_noise_path(path: &Path) -> bool {
    let text = path.to_string_lossy().to_ascii_lowercase();
    ["\\cache\\", "\\logs\\", "\\temp\\", "\\crashdumps\\", "\\shadercache\\", "/cache/", "/logs/"].iter().any(|item| text.contains(item))
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
