use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    process::Command,
    sync::Mutex,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Manager, State};
use uuid::Uuid;
use walkdir::WalkDir;

const SCORE_TIME_MATCH: i64 = 30;
const SCORE_EXTENSION_MATCH: i64 = 20;
const SCORE_WEAK_EXTENSION_MATCH: i64 = 8;
const SCORE_KEYWORD_MATCH: i64 = 25;
const SCORE_GAME_NAME_MATCH: i64 = 20;
const SCORE_FILENAME_MATCH: i64 = 12;
const SCORE_CHANGE_COUNT_MATCH: i64 = 10;
const SCORE_ADDED_FILE_MATCH: i64 = 8;
const SCORE_USER_SAVE_ROOT_MATCH: i64 = 10;
const SCORE_GAME_DIR_MATCH: i64 = 6;
const SCORE_SIZE_REASONABLE: i64 = 6;
const SCORE_NOISE_PATH_PENALTY: i64 = 30;
const SCORE_NOISE_FILENAME_PENALTY: i64 = 20;
const SCORE_TOO_MANY_CHANGES_PENALTY: i64 = 20;
const SCORE_WEAK_ONLY_PENALTY: i64 = 15;
const LOW_CONFIDENCE_THRESHOLD: i64 = 45;
const STRONG_SAVE_EXTENSIONS: [&str; 4] = ["sav", "save", "profile", "slot"];
const WEAK_SAVE_EXTENSIONS: [&str; 3] = ["dat", "json", "bin"];
const PATH_KEYWORDS: [&str; 4] = ["save", "savedata", "profile", "userdata"];
const FILENAME_SAVE_KEYWORDS: [&str; 5] = ["save", "slot", "profile", "global", "system"];
const NOISE_FILENAME_KEYWORDS: [&str; 8] = ["config", "settings", "log", "cache", "crash", "tmp", "temp", "shader"];
const WEAK_PATH_FRAGMENTS: [&str; 7] = [
    "\\cache\\",
    "\\logs\\",
    "\\log\\",
    "\\crash\\",
    "\\config\\",
    "\\settings\\",
    "\\shader",
];
const APP_IDENTIFIER: &str = "com.gamesaver.desktop";
const USERPROFILE_TOKEN: &str = "%USERPROFILE%";
const GAME_DIR_TOKEN: &str = "%GAME_DIR%";
const DEFAULT_BACKUP_KEEP_VERSIONS: usize = 10;
const MAX_BACKUP_KEEP_VERSIONS: usize = 200;
const NOISE_PATH_FRAGMENTS: [&str; 7] = [
    "\\appdata\\local\\temp\\",
    "\\appdata\\local\\tencent\\wetype\\",
    "\\appdata\\local\\microsoft\\edge\\",
    "\\appdata\\local\\google\\chrome\\",
    "\\appdata\\roaming\\microsoft\\windows\\",
    "\\$recycle.bin\\",
    "\\ebwebview\\",
];

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct FileMeta {
    size: u64,
    modified_unix: u64,
    extension: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    snapshot_ref: String,
    created_at_unix: u64,
    files: HashMap<String, FileMeta>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct CandidatePath {
    path: String,
    score: i64,
    changed_files: usize,
    added_files: usize,
    modified_files: usize,
    matched_signals: Vec<String>,
    recommendation: String,
    collapsed: bool,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct LearningSession {
    session_id: String,
    game_id: String,
    exe_path: String,
    started_at: String,
    ended_at: Option<String>,
    status: String,
    baseline_snapshot_ref: String,
    final_snapshot_ref: Option<String>,
    candidates: Vec<CandidatePath>,
    pid: Option<u32>,
    #[serde(default)]
    tracked_pids: Vec<u32>,
    #[serde(default)]
    event_capture_mode: String,
    #[serde(default)]
    event_trace_name: Option<String>,
    #[serde(default)]
    event_trace_path: Option<String>,
    #[serde(default)]
    captured_event_count: usize,
    #[serde(default)]
    event_capture_error: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct GameSaveRule {
    rule_id: String,
    game_id: String,
    #[serde(default)]
    game_uid: String,
    exe_hash: String,
    confirmed_paths: Vec<String>,
    created_at: String,
    confidence: i64,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    updated_at: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct PersistedStore {
    sessions: Vec<LearningSession>,
    rules: Vec<GameSaveRule>,
    #[serde(default)]
    launcher_sessions: Vec<LauncherSession>,
    #[serde(default = "default_execution_config")]
    execution_config: ExecutionConfig,
}

struct AppState {
    store: Mutex<PersistedStore>,
    tasks: Mutex<HashMap<String, BackgroundTask>>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct BackgroundTask {
    task_id: String,
    task_type: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    progress: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    started_at: String,
    updated_at: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeStatus {
    is_admin: bool,
    can_use_etw: bool,
    message: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportRulesResult {
    count: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ImportRulesResult {
    imported: usize,
    overwritten: usize,
    skipped: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportMigrationZipResult {
    rule_count: usize,
    backup_games: usize,
    exported_files: usize,
    skipped_backup_games: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ImportMigrationZipResult {
    imported_rules: usize,
    overwritten_rules: usize,
    skipped_rules: usize,
    imported_backup_games: usize,
    copied_backup_files: usize,
    skipped_backup_games: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MigrationGameIndexItem {
    game_uid: String,
    game_id: String,
    rule_ids: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct LauncherSession {
    launcher_session_id: String,
    exe_path: String,
    exe_hash: String,
    matched_rule_id: Option<String>,
    matched_game_id: Option<String>,
    #[serde(default)]
    matched_game_uid: Option<String>,
    #[serde(default)]
    launch_mode: String,
    status: String,
    pid: Option<u32>,
    injection_status: String,
    #[serde(default)]
    redirect_root: Option<String>,
    #[serde(default)]
    injector_exit_code: Option<i32>,
    #[serde(default)]
    hook_version: Option<String>,
    #[serde(default)]
    sandbox_box_name: Option<String>,
    #[serde(default)]
    sandbox_mirror_paths: Vec<String>,
    started_at: String,
    updated_at: String,
    logs: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResolveRuleResult {
    exe_hash: String,
    matched_rule: Option<GameSaveRule>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ExecutionConfig {
    managed_save_root: String,
    backup_root: String,
    block_on_inject_fail: bool,
    sandbox_root: String,
    sandboxie_start_exe: String,
    #[serde(default)]
    preferred_exe_by_uid: HashMap<String, String>,
    #[serde(default)]
    preferred_rule_uid_by_game: HashMap<String, String>,
    #[serde(default)]
    preferred_rule_id_by_exe_hash: HashMap<String, String>,
    #[serde(default)]
    backup_keep_versions_by_uid: HashMap<String, usize>,
    #[serde(default, alias = "preferredExeByGame", skip_serializing)]
    preferred_exe_by_game_legacy: HashMap<String, String>,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        default_execution_config()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RedirectRuntimeInfo {
    arch: String,
    injector_path: String,
    dll_path: String,
    managed_save_root: String,
    backup_root: String,
    injector_exists: bool,
    dll_exists: bool,
    sandbox_root: String,
    sandboxie_path: String,
    sandboxie_exists: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GameLibraryItem {
    game_id: String,
    total_rules: usize,
    enabled_rules: usize,
    confirmed_path_count: usize,
    last_rule_updated_at: String,
    preferred_exe_path: Option<String>,
    last_session_id: Option<String>,
    last_session_status: Option<String>,
    last_session_updated_at: Option<String>,
    last_injection_status: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BackupVersion {
    version_id: String,
    created_at: String,
    file_count: usize,
    label: String,
    restorable: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RestoreBackupResult {
    game_id: String,
    version_id: String,
    restored_files: usize,
    pre_restore_version_id: Option<String>,
    verified_files: usize,
    hash_sample_count: usize,
}

struct RestoreVerificationSummary {
    verified_files: usize,
    hash_sample_count: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BackupStatsResult {
    game_id: String,
    game_uid: String,
    total_bytes: u64,
    version_count: usize,
    latest_version_id: Option<String>,
    keep_versions: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PruneBackupResult {
    game_id: String,
    game_uid: String,
    keep_versions: usize,
    deleted_versions: usize,
    freed_bytes: u64,
    remaining_versions: usize,
    remaining_bytes: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RuleConflictItem {
    exe_hash: String,
    rule_ids: Vec<String>,
    game_ids: Vec<String>,
    primary_rule_id: Option<String>,
    conflict_count: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackupManifestFileItem {
    path: String,
    size: u64,
    sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackupManifest {
    format: String,
    created_at: String,
    game_uid: String,
    #[serde(default)]
    files: Vec<BackupManifestFileItem>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LaunchPrecheckCheck {
    key: String,
    label: String,
    ok: bool,
    detail: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GameLaunchPrecheck {
    game_id: String,
    preferred_exe_path: Option<String>,
    exe_hash: Option<String>,
    matched_rule_id: Option<String>,
    backup_ready: bool,
    sandbox_ready: bool,
    inject_ready: bool,
    checks: Vec<LaunchPrecheckCheck>,
    checked_at: String,
}

struct RedirectArtifacts {
    injector_path: PathBuf,
    dll_path: PathBuf,
}

struct InjectionRunResult {
    injector_exit_code: i32,
    hook_version: String,
}

struct SandboxLaunchResult {
    pid: Option<u32>,
    box_name: String,
    mirror_paths: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportRuleInput {
    rule_id: Option<String>,
    game_id: String,
    game_uid: Option<String>,
    exe_hash: String,
    confirmed_paths: Vec<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
    confidence: Option<i64>,
    enabled: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CimProcessRow {
    process_id: u32,
    parent_process_id: u32,
}

#[derive(Clone)]
struct EventCaptureHandle {
    trace_name: String,
    etl_path: PathBuf,
}

#[derive(Default)]
struct CandidateAccumulator {
    path: String,
    added_files: usize,
    modified_files: usize,
    changed_files: usize,
    time_hits: usize,
    extension_hits: usize,
    weak_extension_hits: usize,
    keyword_hits: usize,
    game_name_hits: usize,
    filename_hits: usize,
    reasonable_size_hits: usize,
    user_save_root_hits: usize,
    game_dir_hits: usize,
    noise_hits: usize,
    noise_filename_hits: usize,
    signals: HashSet<String>,
}

#[derive(Default)]
struct GameLibraryAccumulator {
    game_id: String,
    game_uid: String,
    total_rules: usize,
    enabled_rules: usize,
    confirmed_path_count: usize,
    last_rule_updated_at: String,
    last_rule_ts: u64,
    last_session_id: Option<String>,
    last_session_status: Option<String>,
    last_session_updated_at: Option<String>,
    last_session_ts: u64,
    last_injection_status: Option<String>,
    preferred_exe_path: Option<String>,
}

impl CandidateAccumulator {
    fn into_candidate(self) -> CandidatePath {
        let mut score = 0;
        if self.time_hits > 0 {
            score += SCORE_TIME_MATCH;
        }
        if self.extension_hits > 0 {
            score += SCORE_EXTENSION_MATCH;
        }
        if self.weak_extension_hits > 0 {
            score += SCORE_WEAK_EXTENSION_MATCH;
        }
        if self.keyword_hits > 0 {
            score += SCORE_KEYWORD_MATCH;
        }
        if self.game_name_hits > 0 {
            score += SCORE_GAME_NAME_MATCH;
        }
        if self.filename_hits > 0 {
            score += SCORE_FILENAME_MATCH;
        }
        if (1..=50).contains(&self.changed_files) {
            score += SCORE_CHANGE_COUNT_MATCH;
        }
        if self.added_files > 0 {
            score += SCORE_ADDED_FILE_MATCH;
        }
        if self.user_save_root_hits > 0 {
            score += SCORE_USER_SAVE_ROOT_MATCH;
        }
        if self.game_dir_hits > 0 {
            score += SCORE_GAME_DIR_MATCH;
        }
        if self.reasonable_size_hits > 0 {
            score += SCORE_SIZE_REASONABLE;
        }
        if self.noise_hits > 0 {
            score -= SCORE_NOISE_PATH_PENALTY;
        }
        if self.noise_filename_hits > 0 {
            score -= SCORE_NOISE_FILENAME_PENALTY;
        }
        if self.changed_files > 200 {
            score -= SCORE_TOO_MANY_CHANGES_PENALTY;
        }
        if self.weak_extension_hits > 0 && self.extension_hits == 0 && self.keyword_hits == 0 {
            score -= SCORE_WEAK_ONLY_PENALTY;
        }
        score = score.max(0);

        let mut signals = self.signals.into_iter().collect::<Vec<_>>();
        signals.sort();
        let effective_signal_count = [
            self.extension_hits > 0,
            self.keyword_hits > 0,
            self.game_name_hits > 0,
            self.filename_hits > 0,
            self.user_save_root_hits > 0,
            self.game_dir_hits > 0,
            self.added_files > 0,
        ]
        .into_iter()
        .filter(|hit| *hit)
        .count();
        let noisy = self.noise_hits > 0 || self.noise_filename_hits > 0 || self.changed_files > 200;
        let recommendation = if self.time_hits > 0
            && self.keyword_hits > 0
            && (self.extension_hits > 0 || self.game_name_hits > 0)
            && !noisy
        {
            "strong"
        } else if self.time_hits > 0 && effective_signal_count >= 2 && !noisy {
            "recommended"
        } else if (self.time_hits > 0 && effective_signal_count >= 1) || score >= LOW_CONFIDENCE_THRESHOLD {
            "possible"
        } else {
            "weak"
        };

        CandidatePath {
            path: self.path,
            score,
            changed_files: self.changed_files,
            added_files: self.added_files,
            modified_files: self.modified_files,
            matched_signals: signals,
            recommendation: recommendation.to_string(),
            collapsed: score < LOW_CONFIDENCE_THRESHOLD,
        }
    }
}

#[tauri::command]
fn start_learning(
    app: AppHandle,
    state: State<AppState>,
    game_id: String,
    exe_path: String,
) -> Result<String, String> {
    if game_id.trim().is_empty() {
        return Err("gameId 不能为空".to_string());
    }
    if exe_path.trim().is_empty() {
        return Err("exePath 不能为空".to_string());
    }
    if !Path::new(&exe_path).exists() {
        return Err("exePath 不存在".to_string());
    }

    let session_id = Uuid::new_v4().to_string();
    let snapshot_ref = format!("baseline_{session_id}.json");
    let snapshot = collect_snapshot(&game_id, &exe_path)?;
    write_snapshot(&app, &snapshot_ref, &snapshot)?;

    let session = LearningSession {
        session_id: session_id.clone(),
        game_id,
        exe_path,
        started_at: now_iso_string(),
        ended_at: None,
        status: "running".to_string(),
        baseline_snapshot_ref: snapshot_ref,
        final_snapshot_ref: None,
        candidates: vec![],
        pid: None,
        tracked_pids: vec![],
        event_capture_mode: "snapshot".to_string(),
        event_trace_name: None,
        event_trace_path: None,
        captured_event_count: 0,
        event_capture_error: None,
    };

    let mut store = state.store.lock().map_err(|_| "无法锁定应用状态".to_string())?;
    store.sessions.push(session);
    persist_store(&app, &store)?;
    Ok(session_id)
}

#[tauri::command]
fn launch_game(app: AppHandle, state: State<AppState>, session_id: String) -> Result<u32, String> {
    let mut store = state.store.lock().map_err(|_| "无法锁定应用状态".to_string())?;
    let session = store
        .sessions
        .iter_mut()
        .find(|item| item.session_id == session_id)
        .ok_or_else(|| "sessionId 不存在".to_string())?;

    let child = Command::new(&session.exe_path)
        .spawn()
        .map_err(|err| format!("无法启动游戏: {err}"))?;

    let pid = child.id();
    session.pid = Some(pid);
    session.tracked_pids = collect_process_tree_pids(pid).unwrap_or_else(|_| vec![pid]);
    match try_start_etw_capture(&app, &session.session_id) {
        Ok(handle) => {
            session.event_capture_mode = "etw".to_string();
            session.event_trace_name = Some(handle.trace_name);
            session.event_trace_path = Some(handle.etl_path.to_string_lossy().to_string());
            session.event_capture_error = None;
        }
        Err(err) => {
            session.event_capture_mode = "snapshot".to_string();
            session.event_trace_name = None;
            session.event_trace_path = None;
            session.event_capture_error = Some(err);
        }
    }
    persist_store(&app, &store)?;
    Ok(pid)
}

fn finish_learning_impl(
    app: &AppHandle,
    app_state: &AppState,
    session_id: &str,
) -> Result<Vec<CandidatePath>, String> {
    let mut store = app_state
        .store
        .lock()
        .map_err(|_| "无法锁定应用状态".to_string())?;
    let session = store
        .sessions
        .iter_mut()
        .find(|item| item.session_id == session_id)
        .ok_or_else(|| "sessionId 不存在".to_string())?;
    if let Some(root_pid) = session.pid {
        if let Ok(pids) = collect_process_tree_pids(root_pid) {
            session.tracked_pids = pids;
        }
    }

    if session.status != "running" {
        return Ok(session.candidates.clone());
    }

    let end_unix = now_unix();
    let final_snapshot_ref = format!("final_{session_id}.json");
    let final_snapshot = collect_snapshot(&session.game_id, &session.exe_path)?;
    write_snapshot(app, &final_snapshot_ref, &final_snapshot)?;

    let baseline_snapshot: Snapshot = read_snapshot(app, &session.baseline_snapshot_ref)?;
    let related_files = match collect_related_files_by_trace(
        session.event_trace_name.as_deref(),
        session.event_trace_path.as_deref(),
        &session.tracked_pids,
    ) {
        Ok(files) => {
            session.captured_event_count = files.len();
            files
        }
        Err(err) => {
            session.captured_event_count = 0;
            if session.event_capture_mode == "etw" {
                session.event_capture_error = Some(err);
            }
            HashSet::new()
        }
    };
    let candidates = build_candidates(
        &baseline_snapshot,
        &final_snapshot,
        &session.game_id,
        &session.exe_path,
        iso_to_unix(&session.started_at).unwrap_or(end_unix),
        end_unix + 120,
        Some(&related_files),
    );

    session.status = "finished".to_string();
    session.ended_at = Some(now_iso_string());
    session.final_snapshot_ref = Some(final_snapshot_ref);
    session.candidates = candidates.clone();

    persist_store(app, &store)?;
    Ok(candidates)
}

fn update_background_task(
    app: &AppHandle,
    task_id: &str,
    status: &str,
    progress: Option<u8>,
    message: Option<String>,
    result: Option<serde_json::Value>,
    error: Option<String>,
) {
    let app_state: State<AppState> = app.state();
    let mut tasks = match app_state.tasks.lock() {
        Ok(guard) => guard,
        Err(_) => return,
    };
    if let Some(task) = tasks.get_mut(task_id) {
        task.status = status.to_string();
        task.progress = progress;
        task.message = message;
        if let Some(payload) = result {
            task.result = Some(payload);
        }
        if status == "success" {
            task.error = None;
        } else if let Some(err) = error {
            task.error = Some(err);
        }
        task.updated_at = now_iso_string();
    }
}

#[tauri::command]
fn start_finish_learning_task(
    app: AppHandle,
    state: State<AppState>,
    session_id: String,
) -> Result<String, String> {
    let trimmed_session_id = session_id.trim().to_string();
    if trimmed_session_id.is_empty() {
        return Err("sessionId 不能为空".to_string());
    }
    {
        let store = state
            .store
            .lock()
            .map_err(|_| "无法锁定应用状态".to_string())?;
        if !store
            .sessions
            .iter()
            .any(|item| item.session_id == trimmed_session_id)
        {
            return Err("sessionId 不存在".to_string());
        }
    }

    let task_id = Uuid::new_v4().to_string();
    let now = now_iso_string();
    let task = BackgroundTask {
        task_id: task_id.clone(),
        task_type: "finish_learning".to_string(),
        status: "pending".to_string(),
        progress: Some(0),
        message: Some("任务已创建，等待执行".to_string()),
        result: None,
        error: None,
        started_at: now.clone(),
        updated_at: now,
    };
    {
        let mut tasks = state
            .tasks
            .lock()
            .map_err(|_| "无法锁定任务状态".to_string())?;
        tasks.insert(task_id.clone(), task);
    }

    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    let session_id_for_thread = trimmed_session_id.clone();
    std::thread::spawn(move || {
        update_background_task(
            &app_handle,
            &task_id_for_thread,
            "running",
            Some(15),
            Some("正在分析存档变化...".to_string()),
            None,
            None,
        );
        let app_state: State<AppState> = app_handle.state();
        match finish_learning_impl(&app_handle, app_state.inner(), &session_id_for_thread) {
            Ok(candidates) => {
                update_background_task(
                    &app_handle,
                    &task_id_for_thread,
                    "success",
                    Some(100),
                    Some(format!("分析完成，共发现 {} 个候选目录", candidates.len())),
                    serde_json::to_value(candidates).ok(),
                    None,
                );
            }
            Err(err) => {
                update_background_task(
                    &app_handle,
                    &task_id_for_thread,
                    "failed",
                    Some(100),
                    Some("分析失败".to_string()),
                    None,
                    Some(err),
                );
            }
        }
    });

    Ok(task_id)
}

#[tauri::command]
fn get_task(state: State<AppState>, task_id: String) -> Result<BackgroundTask, String> {
    let tasks = state
        .tasks
        .lock()
        .map_err(|_| "无法锁定任务状态".to_string())?;
    tasks
        .get(task_id.trim())
        .cloned()
        .ok_or_else(|| "taskId 不存在".to_string())
}

#[tauri::command]
fn finish_learning(
    app: AppHandle,
    state: State<AppState>,
    session_id: String,
) -> Result<Vec<CandidatePath>, String> {
    finish_learning_impl(&app, state.inner(), &session_id)
}

#[tauri::command]
fn confirm_rule(
    app: AppHandle,
    state: State<AppState>,
    session_id: String,
    selected_paths: Vec<String>,
) -> Result<String, String> {
    let mut store = state.store.lock().map_err(|_| "无法锁定应用状态".to_string())?;
    let session = store
        .sessions
        .iter()
        .find(|item| item.session_id == session_id)
        .ok_or_else(|| "sessionId 不存在".to_string())?;

    let session_game_id = session.game_id.clone();
    let session_exe_path = session.exe_path.clone();
    let normalized_paths = normalize_paths(selected_paths, Some(&session_exe_path));
    if normalized_paths.is_empty() {
        return Err("selectedPaths 不能为空".to_string());
    }
    let exe_hash = file_sha256_hex(Path::new(&session_exe_path))?;
    let rule_key = build_rule_key(&session_game_id, &exe_hash);
    let mut score_map: HashMap<String, i64> = HashMap::new();
    for item in &session.candidates {
        let normalized = normalize_confirmed_path_for_storage(&item.path, Some(&session_exe_path));
        if normalized.is_empty() {
            continue;
        }
        let entry = score_map.entry(normalized).or_insert(item.score);
        if item.score > *entry {
            *entry = item.score;
        }
    }

    let mut confidence_sum = 0_i64;
    let mut counted = 0_i64;
    for path in &normalized_paths {
        if let Some(score) = score_map.get(path) {
            confidence_sum += *score;
            counted += 1;
        }
    }
    let confidence = if counted > 0 {
        confidence_sum / counted
    } else {
        LOW_CONFIDENCE_THRESHOLD
    };

    if let Some(existing) = store
        .rules
        .iter_mut()
        .find(|rule| build_rule_key(&rule.game_id, &rule.exe_hash) == rule_key)
    {
        if existing.game_uid.trim().is_empty() {
            existing.game_uid = new_game_uid();
        }
        existing.confirmed_paths = normalized_paths;
        existing.confidence = confidence;
        existing.updated_at = now_iso_string();
        let rule_id = existing.rule_id.clone();
        persist_store(&app, &store)?;
        return Ok(rule_id);
    }

    let rule_id = Uuid::new_v4().to_string();
    let now = now_iso_string();
    store.rules.push(GameSaveRule {
        rule_id: rule_id.clone(),
        game_id: session_game_id,
        game_uid: new_game_uid(),
        exe_hash,
        confirmed_paths: normalized_paths,
        created_at: now.clone(),
        confidence,
        enabled: true,
        updated_at: now,
    });

    persist_store(&app, &store)?;
    Ok(rule_id)
}

#[tauri::command]
fn list_rules(state: State<AppState>) -> Result<Vec<GameSaveRule>, String> {
    let mut rules = state
        .store
        .lock()
        .map_err(|_| "无法锁定应用状态".to_string())?
        .rules
        .clone();
    rules.sort_by(|a, b| {
        let a_time = if a.updated_at.is_empty() { &a.created_at } else { &a.updated_at };
        let b_time = if b.updated_at.is_empty() { &b.created_at } else { &b.updated_at };
        b_time.cmp(a_time)
    });
    Ok(rules)
}

#[tauri::command]
fn list_rule_conflicts(state: State<AppState>) -> Result<Vec<RuleConflictItem>, String> {
    let store = state.store.lock().map_err(|_| "无法锁定应用状态".to_string())?;
    Ok(build_rule_conflicts(&store))
}

#[tauri::command]
fn set_primary_rule(app: AppHandle, state: State<AppState>, rule_id: String) -> Result<GameSaveRule, String> {
    let mut store = state.store.lock().map_err(|_| "无法锁定应用状态".to_string())?;
    let target_rule = store
        .rules
        .iter()
        .find(|rule| rule.rule_id == rule_id)
        .cloned()
        .ok_or_else(|| "ruleId 不存在".to_string())?;

    let normalized_hash = normalize_exe_hash(&target_rule.exe_hash);
    if normalized_hash.is_empty() {
        return Err("该规则 exeHash 为空，无法设为主规则".to_string());
    }

    store
        .execution_config
        .preferred_rule_id_by_exe_hash
        .insert(normalized_hash, target_rule.rule_id.clone());

    let normalized_game_key = normalize_game_key(&target_rule.game_id);
    let normalized_game_uid = normalize_game_uid(&target_rule.game_uid);
    if !normalized_game_key.is_empty() && !normalized_game_uid.is_empty() {
        store
            .execution_config
            .preferred_rule_uid_by_game
            .insert(normalized_game_key, normalized_game_uid);
    }

    normalize_store(&mut store);
    persist_store(&app, &store)?;
    store
        .rules
        .iter()
        .find(|rule| rule.rule_id == rule_id)
        .cloned()
        .ok_or_else(|| "ruleId 不存在".to_string())
}

#[tauri::command]
fn update_rule(
    app: AppHandle,
    state: State<AppState>,
    rule_id: String,
    game_id: String,
    confirmed_paths: Vec<String>,
    enabled: bool,
) -> Result<GameSaveRule, String> {
    let normalized_game_id = game_id.trim().to_string();
    if normalized_game_id.is_empty() {
        return Err("gameId 不能为空".to_string());
    }

    let mut store = state.store.lock().map_err(|_| "无法锁定应用状态".to_string())?;
    let rule_context = store
        .rules
        .iter()
        .find(|item| item.rule_id == rule_id)
        .cloned()
        .ok_or_else(|| "ruleId 不存在".to_string())?;
    let exe_context = {
        let normalized_uid = normalize_game_uid(&rule_context.game_uid);
        if normalized_uid.is_empty() {
            None
        } else {
            store
                .execution_config
                .preferred_exe_by_uid
                .get(&normalized_uid)
                .map(|value| value.as_str())
        }
    };
    let normalized_paths = normalize_paths(confirmed_paths, exe_context);
    if normalized_paths.is_empty() {
        return Err("confirmedPaths 至少需要一条有效路径".to_string());
    }
    {
        let rule = store
            .rules
            .iter_mut()
            .find(|item| item.rule_id == rule_id)
            .ok_or_else(|| "ruleId 不存在".to_string())?;
        rule.game_id = normalized_game_id;
        rule.confirmed_paths = normalized_paths;
        rule.enabled = enabled;
        rule.updated_at = now_iso_string();
    }
    normalize_store(&mut store);
    let updated = store
        .rules
        .iter()
        .find(|item| item.rule_id == rule_id)
        .cloned()
        .ok_or_else(|| "ruleId 不存在".to_string())?;

    persist_store(&app, &store)?;
    Ok(updated)
}

#[tauri::command]
fn delete_rule(app: AppHandle, state: State<AppState>, rule_id: String) -> Result<(), String> {
    let mut store = state.store.lock().map_err(|_| "无法锁定应用状态".to_string())?;
    let before = store.rules.len();
    store.rules.retain(|item| item.rule_id != rule_id);
    if store.rules.len() == before {
        return Err("ruleId 不存在".to_string());
    }
    normalize_store(&mut store);
    persist_store(&app, &store)?;
    Ok(())
}

#[tauri::command]
fn export_rules(state: State<AppState>, file_path: String) -> Result<ExportRulesResult, String> {
    if file_path.trim().is_empty() {
        return Err("filePath 不能为空".to_string());
    }
    let rules = state
        .store
        .lock()
        .map_err(|_| "无法锁定应用状态".to_string())?
        .rules
        .clone();
    let content =
        serde_json::to_string_pretty(&rules).map_err(|err| format!("序列化导出文件失败: {err}"))?;
    fs::write(&file_path, content).map_err(|err| format!("写入导出文件失败: {err}"))?;
    Ok(ExportRulesResult { count: rules.len() })
}

#[tauri::command]
fn import_rules(
    app: AppHandle,
    state: State<AppState>,
    file_path: String,
) -> Result<ImportRulesResult, String> {
    if file_path.trim().is_empty() {
        return Err("filePath 不能为空".to_string());
    }
    let raw = fs::read(&file_path).map_err(|err| format!("读取导入文件失败: {err}"))?;
    let content = decode_text_bytes(&raw);
    let value: serde_json::Value =
        serde_json::from_str(&content).map_err(|err| format!("解析导入文件失败: {err}"))?;
    let array = value
        .as_array()
        .ok_or_else(|| "导入文件必须是规则数组(JSON Array)".to_string())?;

    let mut store = state.store.lock().map_err(|_| "无法锁定应用状态".to_string())?;
    let summary = apply_import_rules_array(&mut store, array);

    normalize_store(&mut store);
    persist_store(&app, &store)?;
    Ok(summary)
}

fn export_migration_zip_impl<F>(
    app_state: &AppState,
    file_path: &str,
    mut on_progress: F,
) -> Result<ExportMigrationZipResult, String>
where
    F: FnMut(u8, String),
{
    let target_path = file_path.trim().to_string();
    if target_path.is_empty() {
        return Err("filePath 不能为空".to_string());
    }
    on_progress(5, "正在读取规则与备份配置...".to_string());
    let (rules, backup_root) = {
        let store = app_state
            .store
            .lock()
            .map_err(|_| "无法锁定应用状态".to_string())?;
        (store.rules.clone(), store.execution_config.backup_root.clone())
    };
    on_progress(
        12,
        format!("已加载 {} 条规则，正在准备迁移目录...", rules.len()),
    );

    let temp_root = create_migration_temp_dir("export")?;
    let result = (|| -> Result<ExportMigrationZipResult, String> {
        let rules_dir = temp_root.join("rules");
        let backups_dir = temp_root.join("backups");
        let meta_dir = temp_root.join("meta");
        fs::create_dir_all(&rules_dir).map_err(|err| format!("创建导出目录失败: {err}"))?;
        fs::create_dir_all(&backups_dir).map_err(|err| format!("创建导出目录失败: {err}"))?;
        fs::create_dir_all(&meta_dir).map_err(|err| format!("创建导出目录失败: {err}"))?;
        on_progress(22, "正在写入规则与索引文件...".to_string());

        let rules_path = rules_dir.join("gamesaver-rules.json");
        write_pretty_json_file(&rules_path, &rules)?;

        let game_index = build_migration_game_index(&rules);
        let game_index_path = meta_dir.join("game-index.json");
        write_pretty_json_file(&game_index_path, &game_index)?;

        let mut processed_uids = HashSet::new();
        let mut backup_games = 0_usize;
        let mut skipped_backup_games = 0_usize;
        let total_rules = rules.len().max(1);
        for (index, rule) in rules.iter().enumerate() {
            let game_uid = normalize_game_uid(&rule.game_uid);
            if game_uid.is_empty() {
                skipped_backup_games += 1;
                let progress = 30 + (((index + 1) * 45) / total_rules) as u8;
                on_progress(
                    progress,
                    format!("正在整理备份目录 ({}/{})...", index + 1, rules.len()),
                );
                continue;
            }
            if !processed_uids.insert(game_uid.clone()) {
                let progress = 30 + (((index + 1) * 45) / total_rules) as u8;
                on_progress(
                    progress,
                    format!("正在整理备份目录 ({}/{})...", index + 1, rules.len()),
                );
                continue;
            }

            let uid_root = backup_game_root(&backup_root, &game_uid);
            let legacy_root = legacy_backup_game_root(&backup_root, &rule.game_id);
            let source_root = if uid_root.exists() {
                Some(uid_root)
            } else if legacy_root.exists() {
                Some(legacy_root)
            } else {
                None
            };

            if let Some(source_root) = source_root {
                let copied = sync_directory(&source_root, &backups_dir.join(&game_uid))?;
                if copied > 0 {
                    backup_games += 1;
                } else {
                    skipped_backup_games += 1;
                }
            } else {
                skipped_backup_games += 1;
            }
            let progress = 30 + (((index + 1) * 45) / total_rules) as u8;
            on_progress(
                progress,
                format!("正在整理备份目录 ({}/{})...", index + 1, rules.len()),
            );
        }
        if rules.is_empty() {
            on_progress(75, "未检测到规则，正在写入清单...".to_string());
        }

        let manifest = serde_json::json!({
            "format": "gamesaver-migration-v1",
            "createdAt": now_iso_string(),
            "ruleCount": rules.len(),
            "backupGames": backup_games
        });
        write_pretty_json_file(&temp_root.join("manifest.json"), &manifest)?;
        on_progress(84, "正在压缩迁移包，请稍候...".to_string());

        let exported_files = zip_directory_contents(&temp_root, Path::new(&target_path))?;
        on_progress(
            97,
            format!("压缩完成，共 {} 个文件，正在收尾...", exported_files),
        );
        Ok(ExportMigrationZipResult {
            rule_count: rules.len(),
            backup_games,
            exported_files,
            skipped_backup_games,
        })
    })();

    let _ = fs::remove_dir_all(&temp_root);
    if result.is_ok() {
        on_progress(100, "迁移包导出完成".to_string());
    }
    result
}

#[tauri::command]
fn start_export_migration_zip_task(
    app: AppHandle,
    state: State<AppState>,
    file_path: String,
) -> Result<String, String> {
    let trimmed_file_path = file_path.trim().to_string();
    if trimmed_file_path.is_empty() {
        return Err("filePath 不能为空".to_string());
    }

    let task_id = Uuid::new_v4().to_string();
    let now = now_iso_string();
    let task = BackgroundTask {
        task_id: task_id.clone(),
        task_type: "export_migration_zip".to_string(),
        status: "pending".to_string(),
        progress: Some(0),
        message: Some("任务已创建，等待执行".to_string()),
        result: None,
        error: None,
        started_at: now.clone(),
        updated_at: now,
    };
    {
        let mut tasks = state
            .tasks
            .lock()
            .map_err(|_| "无法锁定任务状态".to_string())?;
        tasks.insert(task_id.clone(), task);
    }

    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        update_background_task(
            &app_handle,
            &task_id_for_thread,
            "running",
            Some(3),
            Some("准备导出迁移包...".to_string()),
            None,
            None,
        );
        let app_state: State<AppState> = app_handle.state();
        match export_migration_zip_impl(app_state.inner(), &trimmed_file_path, |progress, message| {
            update_background_task(
                &app_handle,
                &task_id_for_thread,
                "running",
                Some(progress),
                Some(message),
                None,
                None,
            )
        }) {
            Ok(summary) => {
                update_background_task(
                    &app_handle,
                    &task_id_for_thread,
                    "success",
                    Some(100),
                    Some(format!(
                        "导出完成：规则 {} 条，备份游戏 {} 个，文件 {} 个",
                        summary.rule_count, summary.backup_games, summary.exported_files
                    )),
                    serde_json::to_value(summary).ok(),
                    None,
                );
            }
            Err(err) => {
                update_background_task(
                    &app_handle,
                    &task_id_for_thread,
                    "failed",
                    Some(100),
                    Some("导出迁移包失败".to_string()),
                    None,
                    Some(err),
                );
            }
        }
    });

    Ok(task_id)
}

#[tauri::command]
fn export_migration_zip(
    state: State<AppState>,
    file_path: String,
) -> Result<ExportMigrationZipResult, String> {
    export_migration_zip_impl(state.inner(), &file_path, |_progress, _message| {})
}

#[tauri::command]
fn import_migration_zip(
    app: AppHandle,
    state: State<AppState>,
    file_path: String,
) -> Result<ImportMigrationZipResult, String> {
    import_migration_zip_impl(&app, state.inner(), &file_path, |_progress, _message| {})
}

fn import_migration_zip_impl<F>(
    app: &AppHandle,
    app_state: &AppState,
    file_path: &str,
    mut on_progress: F,
) -> Result<ImportMigrationZipResult, String>
where
    F: FnMut(u8, String),
{
    let source_path = file_path.trim().to_string();
    if source_path.is_empty() {
        return Err("filePath 不能为空".to_string());
    }
    on_progress(5, "正在读取迁移包...".to_string());

    let temp_root = create_migration_temp_dir("import")?;
    let result = (|| -> Result<ImportMigrationZipResult, String> {
        on_progress(15, "正在解压迁移包...".to_string());
        unzip_archive_to_directory(Path::new(&source_path), &temp_root)?;

        on_progress(28, "正在读取规则文件...".to_string());
        let rules_file_path = resolve_import_rules_path(&temp_root)
            .ok_or_else(|| "迁移包缺少 rules/gamesaver-rules.json".to_string())?;
        let raw_rules = fs::read(&rules_file_path).map_err(|err| format!("读取规则文件失败: {err}"))?;
        let rules_text = decode_text_bytes(&raw_rules);
        let rules_value: serde_json::Value =
            serde_json::from_str(&rules_text).map_err(|err| format!("解析规则文件失败: {err}"))?;
        let rules_array = rules_value
            .as_array()
            .ok_or_else(|| "迁移包规则文件格式错误：必须是数组".to_string())?;

        on_progress(40, "正在校验并合并规则...".to_string());
        let (summary, backup_root, staged_store) = {
            let store = app_state
                .store
                .lock()
                .map_err(|_| "无法锁定应用状态".to_string())?;
            let mut staged = store.clone();
            let summary = apply_import_rules_array(&mut staged, rules_array);
            normalize_store(&mut staged);
            let backup_root = staged.execution_config.backup_root.clone();
            (summary, backup_root, staged)
        };

        let backups_root = temp_root.join("backups");
        let mut imported_backup_games = 0_usize;
        let mut copied_backup_files = 0_usize;
        let mut skipped_backup_games = 0_usize;
        if backups_root.exists() {
            let entries = fs::read_dir(&backups_root).map_err(|err| format!("读取备份目录失败: {err}"))?;
            let mut entry_list = Vec::new();
            for entry in entries {
                match entry {
                    Ok(value) => entry_list.push(value),
                    Err(_) => skipped_backup_games += 1,
                }
            }
            let total_entries = entry_list.len().max(1);
            for (index, entry) in entry_list.into_iter().enumerate() {
                let progress = 50 + (((index + 1) * 38) / total_entries) as u8;
                let file_type = entry.file_type().map_err(|err| format!("读取备份条目失败: {err}"))?;
                if !file_type.is_dir() {
                    skipped_backup_games += 1;
                    on_progress(
                        progress,
                        format!("正在导入备份目录 ({}/{})...", index + 1, total_entries),
                    );
                    continue;
                }
                let game_uid = normalize_game_uid(&entry.file_name().to_string_lossy());
                if game_uid.is_empty() {
                    skipped_backup_games += 1;
                    on_progress(
                        progress,
                        format!("正在导入备份目录 ({}/{})...", index + 1, total_entries),
                    );
                    continue;
                }

                let copied = sync_directory(&entry.path(), &backup_game_root(&backup_root, &game_uid))?;
                copied_backup_files += copied;
                imported_backup_games += 1;
                on_progress(
                    progress,
                    format!("正在导入备份目录 ({}/{})...", index + 1, total_entries),
                );
            }
        }

        on_progress(92, "正在写入本地存储...".to_string());
        {
            let mut store = app_state
                .store
                .lock()
                .map_err(|_| "无法锁定应用状态".to_string())?;
            *store = staged_store;
            persist_store(app, &store)?;
        }

        Ok(ImportMigrationZipResult {
            imported_rules: summary.imported,
            overwritten_rules: summary.overwritten,
            skipped_rules: summary.skipped,
            imported_backup_games,
            copied_backup_files,
            skipped_backup_games,
        })
    })();

    let _ = fs::remove_dir_all(&temp_root);
    if result.is_ok() {
        on_progress(100, "迁移包导入完成".to_string());
    }
    result
}

#[tauri::command]
fn start_import_migration_zip_task(
    app: AppHandle,
    state: State<AppState>,
    file_path: String,
) -> Result<String, String> {
    let trimmed_file_path = file_path.trim().to_string();
    if trimmed_file_path.is_empty() {
        return Err("filePath 不能为空".to_string());
    }

    let task_id = Uuid::new_v4().to_string();
    let now = now_iso_string();
    let task = BackgroundTask {
        task_id: task_id.clone(),
        task_type: "import_migration_zip".to_string(),
        status: "pending".to_string(),
        progress: Some(0),
        message: Some("任务已创建，等待执行".to_string()),
        result: None,
        error: None,
        started_at: now.clone(),
        updated_at: now,
    };
    {
        let mut tasks = state
            .tasks
            .lock()
            .map_err(|_| "无法锁定任务状态".to_string())?;
        tasks.insert(task_id.clone(), task);
    }

    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        update_background_task(
            &app_handle,
            &task_id_for_thread,
            "running",
            Some(3),
            Some("准备导入迁移包...".to_string()),
            None,
            None,
        );
        let app_state: State<AppState> = app_handle.state();
        match import_migration_zip_impl(&app_handle, app_state.inner(), &trimmed_file_path, |progress, message| {
            update_background_task(
                &app_handle,
                &task_id_for_thread,
                "running",
                Some(progress),
                Some(message),
                None,
                None,
            )
        }) {
            Ok(summary) => {
                update_background_task(
                    &app_handle,
                    &task_id_for_thread,
                    "success",
                    Some(100),
                    Some(format!(
                        "导入完成：新增规则 {}，覆盖规则 {}，导入备份游戏 {}",
                        summary.imported_rules, summary.overwritten_rules, summary.imported_backup_games
                    )),
                    serde_json::to_value(summary).ok(),
                    None,
                );
            }
            Err(err) => {
                update_background_task(
                    &app_handle,
                    &task_id_for_thread,
                    "failed",
                    Some(100),
                    Some("导入迁移包失败".to_string()),
                    None,
                    Some(err),
                );
            }
        }
    });

    Ok(task_id)
}

fn apply_import_rules_array(store: &mut PersistedStore, array: &[serde_json::Value]) -> ImportRulesResult {
    let mut imported = 0_usize;
    let mut overwritten = 0_usize;
    let mut skipped = 0_usize;
    for item in array {
        let parsed = match serde_json::from_value::<ImportRuleInput>(item.clone()) {
            Ok(rule) => rule,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };

        let game_id = parsed.game_id.trim().to_string();
        let exe_hash = parsed.exe_hash.trim().to_string();
        let confirmed_paths = normalize_paths(parsed.confirmed_paths, None);
        if game_id.is_empty() || exe_hash.is_empty() || confirmed_paths.is_empty() {
            skipped += 1;
            continue;
        }

        let key = build_rule_key(&game_id, &exe_hash);
        let parsed_game_uid = parsed.game_uid.as_deref().map(normalize_game_uid);
        let created_at = parsed.created_at.unwrap_or_else(now_iso_string);
        let updated_at = parsed
            .updated_at
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| created_at.clone());
        let confidence = parsed.confidence.unwrap_or(LOW_CONFIDENCE_THRESHOLD);
        let enabled = parsed.enabled.unwrap_or(true);

        if let Some(existing) = store
            .rules
            .iter_mut()
            .find(|rule| build_rule_key(&rule.game_id, &rule.exe_hash) == key)
        {
            let next_uid = parsed_game_uid
                .as_ref()
                .filter(|uid| !uid.is_empty())
                .cloned()
                .unwrap_or_else(|| {
                    let current = normalize_game_uid(&existing.game_uid);
                    if current.is_empty() {
                        new_game_uid()
                    } else {
                        current
                    }
                });
            existing.game_id = game_id;
            existing.game_uid = next_uid;
            existing.exe_hash = exe_hash;
            existing.confirmed_paths = confirmed_paths;
            existing.confidence = confidence;
            existing.enabled = enabled;
            existing.created_at = created_at;
            existing.updated_at = updated_at;
            overwritten += 1;
        } else {
            store.rules.push(GameSaveRule {
                rule_id: parsed.rule_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
                game_id,
                game_uid: parsed_game_uid.filter(|uid| !uid.is_empty()).unwrap_or_else(new_game_uid),
                exe_hash,
                confirmed_paths,
                created_at: created_at.clone(),
                confidence,
                enabled,
                updated_at,
            });
            imported += 1;
        }
    }
    ImportRulesResult {
        imported,
        overwritten,
        skipped,
    }
}

fn create_migration_temp_dir(prefix: &str) -> Result<PathBuf, String> {
    let path = std::env::temp_dir()
        .join("gamesaver-migration")
        .join(format!("{}-{}", prefix, Uuid::new_v4()));
    fs::create_dir_all(&path).map_err(|err| format!("创建临时目录失败: {err}"))?;
    Ok(path)
}

fn write_pretty_json_file<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("创建目录失败: {err}"))?;
    }
    let content = serde_json::to_string_pretty(value).map_err(|err| format!("序列化 JSON 失败: {err}"))?;
    fs::write(path, content).map_err(|err| format!("写入文件失败: {err}"))
}

fn build_migration_game_index(rules: &[GameSaveRule]) -> Vec<MigrationGameIndexItem> {
    let mut by_uid: HashMap<String, (String, u64, Vec<String>)> = HashMap::new();
    for rule in rules {
        let game_uid = normalize_game_uid(&rule.game_uid);
        if game_uid.is_empty() {
            continue;
        }
        let ts = rule_updated_ts(rule);
        let entry = by_uid
            .entry(game_uid)
            .or_insert_with(|| (rule.game_id.clone(), ts, Vec::new()));
        if ts >= entry.1 {
            entry.0 = rule.game_id.clone();
            entry.1 = ts;
        }
        entry.2.push(rule.rule_id.clone());
    }
    let mut items = by_uid
        .into_iter()
        .map(|(game_uid, (game_id, _updated_ts, rule_ids))| MigrationGameIndexItem {
            game_uid,
            game_id,
            rule_ids,
        })
        .collect::<Vec<_>>();
    items.sort_by(|a, b| a.game_id.cmp(&b.game_id).then_with(|| a.game_uid.cmp(&b.game_uid)));
    items
}

fn normalize_zip_entry_name(relative_path: &Path) -> Result<String, String> {
    let mut parts = Vec::new();
    for component in relative_path.components() {
        match component {
            Component::Normal(value) => {
                let part = value.to_string_lossy().trim().to_string();
                if part.is_empty() {
                    return Err("ZIP 条目存在空路径片段".to_string());
                }
                parts.push(part);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("ZIP 条目存在非法路径".to_string());
            }
        }
    }
    if parts.is_empty() {
        return Err("ZIP 条目路径为空".to_string());
    }
    Ok(parts.join("/"))
}

fn zip_directory_contents(source_dir: &Path, output_zip_path: &Path) -> Result<usize, String> {
    if !source_dir.exists() {
        return Err("导出目录不存在".to_string());
    }
    if let Some(parent) = output_zip_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|err| format!("创建 ZIP 目录失败: {err}"))?;
        }
    }
    let output_file = fs::File::create(output_zip_path).map_err(|err| format!("创建 ZIP 文件失败: {err}"))?;
    let mut zip_writer = zip::ZipWriter::new(output_file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);

    let mut file_count = 0_usize;
    for entry in WalkDir::new(source_dir).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(source_dir)
            .map_err(|err| format!("生成 ZIP 相对路径失败: {err}"))?;
        let zip_entry_name = normalize_zip_entry_name(relative)?;
        zip_writer
            .start_file(zip_entry_name, options)
            .map_err(|err| format!("写入 ZIP 条目失败: {err}"))?;
        let mut source_file = fs::File::open(entry.path()).map_err(|err| format!("读取文件失败: {err}"))?;
        std::io::copy(&mut source_file, &mut zip_writer).map_err(|err| format!("压缩文件失败: {err}"))?;
        file_count += 1;
    }

    zip_writer.finish().map_err(|err| format!("完成 ZIP 写入失败: {err}"))?;
    Ok(file_count)
}

fn unzip_archive_to_directory(zip_path: &Path, destination: &Path) -> Result<usize, String> {
    if !zip_path.exists() {
        return Err("迁移包不存在".to_string());
    }
    fs::create_dir_all(destination).map_err(|err| format!("创建解压目录失败: {err}"))?;
    let zip_file = fs::File::open(zip_path).map_err(|err| format!("打开迁移包失败: {err}"))?;
    let mut archive = zip::ZipArchive::new(zip_file).map_err(|err| format!("读取 ZIP 失败: {err}"))?;

    let mut extracted_files = 0_usize;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|err| format!("读取 ZIP 条目失败: {err}"))?;
        let entry_name = entry.name().to_string();
        let enclosed = entry
            .enclosed_name()
            .ok_or_else(|| format!("ZIP 包含非法路径: {entry_name}"))?
            .to_path_buf();
        if enclosed
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
        {
            return Err(format!("ZIP 包含非法路径: {entry_name}"));
        }
        let output_path = destination.join(&enclosed);
        if entry.is_dir() {
            fs::create_dir_all(&output_path).map_err(|err| format!("创建目录失败: {err}"))?;
            continue;
        }
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent).map_err(|err| format!("创建目录失败: {err}"))?;
        }
        let mut output_file = fs::File::create(&output_path).map_err(|err| format!("写入解压文件失败: {err}"))?;
        std::io::copy(&mut entry, &mut output_file).map_err(|err| format!("解压文件失败: {err}"))?;
        extracted_files += 1;
    }
    Ok(extracted_files)
}

fn resolve_import_rules_path(temp_root: &Path) -> Option<PathBuf> {
    let candidates = [
        temp_root.join("rules").join("gamesaver-rules.json"),
        temp_root.join("gamesaver-rules.json"),
    ];
    candidates.into_iter().find(|path| path.exists())
}

#[tauri::command]
fn get_runtime_status() -> Result<RuntimeStatus, String> {
    let is_admin = is_running_as_admin();
    let message = if is_admin {
        "当前为管理员模式，可使用 ETW 高精度过滤。".to_string()
    } else {
        "当前非管理员模式：会自动降级到 snapshot 学习。可点击“一键管理员重启”开启 ETW。".to_string()
    };
    Ok(RuntimeStatus {
        is_admin,
        can_use_etw: is_admin,
        message,
    })
}

#[tauri::command]
fn restart_as_admin() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|err| format!("读取当前程序路径失败: {err}"))?;
    let exe_str = exe.to_string_lossy().to_string();
    let mut command = Command::new("powershell");
    command.args([
        "-NoProfile",
        "-Command",
        &format!("Start-Process -FilePath '{}' -Verb RunAs", exe_str.replace('\'', "''")),
    ]);
    let started = apply_background_process_flags(&mut command)
        .status()
        .map_err(|err| format!("请求管理员重启失败: {err}"))?;

    if !started.success() {
        return Err("管理员重启请求未成功执行".to_string());
    }

    std::process::exit(0);
}

#[tauri::command]
fn get_learning_session(
    state: State<AppState>,
    session_id: String,
) -> Result<LearningSession, String> {
    let store = state.store.lock().map_err(|_| "无法锁定应用状态".to_string())?;
    let session = store
        .sessions
        .iter()
        .find(|item| item.session_id == session_id)
        .ok_or_else(|| "sessionId 不存在".to_string())?;
    Ok(session.clone())
}

#[tauri::command]
fn open_candidate_path(path: String) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("path 不能为空".to_string());
    }
    let target = Path::new(&path);
    if !target.exists() {
        return Err("候选目录不存在".to_string());
    }
    if !target.is_dir() {
        return Err("候选路径不是目录".to_string());
    }

    Command::new("explorer")
        .arg(&path)
        .spawn()
        .map_err(|err| format!("打开目录失败: {err}"))?;

    Ok(())
}

#[tauri::command]
fn resolve_rule_for_exe(state: State<AppState>, exe_path: String) -> Result<ResolveRuleResult, String> {
    let trimmed = exe_path.trim();
    if trimmed.is_empty() {
        return Err("exePath 不能为空".to_string());
    }
    let exe = Path::new(trimmed);
    if !exe.exists() {
        return Err("exePath 不存在".to_string());
    }
    if !exe.is_file() {
        return Err("exePath 不是有效文件".to_string());
    }

    let exe_hash = file_sha256_hex(exe)?;
    let store = state.store.lock().map_err(|_| "无法锁定应用状态".to_string())?;
    let matched_rule = store
        .rules
        .iter()
        .find(|rule| rule.enabled && rule.exe_hash.eq_ignore_ascii_case(&exe_hash))
        .cloned();
    Ok(ResolveRuleResult { exe_hash, matched_rule })
}

#[tauri::command]
fn launch_with_rule(
    app: AppHandle,
    state: State<AppState>,
    exe_path: String,
    launch_mode: Option<String>,
) -> Result<LauncherSession, String> {
    launch_with_rule_internal(&app, &state, exe_path, launch_mode, None, false)
}

#[tauri::command]
fn launch_game_from_library(
    app: AppHandle,
    state: State<AppState>,
    game_id: String,
    launch_mode: Option<String>,
) -> Result<LauncherSession, String> {
    let normalized_game_key = normalize_game_key(&game_id);
    if normalized_game_key.is_empty() {
        return Err("gameId 不能为空".to_string());
    }
    let preferred_exe_path = {
        let store = state.store.lock().map_err(|_| "无法锁定应用状态".to_string())?;
        let rule = select_rule_for_game(&store, &game_id)
            .ok_or_else(|| format!("游戏 {} 暂无可用规则，请先学习并保存规则", game_id.trim()))?;
        let game_uid = normalize_game_uid(&rule.game_uid);
        if game_uid.is_empty() {
            return Err(format!("游戏 {} 规则缺少 gameUid，请先刷新或重新导入规则", game_id.trim()));
        }
        store
            .execution_config
            .preferred_exe_by_uid
            .get(&game_uid)
            .cloned()
            .ok_or_else(|| format!("游戏 {} 尚未绑定 EXE，请先点击“选择/更换 EXE”", game_id.trim()))?
    };
    let preferred_exe_path = preferred_exe_path.trim().to_string();
    if preferred_exe_path.is_empty() {
        return Err(format!("游戏 {} 的 EXE 绑定为空，请重新绑定", game_id.trim()));
    }
    let exe = Path::new(&preferred_exe_path);
    if !exe.exists() {
        return Err(format!("已绑定 EXE 不存在：{}，请重新绑定", preferred_exe_path));
    }
    if !exe.is_file() {
        return Err(format!("已绑定路径不是有效 EXE 文件：{}，请重新绑定", preferred_exe_path));
    }
    if !preferred_exe_path.to_ascii_lowercase().ends_with(".exe") {
        return Err(format!("已绑定路径不是 .exe 文件：{}，请重新绑定", preferred_exe_path));
    }
    launch_with_rule_internal(
        &app,
        &state,
        preferred_exe_path,
        launch_mode,
        Some(normalized_game_key),
        true,
    )
}

fn launch_with_rule_internal(
    app: &AppHandle,
    state: &State<AppState>,
    exe_path: String,
    launch_mode: Option<String>,
    expected_game_key: Option<String>,
    require_rule_match: bool,
) -> Result<LauncherSession, String> {
    let launch_mode_value = normalize_launch_mode(launch_mode.as_deref());
    let now = now_iso_string();
    let mut session = LauncherSession {
        launcher_session_id: Uuid::new_v4().to_string(),
        exe_path: exe_path.trim().to_string(),
        exe_hash: String::new(),
        matched_rule_id: None,
        matched_game_id: None,
        matched_game_uid: None,
        launch_mode: launch_mode_value.clone(),
        status: "idle".to_string(),
        pid: None,
        injection_status: "not_required".to_string(),
        redirect_root: None,
        injector_exit_code: None,
        hook_version: None,
        sandbox_box_name: None,
        sandbox_mirror_paths: vec![],
        started_at: now.clone(),
        updated_at: now,
        logs: vec![],
    };
    let mut failed_error: Option<String> = None;

    if session.exe_path.is_empty() {
        failed_error = Some("exePath 不能为空".to_string());
        session.status = "failed".to_string();
        session.injection_status = "failed".to_string();
        append_session_log(&mut session, "启动失败：exePath 为空");
    } else {
        let exe = Path::new(&session.exe_path);
        if !exe.exists() {
            failed_error = Some("exePath 不存在".to_string());
            session.status = "failed".to_string();
            session.injection_status = "failed".to_string();
            append_session_log(&mut session, "启动失败：目标 exe 不存在");
        } else if !exe.is_file() {
            failed_error = Some("exePath 不是有效文件".to_string());
            session.status = "failed".to_string();
            session.injection_status = "failed".to_string();
            append_session_log(&mut session, "启动失败：目标 exe 不是文件");
        } else {
            match file_sha256_hex(exe) {
                Ok(hash) => {
                    session.exe_hash = hash;
                }
                Err(err) => {
                    failed_error = Some(format!("读取 exe 哈希失败: {err}"));
                    session.status = "failed".to_string();
                    session.injection_status = "failed".to_string();
                    append_session_log(&mut session, "启动失败：读取 exe 哈希失败");
                }
            }
        }
    }

    let mut execution_config = default_execution_config();
    let mut matched_rule: Option<GameSaveRule> = None;
    let mut unresolved_primary_conflict = false;
    if failed_error.is_none() {
        let mut hash_matched_any = false;
        match state.store.lock() {
            Ok(store) => {
                execution_config = store.execution_config.clone();
                let (resolved_rule, any_hash_match) = match_enabled_rule_for_exe_hash(
                    &store.rules,
                    &store.execution_config,
                    &session.exe_hash,
                    expected_game_key.as_deref(),
                );
                matched_rule = resolved_rule;
                hash_matched_any = any_hash_match;
                unresolved_primary_conflict = has_unresolved_primary_rule_conflict_for_exe_hash(
                    &store.rules,
                    &store.execution_config,
                    &session.exe_hash,
                    expected_game_key.as_deref(),
                );
            }
            Err(_) => {
                failed_error = Some("无法锁定应用状态".to_string());
                session.status = "failed".to_string();
                session.injection_status = "failed".to_string();
                append_session_log(&mut session, "启动失败：无法读取规则");
            }
        }
        if failed_error.is_none() && require_rule_match && unresolved_primary_conflict {
            session.status = "failed".to_string();
            session.injection_status = "failed".to_string();
            let message = "已阻止启动：当前 EXE 命中多条启用规则，且未设置主规则，请到规则管理将目标规则设为主规则".to_string();
            append_session_log(&mut session, &message);
            failed_error = Some(message);
        }
        if failed_error.is_none() && require_rule_match && matched_rule.is_none() {
            session.status = "failed".to_string();
            session.injection_status = "failed".to_string();
            let message = if let Some(game_key) = expected_game_key.as_ref() {
                if hash_matched_any {
                    format!(
                        "已阻止启动：当前绑定 EXE 与游戏 {} 规则不匹配，请重新绑定正确 EXE",
                        game_key
                    )
                } else {
                    format!("已阻止启动：游戏 {} 未匹配到启用规则，请先完成学习或检查规则状态", game_key)
                }
            } else {
                "已阻止启动：未匹配到启用规则".to_string()
            };
            append_session_log(&mut session, &message);
            failed_error = Some(message);
        }
    }

    if failed_error.is_none() {
        if let Some(rule) = &matched_rule {
            if launch_mode_value == "inject" && !is_x64_pe(Path::new(&session.exe_path)) {
                failed_error = Some("仅支持 x64 目标进程，当前 exe 不是 x64".to_string());
                session.status = "failed".to_string();
                session.injection_status = "failed".to_string();
                append_session_log(&mut session, "执行失败：目标进程架构非 x64");
            }
            session.matched_rule_id = Some(rule.rule_id.clone());
            session.matched_game_id = Some(rule.game_id.clone());
            session.matched_game_uid = Some(rule.game_uid.clone());
            if launch_mode_value == "inject" {
                session.injection_status = "pending".to_string();
                session.redirect_root = Some(join_managed_root_for_game(
                    &execution_config.managed_save_root,
                    &rule.game_id,
                ));
            } else if launch_mode_value == "backup" {
                session.injection_status = "not_required".to_string();
                session.hook_version = Some("backup-auto-v1".to_string());
                session.redirect_root = Some(
                    backup_game_root(&execution_config.backup_root, &rule.game_uid)
                        .to_string_lossy()
                        .to_string(),
                );
            } else {
                session.injection_status = "pending".to_string();
                session.redirect_root = Some(join_managed_root_for_game(
                    &execution_config.managed_save_root,
                    &rule.game_id,
                ));
            }
            append_session_log(
                &mut session,
                &format!("已匹配规则：{} ({})，启动模式={launch_mode_value}", rule.game_id, rule.rule_id),
            );
        } else {
            session.injection_status = "not_required".to_string();
            append_session_log(&mut session, "未匹配到启用规则，将按普通模式启动");
        }
    }

    if failed_error.is_none() && launch_mode_value == "backup" {
        if let Some(rule) = &matched_rule {
            match restore_latest_backup_for_rule(rule, &execution_config.backup_root, Some(&session.exe_path)) {
                Ok(copied) => {
                    append_session_log(
                        &mut session,
                        &format!("启动前自动恢复完成：文件 {}，备份根目录 {}", copied, execution_config.backup_root),
                    );
                }
                Err(err) => {
                    failed_error = Some(format!("自动恢复失败: {err}"));
                    session.status = "failed".to_string();
                    session.injection_status = "failed".to_string();
                    append_session_log(&mut session, &format!("启动前自动恢复失败：{err}"));
                }
            }
        }
    }

    let mut launched_child_for_backup: Option<std::process::Child> = None;
    if failed_error.is_none() {
        session.status = "launching".to_string();
        if launch_mode_value == "sandbox" && matched_rule.is_some() {
            let rule = matched_rule.as_ref().expect("checked is_some");
            let exe_path_for_sandbox = session.exe_path.clone();
            append_session_log(&mut session, "正在以沙盒模式启动目标进程");
            match run_sandbox_launch_flow(&exe_path_for_sandbox, rule, &execution_config, &mut session) {
                Ok(result) => {
                    session.pid = result.pid;
                    session.sandbox_box_name = Some(result.box_name);
                    session.sandbox_mirror_paths = result.mirror_paths;
                    session.hook_version = Some("sandbox-sync-v1".to_string());
                }
                Err(err) => {
                    failed_error = Some(format!("沙盒启动失败: {err}"));
                    session.status = "failed".to_string();
                    session.injection_status = "failed".to_string();
                    append_session_log(&mut session, &format!("沙盒启动失败：{err}"));
                }
            }
        } else {
            append_session_log(&mut session, "正在启动目标进程");
            match Command::new(&session.exe_path).spawn() {
                Ok(child) => {
                    session.pid = Some(child.id());
                    if launch_mode_value == "backup" && matched_rule.is_some() {
                        launched_child_for_backup = Some(child);
                    }
                }
                Err(err) => {
                    failed_error = Some(format!("无法启动游戏: {err}"));
                    session.status = "failed".to_string();
                    session.injection_status = "failed".to_string();
                    append_session_log(&mut session, &format!("启动失败：{err}"));
                }
            }
        }
    }

    if failed_error.is_none() {
        if launch_mode_value == "inject" && matched_rule.is_some() {
            let rule = matched_rule.as_ref().expect("checked is_some");
            let Some(pid) = session.pid else {
                session.status = "failed".to_string();
                session.injection_status = "failed".to_string();
                append_session_log(&mut session, "注入失败：无法获取进程 PID");
                session.updated_at = now_iso_string();
                let mut store = state.store.lock().map_err(|_| "无法锁定应用状态".to_string())?;
                store.launcher_sessions.push(session.clone());
                persist_store(app, &store)?;
                return Err("启动成功但未获取进程 PID".to_string());
            };
            let started = Instant::now();
            match run_real_injection_flow(app, pid, rule, &execution_config, &mut session) {
                Ok(result) => {
                    session.injection_status = "noop_injected".to_string();
                    session.injector_exit_code = Some(result.injector_exit_code);
                    session.hook_version = Some(result.hook_version);
                    append_session_log(
                        &mut session,
                        &format!(
                            "CreateFileW hook installed，耗时 {}ms",
                            started.elapsed().as_millis()
                        ),
                    );
                }
                Err(err) => {
                    session.status = "failed".to_string();
                    session.injection_status = "failed".to_string();
                    append_session_log(&mut session, &format!("注入失败：{err}"));
                    session.injector_exit_code = Some(1);
                    if execution_config.block_on_inject_fail {
                        if let Some(pid) = session.pid {
                            let _ = terminate_process(pid);
                            append_session_log(&mut session, "已按策略终止目标进程（注入失败）");
                        }
                    }
                    failed_error = Some(format!("已阻止启动（注入失败）：{err}"));
                }
            }
        } else if launch_mode_value == "sandbox" && matched_rule.is_some() {
            session.injection_status = "not_required".to_string();
            session.injector_exit_code = Some(0);
            append_session_log(&mut session, "沙盒模式已启动：退出游戏后请点击“回收沙盒存档”");
        } else if launch_mode_value == "backup" && matched_rule.is_some() {
            session.injection_status = "not_required".to_string();
            session.injector_exit_code = Some(0);
            if let Some(child) = launched_child_for_backup.take() {
                if let Some(rule) = matched_rule.clone() {
                    let keep_versions = resolve_backup_keep_versions(&execution_config, &rule.game_uid);
                    spawn_post_exit_backup_worker(
                        app.clone(),
                        session.launcher_session_id.clone(),
                        child,
                        rule,
                        execution_config.backup_root.clone(),
                        keep_versions,
                        session.exe_path.clone(),
                    );
                    append_session_log(
                        &mut session,
                        &format!("已启用退出后自动增量备份（保留最近 {} 版）", keep_versions),
                    );
                }
            } else {
                append_session_log(&mut session, "未获取进程句柄，无法启用自动退出备份");
            }
        }
        if failed_error.is_none() {
            session.status = "running".to_string();
            append_session_log(&mut session, "启动完成");
        }
    }

    session.updated_at = now_iso_string();
    {
        let mut store = state.store.lock().map_err(|_| "无法锁定应用状态".to_string())?;
        store.launcher_sessions.push(session.clone());
        persist_store(app, &store)?;
    }

    if let Some(err) = failed_error {
        Err(err)
    } else {
        Ok(session)
    }
}

#[tauri::command]
fn get_redirect_runtime_info(app: AppHandle, state: State<AppState>) -> Result<RedirectRuntimeInfo, String> {
    let execution_config = {
        let store = state.store.lock().map_err(|_| "无法锁定应用状态".to_string())?;
        store.execution_config.clone()
    };
    let artifacts = resolve_redirector_artifacts(&app);
    Ok(RedirectRuntimeInfo {
        arch: "x64".to_string(),
        injector_path: artifacts.injector_path.to_string_lossy().to_string(),
        dll_path: artifacts.dll_path.to_string_lossy().to_string(),
        managed_save_root: execution_config.managed_save_root.clone(),
        backup_root: execution_config.backup_root.clone(),
        injector_exists: artifacts.injector_path.exists(),
        dll_exists: artifacts.dll_path.exists(),
        sandbox_root: execution_config.sandbox_root.clone(),
        sandboxie_path: execution_config.sandboxie_start_exe.clone(),
        sandboxie_exists: resolve_sandboxie_start_path(&execution_config).is_ok(),
    })
}

#[tauri::command]
fn sync_sandbox_session(
    app: AppHandle,
    state: State<AppState>,
    launcher_session_id: String,
) -> Result<LauncherSession, String> {
    let mut store = state.store.lock().map_err(|_| "无法锁定应用状态".to_string())?;
    let execution_config = store.execution_config.clone();
    let session = store
        .launcher_sessions
        .iter_mut()
        .find(|item| item.launcher_session_id == launcher_session_id)
        .ok_or_else(|| "launcherSessionId 不存在".to_string())?;
    if session.launch_mode != "sandbox" {
        return Err("当前会话不是沙盒模式".to_string());
    }
    let redirect_root = session
        .redirect_root
        .clone()
        .ok_or_else(|| "当前会话缺少 redirectRoot".to_string())?;
    let mut copied_total = 0_usize;
    if session.sandbox_mirror_paths.is_empty() {
        append_session_log(session, "未记录沙盒镜像路径，跳过回收");
    } else {
        for mirror in &session.sandbox_mirror_paths {
            copied_total += sync_directory(Path::new(mirror), Path::new(&redirect_root))?;
        }
    }
    append_session_log(
        session,
        &format!(
            "沙盒存档回收完成，累计文件 {}，统一目录 {}",
            copied_total, execution_config.managed_save_root
        ),
    );
    session.status = "exited".to_string();
    session.updated_at = now_iso_string();
    let updated = session.clone();
    persist_store(&app, &store)?;
    Ok(updated)
}

#[tauri::command]
fn get_backup_stats(state: State<AppState>, game_id: String) -> Result<BackupStatsResult, String> {
    let trimmed_game_id = game_id.trim();
    if trimmed_game_id.is_empty() {
        return Err("gameId 不能为空".to_string());
    }
    let (rule, backup_root, keep_versions) = {
        let store = state.store.lock().map_err(|_| "无法锁定应用状态".to_string())?;
        let rule =
            select_rule_for_game(&store, trimmed_game_id).ok_or_else(|| "未找到该游戏可用规则".to_string())?;
        let keep_versions = resolve_backup_keep_versions(&store.execution_config, &rule.game_uid);
        (rule, store.execution_config.backup_root.clone(), keep_versions)
    };
    build_backup_stats_for_rule(&rule, &backup_root, keep_versions)
}

#[tauri::command]
fn set_backup_keep_versions(
    app: AppHandle,
    state: State<AppState>,
    game_id: String,
    keep_versions: usize,
) -> Result<BackupStatsResult, String> {
    let trimmed_game_id = game_id.trim();
    if trimmed_game_id.is_empty() {
        return Err("gameId 不能为空".to_string());
    }
    if keep_versions == 0 {
        return Err("keepVersions 不能为空且至少为 1".to_string());
    }

    let normalized_keep = normalize_backup_keep_versions(keep_versions);
    let (rule, backup_root) = {
        let mut store = state.store.lock().map_err(|_| "无法锁定应用状态".to_string())?;
        let rule =
            select_rule_for_game(&store, trimmed_game_id).ok_or_else(|| "未找到该游戏可用规则".to_string())?;
        let game_uid = normalize_game_uid(&rule.game_uid);
        if game_uid.is_empty() {
            return Err("当前游戏规则缺少 gameUid".to_string());
        }
        store
            .execution_config
            .backup_keep_versions_by_uid
            .insert(game_uid, normalized_keep);
        let backup_root = store.execution_config.backup_root.clone();
        persist_store(&app, &store)?;
        (rule, backup_root)
    };

    build_backup_stats_for_rule(&rule, &backup_root, normalized_keep)
}

#[tauri::command]
fn prune_backup_versions(
    app: AppHandle,
    state: State<AppState>,
    game_id: String,
    keep_versions: Option<usize>,
) -> Result<PruneBackupResult, String> {
    let trimmed_game_id = game_id.trim();
    if trimmed_game_id.is_empty() {
        return Err("gameId 不能为空".to_string());
    }
    if keep_versions.is_some_and(|value| value == 0) {
        return Err("keepVersions 至少为 1".to_string());
    }

    let (rule, backup_root, keep) = {
        let mut store = state.store.lock().map_err(|_| "无法锁定应用状态".to_string())?;
        let rule =
            select_rule_for_game(&store, trimmed_game_id).ok_or_else(|| "未找到该游戏可用规则".to_string())?;
        let game_uid = normalize_game_uid(&rule.game_uid);
        if game_uid.is_empty() {
            return Err("当前游戏规则缺少 gameUid".to_string());
        }
        let keep = if let Some(value) = keep_versions {
            let normalized = normalize_backup_keep_versions(value);
            store
                .execution_config
                .backup_keep_versions_by_uid
                .insert(game_uid, normalized);
            persist_store(&app, &store)?;
            normalized
        } else {
            resolve_backup_keep_versions(&store.execution_config, &rule.game_uid)
        };
        (rule, store.execution_config.backup_root.clone(), keep)
    };

    let base = ensure_backup_root_for_rule(&rule, &backup_root)?;
    let versions_dir = base.join("versions");
    let version_dirs = list_version_directories(&versions_dir)?;
    let deleted_versions = version_dirs.len().saturating_sub(keep);
    let mut freed_bytes = 0_u64;
    for (_, path) in version_dirs.into_iter().take(deleted_versions) {
        freed_bytes += directory_total_bytes(&path);
        fs::remove_dir_all(&path).map_err(|err| format!("清理旧版本失败: {err}"))?;
    }

    let remaining_versions = list_version_directories(&versions_dir)?.len();
    let remaining_bytes = directory_total_bytes(&base);
    Ok(PruneBackupResult {
        game_id: rule.game_id.clone(),
        game_uid: normalize_game_uid(&rule.game_uid),
        keep_versions: keep,
        deleted_versions,
        freed_bytes,
        remaining_versions,
        remaining_bytes,
    })
}

fn build_backup_stats_for_rule(
    rule: &GameSaveRule,
    backup_root: &str,
    keep_versions: usize,
) -> Result<BackupStatsResult, String> {
    let normalized_keep = normalize_backup_keep_versions(keep_versions);
    let base = ensure_backup_root_for_rule(rule, backup_root)?;
    let versions_dir = base.join("versions");
    let version_dirs = list_version_directories(&versions_dir)?;
    let latest_version_id = version_dirs.last().map(|(version_id, _)| version_id.clone());
    Ok(BackupStatsResult {
        game_id: rule.game_id.clone(),
        game_uid: normalize_game_uid(&rule.game_uid),
        total_bytes: directory_total_bytes(&base),
        version_count: version_dirs.len(),
        latest_version_id,
        keep_versions: normalized_keep,
    })
}

struct RestorePlanItem {
    source_path: String,
    slot_name: String,
    stage_slot: PathBuf,
    rollback_slot: PathBuf,
    target_exists: bool,
}

fn remove_path_if_exists(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    if path.is_dir() {
        fs::remove_dir_all(path).map_err(|err| format!("清理目标目录失败: {err}"))
    } else {
        fs::remove_file(path).map_err(|err| format!("清理目标文件失败: {err}"))
    }
}

fn rollback_applied_restore_plans(plans: &[RestorePlanItem], applied_indices: &[usize]) -> Result<(), String> {
    let mut rollback_errors = Vec::new();
    for idx in applied_indices.iter().rev() {
        let plan = &plans[*idx];
        let target = Path::new(&plan.source_path);
        if let Err(err) = remove_path_if_exists(target) {
            rollback_errors.push(format!("槽位 {} 清理失败: {err}", plan.slot_name));
            continue;
        }
        if plan.target_exists {
            if let Err(err) = sync_directory(&plan.rollback_slot, target) {
                rollback_errors.push(format!("槽位 {} 回滚失败: {err}", plan.slot_name));
            }
        }
    }
    if rollback_errors.is_empty() {
        Ok(())
    } else {
        Err(rollback_errors.join("；"))
    }
}

fn cleanup_restore_temp_dirs(staging_root: &Path, rollback_root: &Path) {
    if staging_root.exists() {
        let _ = fs::remove_dir_all(staging_root);
    }
    if rollback_root.exists() {
        let _ = fs::remove_dir_all(rollback_root);
    }
}

fn restore_backup_version_transactional(
    rule: &GameSaveRule,
    version_dir: &Path,
    base: &Path,
    exe_path: Option<&str>,
) -> Result<usize, String> {
    let tx_id = format!("{}_{}", now_unix(), Uuid::new_v4().simple());
    let staging_root = base.join("_restore_tmp").join(&tx_id);
    let rollback_root = base.join("_restore_rollback").join(&tx_id);
    fs::create_dir_all(&staging_root).map_err(|err| format!("创建恢复临时目录失败: {err}"))?;
    fs::create_dir_all(&rollback_root).map_err(|err| format!("创建回滚临时目录失败: {err}"))?;

    let mut plans = Vec::new();
    for source_path in &rule.confirmed_paths {
        let slot = backup_slot_name(source_path);
        let mut slot_source = version_dir.join(&slot);
        if !slot_source.exists() {
            let legacy_slot = backup_slot_name_legacy(source_path);
            slot_source = version_dir.join(legacy_slot);
        }
        if !slot_source.exists() {
            cleanup_restore_temp_dirs(&staging_root, &rollback_root);
            return Err(format!("备份版本不完整，缺少槽位 {slot}"));
        }

        let stage_slot = staging_root.join(&slot);
        sync_directory(&slot_source, &stage_slot).map_err(|err| {
            cleanup_restore_temp_dirs(&staging_root, &rollback_root);
            format!("准备恢复槽位 {slot} 失败: {err}")
        })?;

        let runtime_path = expand_confirmed_path_for_runtime(source_path, exe_path)?;
        let target = Path::new(&runtime_path);
        let rollback_slot = rollback_root.join(&slot);
        let target_exists = target.exists();
        if target_exists {
            sync_directory(target, &rollback_slot).map_err(|err| {
                cleanup_restore_temp_dirs(&staging_root, &rollback_root);
                format!("创建回滚快照失败（槽位 {slot}）: {err}")
            })?;
        }

        plans.push(RestorePlanItem {
            source_path: runtime_path,
            slot_name: slot,
            stage_slot,
            rollback_slot,
            target_exists,
        });
    }

    let mut restored_files = 0_usize;
    let mut applied_indices = Vec::new();
    for (idx, plan) in plans.iter().enumerate() {
        let target = Path::new(&plan.source_path);
        if let Err(err) = remove_path_if_exists(target) {
            let rollback_result = rollback_applied_restore_plans(&plans, &applied_indices);
            cleanup_restore_temp_dirs(&staging_root, &rollback_root);
            return Err(match rollback_result {
                Ok(_) => format!("恢复失败（槽位 {}），已自动回滚：{err}", plan.slot_name),
                Err(rollback_err) => format!(
                    "恢复失败（槽位 {}），且自动回滚失败：{err}；回滚错误：{rollback_err}",
                    plan.slot_name
                ),
            });
        }
        match sync_directory(&plan.stage_slot, target) {
            Ok(count) => {
                restored_files += count;
                applied_indices.push(idx);
            }
            Err(err) => {
                let rollback_result = rollback_applied_restore_plans(&plans, &applied_indices);
                cleanup_restore_temp_dirs(&staging_root, &rollback_root);
                return Err(match rollback_result {
                    Ok(_) => format!("恢复失败（槽位 {}），已自动回滚：{err}", plan.slot_name),
                    Err(rollback_err) => format!(
                        "恢复失败（槽位 {}），且自动回滚失败：{err}；回滚错误：{rollback_err}",
                        plan.slot_name
                    ),
                });
            }
        }
    }

    cleanup_restore_temp_dirs(&staging_root, &rollback_root);
    Ok(restored_files)
}

#[tauri::command]
fn list_backup_versions(state: State<AppState>, game_id: String) -> Result<Vec<BackupVersion>, String> {
    let trimmed_game_id = game_id.trim();
    if trimmed_game_id.is_empty() {
        return Err("gameId 不能为空".to_string());
    }
    let store = state.store.lock().map_err(|_| "无法锁定应用状态".to_string())?;
    let rule =
        select_rule_for_game(&store, trimmed_game_id).ok_or_else(|| "未找到该游戏可用规则".to_string())?;
    let base = ensure_backup_root_for_rule(&rule, &store.execution_config.backup_root)?;
    let versions_dir = base.join("versions");
    let mut versions = list_version_directories(&versions_dir)?
        .into_iter()
        .map(|(version_id, path)| BackupVersion {
            created_at: version_id.clone(),
            label: backup_version_label(&version_id),
            restorable: !version_id.starts_with("_"),
            file_count: WalkDir::new(path)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|item| item.file_type().is_file())
                .count(),
            version_id,
        })
        .collect::<Vec<_>>();
    versions.sort_by(|a, b| b.version_id.cmp(&a.version_id));
    Ok(versions)
}

#[tauri::command]
fn restore_backup_version(
    app: AppHandle,
    state: State<AppState>,
    game_id: String,
    version_id: String,
) -> Result<RestoreBackupResult, String> {
    restore_backup_version_impl(&app, state.inner(), &game_id, &version_id, |_progress, _message| {})
}

fn restore_backup_version_impl<F>(
    app: &AppHandle,
    app_state: &AppState,
    game_id: &str,
    version_id: &str,
    mut on_progress: F,
) -> Result<RestoreBackupResult, String>
where
    F: FnMut(u8, String),
{
    let trimmed_game_id = game_id.trim().to_string();
    let trimmed_version_id = version_id.trim().to_string();
    if trimmed_game_id.is_empty() {
        return Err("gameId 不能为空".to_string());
    }
    if trimmed_version_id.is_empty() {
        return Err("versionId 不能为空".to_string());
    }
    on_progress(6, "正在检查游戏状态...".to_string());

    let (backup_root, rule, keep_versions, running_candidates) = {
        let store = app_state
            .store
            .lock()
            .map_err(|_| "无法锁定应用状态".to_string())?;
        let rule = select_rule_for_game(&store, &trimmed_game_id)
            .ok_or_else(|| "未找到该游戏可用规则".to_string())?;
        let candidates = collect_restore_running_session_candidates(&store, &rule, &trimmed_game_id);
        let keep_versions = resolve_backup_keep_versions(&store.execution_config, &rule.game_uid);
        (store.execution_config.backup_root.clone(), rule, keep_versions, candidates)
    };

    for (session_id, pid, _updated_ts) in running_candidates {
        if is_process_running(pid) {
            return Err(format!(
                "检测到游戏仍在运行（sessionId={}, pid={}），请先退出游戏再执行回滚",
                session_id, pid
            ));
        }
    };

    on_progress(18, "正在校验备份完整性与磁盘条件...".to_string());
    let base = ensure_backup_root_for_rule(&rule, &backup_root)?;
    let version_dir = base.join("versions").join(&trimmed_version_id);
    if !version_dir.exists() {
        return Err("指定备份版本不存在".to_string());
    }
    let manifest = verify_backup_manifest_integrity(&version_dir)?;
    let restore_exe_path = {
        let normalized_uid = normalize_game_uid(&rule.game_uid);
        let store = app_state
            .store
            .lock()
            .map_err(|_| "无法锁定应用状态".to_string())?;
        if normalized_uid.is_empty() {
            None
        } else {
            store.execution_config.preferred_exe_by_uid.get(&normalized_uid).cloned()
        }
    };
    validate_restore_targets_writable(&rule, restore_exe_path.as_deref())?;
    validate_restore_disk_space(&rule, &manifest, restore_exe_path.as_deref())?;

    on_progress(30, "正在创建回滚前备份...".to_string());
    let pre_restore_version_id =
        match backup_current_state_for_rule(
            &rule,
            &backup_root,
            keep_versions + 1,
            Some("pre_restore"),
            restore_exe_path.as_deref(),
        ) {
            Ok((changed, version_id)) if changed > 0 => Some(version_id),
            Ok(_) => None,
            Err(err) => return Err(format!("恢复前备份当前存档失败，已阻止回滚：{err}")),
        };

    on_progress(55, "正在恢复目标版本...".to_string());
    let restored_files =
        restore_backup_version_transactional(&rule, &version_dir, &base, restore_exe_path.as_deref())?;
    on_progress(78, "正在校验恢复结果...".to_string());
    let verification = match verify_restored_targets(&rule, &manifest, restore_exe_path.as_deref()) {
        Ok(summary) => summary,
        Err(verify_err) => {
            if let Some(pre_restore_id) = pre_restore_version_id.as_ref() {
                let rollback_version_dir = base.join("versions").join(pre_restore_id);
                if rollback_version_dir.exists() {
                    match restore_backup_version_transactional(
                        &rule,
                        &rollback_version_dir,
                        &base,
                        restore_exe_path.as_deref(),
                    ) {
                        Ok(_) => {
                            return Err(format!("恢复后校验失败，已自动回退到回滚前状态：{verify_err}"));
                        }
                        Err(rollback_err) => {
                            return Err(format!(
                                "恢复后校验失败，且自动回退失败：{verify_err}；回退错误：{rollback_err}"
                            ));
                        }
                    }
                }
            }
            return Err(format!("恢复后校验失败：{verify_err}"));
        }
    };

    on_progress(92, "正在写入会话日志...".to_string());
    {
        let mut store = app_state
            .store
            .lock()
            .map_err(|_| "无法锁定应用状态".to_string())?;
        let target_uid = normalize_game_uid(&rule.game_uid);
        let latest_idx = store
            .launcher_sessions
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                item.matched_game_uid
                    .as_ref()
                    .is_some_and(|uid| normalize_game_uid(uid) == target_uid)
                    || item
                        .matched_game_id
                        .as_ref()
                        .is_some_and(|id| id.eq_ignore_ascii_case(&trimmed_game_id))
            })
            .max_by_key(|(_, item)| session_updated_ts(item))
            .map(|(idx, _)| idx);
        if let Some(idx) = latest_idx {
            let session = &mut store.launcher_sessions[idx];
            append_session_log(
                session,
                &format!(
                    "已执行版本回滚（事务化）：game={}, version={}, files={}, verify_files={}, verify_hash_sample={}",
                    trimmed_game_id,
                    trimmed_version_id,
                    restored_files,
                    verification.verified_files,
                    verification.hash_sample_count
                ),
            );
            session.updated_at = now_iso_string();
        }
        persist_store(app, &store)?;
    }

    on_progress(
        100,
        format!(
            "恢复完成：{} 个文件，校验 {} 个，哈希抽样 {} 项",
            restored_files, verification.verified_files, verification.hash_sample_count
        ),
    );
    Ok(RestoreBackupResult {
        game_id: trimmed_game_id,
        version_id: trimmed_version_id,
        restored_files,
        pre_restore_version_id,
        verified_files: verification.verified_files,
        hash_sample_count: verification.hash_sample_count,
    })
}

#[tauri::command]
fn start_restore_backup_version_task(
    app: AppHandle,
    state: State<AppState>,
    game_id: String,
    version_id: String,
) -> Result<String, String> {
    let trimmed_game_id = game_id.trim().to_string();
    let trimmed_version_id = version_id.trim().to_string();
    if trimmed_game_id.is_empty() {
        return Err("gameId 不能为空".to_string());
    }
    if trimmed_version_id.is_empty() {
        return Err("versionId 不能为空".to_string());
    }

    let task_id = Uuid::new_v4().to_string();
    let now = now_iso_string();
    let task = BackgroundTask {
        task_id: task_id.clone(),
        task_type: "restore_backup_version".to_string(),
        status: "pending".to_string(),
        progress: Some(0),
        message: Some("任务已创建，等待执行".to_string()),
        result: None,
        error: None,
        started_at: now.clone(),
        updated_at: now,
    };
    {
        let mut tasks = state
            .tasks
            .lock()
            .map_err(|_| "无法锁定任务状态".to_string())?;
        tasks.insert(task_id.clone(), task);
    }

    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        update_background_task(
            &app_handle,
            &task_id_for_thread,
            "running",
            Some(3),
            Some("准备执行版本恢复...".to_string()),
            None,
            None,
        );
        let app_state: State<AppState> = app_handle.state();
        match restore_backup_version_impl(
            &app_handle,
            app_state.inner(),
            &trimmed_game_id,
            &trimmed_version_id,
            |progress, message| {
                update_background_task(
                    &app_handle,
                    &task_id_for_thread,
                    "running",
                    Some(progress),
                    Some(message),
                    None,
                    None,
                )
            },
        ) {
            Ok(summary) => {
                update_background_task(
                    &app_handle,
                    &task_id_for_thread,
                    "success",
                    Some(100),
                    Some(format!(
                        "恢复完成：文件 {}，校验 {}，抽样 {}",
                        summary.restored_files, summary.verified_files, summary.hash_sample_count
                    )),
                    serde_json::to_value(summary).ok(),
                    None,
                );
            }
            Err(err) => {
                update_background_task(
                    &app_handle,
                    &task_id_for_thread,
                    "failed",
                    Some(100),
                    Some("恢复失败".to_string()),
                    None,
                    Some(err),
                );
            }
        }
    });

    Ok(task_id)
}

#[tauri::command]
fn get_launcher_session(
    state: State<AppState>,
    launcher_session_id: String,
) -> Result<LauncherSession, String> {
    let store = state.store.lock().map_err(|_| "无法锁定应用状态".to_string())?;
    let session = store
        .launcher_sessions
        .iter()
        .find(|item| item.launcher_session_id == launcher_session_id)
        .ok_or_else(|| "launcherSessionId 不存在".to_string())?;
    Ok(session.clone())
}

#[tauri::command]
fn list_launcher_sessions(state: State<AppState>) -> Result<Vec<LauncherSession>, String> {
    let mut sessions = state
        .store
        .lock()
        .map_err(|_| "无法锁定应用状态".to_string())?
        .launcher_sessions
        .clone();
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(sessions)
}

#[tauri::command]
fn list_game_library_items(state: State<AppState>) -> Result<Vec<GameLibraryItem>, String> {
    let store = state.store.lock().map_err(|_| "无法锁定应用状态".to_string())?;
    Ok(build_game_library_items(&store))
}

#[tauri::command]
fn precheck_game_launch(
    app: AppHandle,
    state: State<AppState>,
    game_id: String,
) -> Result<GameLaunchPrecheck, String> {
    let trimmed_game_id = game_id.trim().to_string();
    if trimmed_game_id.is_empty() {
        return Err("gameId 不能为空".to_string());
    }
    let normalized_game_key = normalize_game_key(&trimmed_game_id);
    if normalized_game_key.is_empty() {
        return Err("gameId 不能为空".to_string());
    }

    let (rules, execution_config, selected_rule, preferred_exe_path) = {
        let store = state.store.lock().map_err(|_| "无法锁定应用状态".to_string())?;
        let selected_rule = select_rule_for_game(&store, &trimmed_game_id);
        let preferred_exe_path = selected_rule.as_ref().and_then(|rule| {
            let game_uid = normalize_game_uid(&rule.game_uid);
            if game_uid.is_empty() {
                None
            } else {
                store.execution_config.preferred_exe_by_uid.get(&game_uid).cloned()
            }
        });
        (
            store.rules.clone(),
            store.execution_config.clone(),
            selected_rule,
            preferred_exe_path,
        )
    };

    let mut checks = Vec::new();
    let mut exe_hash: Option<String> = None;
    let mut matched_rule: Option<GameSaveRule> = None;
    let mut exe_is_valid = false;
    let mut primary_rule_resolved = true;

    let has_selected_rule = selected_rule.is_some();
    checks.push(LaunchPrecheckCheck {
        key: "rule_available".to_string(),
        label: "可用规则".to_string(),
        ok: has_selected_rule,
        detail: if let Some(rule) = selected_rule.as_ref() {
            format!("已选规则 {}（{}）", rule.game_id, rule.rule_id)
        } else {
            "未找到可用规则，请先学习并保存规则".to_string()
        },
    });

    let trimmed_exe_path = preferred_exe_path
        .as_ref()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    checks.push(LaunchPrecheckCheck {
        key: "exe_bound".to_string(),
        label: "已绑定 EXE".to_string(),
        ok: trimmed_exe_path.is_some(),
        detail: trimmed_exe_path
            .as_ref()
            .map(|value| format!("当前绑定：{value}"))
            .unwrap_or_else(|| "未绑定 EXE，请先点击“选择/更换 EXE”".to_string()),
    });

    if let Some(exe_path_value) = trimmed_exe_path.as_ref() {
        let exe_path = Path::new(exe_path_value);
        let path_ok = exe_path.exists()
            && exe_path.is_file()
            && exe_path_value.to_ascii_lowercase().ends_with(".exe");
        exe_is_valid = path_ok;
        checks.push(LaunchPrecheckCheck {
            key: "exe_exists".to_string(),
            label: "EXE 文件可访问".to_string(),
            ok: path_ok,
            detail: if !exe_path.exists() {
                "已绑定 EXE 不存在，请重新绑定".to_string()
            } else if !exe_path.is_file() {
                "已绑定路径不是文件，请重新绑定".to_string()
            } else if !exe_path_value.to_ascii_lowercase().ends_with(".exe") {
                "已绑定路径不是 .exe 文件，请重新绑定".to_string()
            } else {
                "EXE 文件存在且可访问".to_string()
            },
        });
        if path_ok {
            match file_sha256_hex(exe_path) {
                Ok(hash) => {
                    exe_hash = Some(hash.clone());
                    let (resolved_rule, hash_matched_any) = match_enabled_rule_for_exe_hash(
                        &rules,
                        &execution_config,
                        &hash,
                        Some(&normalized_game_key),
                    );
                    matched_rule = resolved_rule;
                    primary_rule_resolved = !has_unresolved_primary_rule_conflict_for_exe_hash(
                        &rules,
                        &execution_config,
                        &hash,
                        Some(&normalized_game_key),
                    );
                    let match_ok = matched_rule.is_some();
                    checks.push(LaunchPrecheckCheck {
                        key: "rule_match".to_string(),
                        label: "规则哈希命中".to_string(),
                        ok: match_ok,
                        detail: if let Some(rule) = matched_rule.as_ref() {
                            format!("命中规则 {}（{}）", rule.game_id, rule.rule_id)
                        } else if hash_matched_any {
                            "当前 EXE 命中了其他游戏规则，请重新绑定".to_string()
                        } else {
                            "当前 EXE 未命中任何已启用规则".to_string()
                        },
                    });
                    checks.push(LaunchPrecheckCheck {
                        key: "rule_primary".to_string(),
                        label: "主规则已设置".to_string(),
                        ok: primary_rule_resolved,
                        detail: if !match_ok {
                            "当前未命中该游戏可用规则，暂无法判断主规则冲突".to_string()
                        } else if primary_rule_resolved {
                            "当前 EXE 命中的规则冲突已处理，可确定唯一生效规则".to_string()
                        } else {
                            "当前 EXE 命中多条启用规则但未设置主规则，请到规则管理点击“设为主规则”".to_string()
                        },
                    });
                }
                Err(err) => {
                    checks.push(LaunchPrecheckCheck {
                        key: "rule_match".to_string(),
                        label: "规则哈希命中".to_string(),
                        ok: false,
                        detail: format!("计算 EXE 哈希失败：{err}"),
                    });
                    checks.push(LaunchPrecheckCheck {
                        key: "rule_primary".to_string(),
                        label: "主规则已设置".to_string(),
                        ok: false,
                        detail: "无法计算 EXE 哈希，无法判断主规则冲突".to_string(),
                    });
                }
            }
        } else {
            checks.push(LaunchPrecheckCheck {
                key: "rule_match".to_string(),
                label: "规则哈希命中".to_string(),
                ok: false,
                detail: "需要先绑定有效 EXE".to_string(),
            });
            checks.push(LaunchPrecheckCheck {
                key: "rule_primary".to_string(),
                label: "主规则已设置".to_string(),
                ok: false,
                detail: "需要先绑定有效 EXE".to_string(),
            });
        }
    } else {
        checks.push(LaunchPrecheckCheck {
            key: "exe_exists".to_string(),
            label: "EXE 文件可访问".to_string(),
            ok: false,
            detail: "尚未绑定 EXE".to_string(),
        });
        checks.push(LaunchPrecheckCheck {
            key: "rule_match".to_string(),
            label: "规则哈希命中".to_string(),
            ok: false,
            detail: "尚未绑定 EXE".to_string(),
        });
        checks.push(LaunchPrecheckCheck {
            key: "rule_primary".to_string(),
            label: "主规则已设置".to_string(),
            ok: false,
            detail: "尚未绑定 EXE".to_string(),
        });
    }

    let resolved_rule_for_paths = matched_rule.as_ref().or(selected_rule.as_ref());
    let path_resolution = if let Some(rule) = resolved_rule_for_paths {
        let exe_context = trimmed_exe_path.as_deref();
        let mut failed_message: Option<String> = None;
        for confirmed_path in &rule.confirmed_paths {
            if let Err(err) = expand_confirmed_path_for_runtime(confirmed_path, exe_context) {
                failed_message = Some(err);
                break;
            }
        }
        match failed_message {
            Some(message) => (false, message),
            None => (true, "规则路径均可解析".to_string()),
        }
    } else {
        (false, "当前没有可用于解析路径的规则".to_string())
    };
    checks.push(LaunchPrecheckCheck {
        key: "rule_path_resolution".to_string(),
        label: "规则路径可解析".to_string(),
        ok: path_resolution.0,
        detail: path_resolution.1.clone(),
    });

    let backup_probe_root = if let Some(rule) = resolved_rule_for_paths {
        let uid = normalize_game_uid(&rule.game_uid);
        if uid.is_empty() {
            Path::new(&execution_config.backup_root).to_path_buf()
        } else {
            backup_game_root(&execution_config.backup_root, &uid)
        }
    } else {
        Path::new(&execution_config.backup_root).to_path_buf()
    };
    let backup_writable = ensure_directory_writable(&backup_probe_root);
    let backup_writable_ok = backup_writable.is_ok();
    let backup_writable_detail = match backup_writable {
        Ok(_) => format!("可写：{}", backup_probe_root.to_string_lossy()),
        Err(err) => format!("不可写：{err}"),
    };
    checks.push(LaunchPrecheckCheck {
        key: "backup_writable".to_string(),
        label: "备份目录可写".to_string(),
        ok: backup_writable_ok,
        detail: backup_writable_detail,
    });

    let sandbox_available = resolve_sandboxie_start_path(&execution_config);
    let sandbox_available_ok = sandbox_available.is_ok();
    let sandbox_available_detail = sandbox_available
        .map(|path| format!("可用：{path}"))
        .unwrap_or_else(|err| err);
    checks.push(LaunchPrecheckCheck {
        key: "sandbox_runtime".to_string(),
        label: "Sandboxie 可用".to_string(),
        ok: sandbox_available_ok,
        detail: sandbox_available_detail,
    });

    let artifacts = resolve_redirector_artifacts(&app);
    let inject_artifacts_ok = artifacts.injector_path.exists() && artifacts.dll_path.exists();
    let inject_artifacts_detail = if inject_artifacts_ok {
        format!(
            "组件就绪：{} / {}",
            artifacts.injector_path.to_string_lossy(),
            artifacts.dll_path.to_string_lossy()
        )
    } else {
        let mut missing = Vec::new();
        if !artifacts.injector_path.exists() {
            missing.push(format!("injector 缺失：{}", artifacts.injector_path.to_string_lossy()));
        }
        if !artifacts.dll_path.exists() {
            missing.push(format!("hook DLL 缺失：{}", artifacts.dll_path.to_string_lossy()));
        }
        missing.join("；")
    };
    checks.push(LaunchPrecheckCheck {
        key: "inject_artifacts".to_string(),
        label: "注入组件可用".to_string(),
        ok: inject_artifacts_ok,
        detail: inject_artifacts_detail,
    });

    let inject_arch_ok = if let Some(exe_path_value) = trimmed_exe_path.as_ref() {
        if exe_is_valid {
            is_x64_pe(Path::new(exe_path_value))
        } else {
            false
        }
    } else {
        false
    };
    checks.push(LaunchPrecheckCheck {
        key: "inject_arch".to_string(),
        label: "注入架构兼容".to_string(),
        ok: inject_arch_ok,
        detail: if !exe_is_valid {
            "需要先绑定有效 EXE".to_string()
        } else if inject_arch_ok {
            "目标 EXE 为 x64".to_string()
        } else {
            "目标 EXE 非 x64，注入模式暂不支持".to_string()
        },
    });

    let rule_match_ok = matched_rule.is_some();
    let path_resolution_ok = path_resolution.0;
    let backup_ready = rule_match_ok && primary_rule_resolved && path_resolution_ok && backup_writable_ok;
    let sandbox_ready = rule_match_ok && path_resolution_ok && sandbox_available_ok;
    let inject_ready = rule_match_ok && path_resolution_ok && inject_artifacts_ok && inject_arch_ok;

    Ok(GameLaunchPrecheck {
        game_id: trimmed_game_id,
        preferred_exe_path: trimmed_exe_path,
        exe_hash,
        matched_rule_id: matched_rule.as_ref().map(|rule| rule.rule_id.clone()),
        backup_ready,
        sandbox_ready,
        inject_ready,
        checks,
        checked_at: now_iso_string(),
    })
}

#[tauri::command]
fn set_preferred_exe_path(
    app: AppHandle,
    state: State<AppState>,
    game_id: String,
    exe_path: String,
) -> Result<GameLibraryItem, String> {
    let normalized_game_key = normalize_game_key(&game_id);
    if normalized_game_key.is_empty() {
        return Err("gameId 不能为空".to_string());
    }
    let trimmed_exe_path = exe_path.trim().to_string();
    if trimmed_exe_path.is_empty() {
        return Err("exePath 不能为空".to_string());
    }
    let exe = Path::new(&trimmed_exe_path);
    if !exe.exists() {
        return Err("exePath 不存在".to_string());
    }
    if !exe.is_file() {
        return Err("exePath 不是有效文件".to_string());
    }
    if !trimmed_exe_path.to_ascii_lowercase().ends_with(".exe") {
        return Err("exePath 必须是 .exe 文件".to_string());
    }

    let mut store = state.store.lock().map_err(|_| "无法锁定应用状态".to_string())?;
    let Some(rule) = select_rule_for_game(&store, &game_id) else {
        return Err("该 gameId 暂无可用规则，请先学习并保存规则".to_string());
    };
    let game_uid = normalize_game_uid(&rule.game_uid);
    if game_uid.is_empty() {
        return Err("规则缺少 gameUid，请先刷新规则后重试".to_string());
    }
    store
        .execution_config
        .preferred_exe_by_uid
        .insert(game_uid.clone(), trimmed_exe_path.clone());
    store
        .execution_config
        .preferred_rule_uid_by_game
        .insert(normalized_game_key.clone(), normalize_game_uid(&rule.game_uid));
    for existing_rule in &mut store.rules {
        if normalize_game_uid(&existing_rule.game_uid) != game_uid {
            continue;
        }
        existing_rule.confirmed_paths =
            normalize_paths(existing_rule.confirmed_paths.clone(), Some(&trimmed_exe_path));
        existing_rule.updated_at = now_iso_string();
    }
    normalize_store(&mut store);
    persist_store(&app, &store)?;

    build_game_library_items(&store)
        .into_iter()
        .find(|item| normalize_game_key(&item.game_id) == normalized_game_key)
        .ok_or_else(|| "更新 EXE 绑定后未找到对应游戏卡片".to_string())
}

fn resolve_user_profile_path() -> Option<String> {
    let by_user_profile = std::env::var("USERPROFILE").ok().map(|value| value.trim().to_string());
    if let Some(path) = by_user_profile.filter(|value| !value.is_empty()) {
        return Some(path.replace('/', "\\"));
    }
    let home_drive = std::env::var("HOMEDRIVE").ok().map(|value| value.trim().to_string());
    let home_path = std::env::var("HOMEPATH").ok().map(|value| value.trim().to_string());
    match (home_drive, home_path) {
        (Some(drive), Some(path)) if !drive.is_empty() && !path.is_empty() => {
            Some(format!("{}{}", drive, path).replace('/', "\\"))
        }
        _ => None,
    }
}

fn strip_prefix_case_insensitive(path: &str, prefix: &str) -> Option<String> {
    let normalized_path = path.replace('/', "\\");
    let normalized_prefix = prefix.replace('/', "\\").trim_end_matches('\\').to_string();
    if normalized_prefix.is_empty() {
        return None;
    }
    let path_lower = normalized_path.to_ascii_lowercase();
    let prefix_lower = normalized_prefix.to_ascii_lowercase();
    if path_lower == prefix_lower {
        return Some(String::new());
    }
    let prefix_with_sep = format!("{prefix_lower}\\");
    if path_lower.starts_with(&prefix_with_sep) {
        return Some(normalized_path[normalized_prefix.len() + 1..].to_string());
    }
    None
}

fn strip_windows_users_prefix(path: &str) -> Option<String> {
    let normalized = path.trim().replace('/', "\\");
    let bytes = normalized.as_bytes();
    if bytes.len() < 4 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' || bytes[2] != b'\\' {
        return None;
    }
    let rest = &normalized[3..];
    let rest_lower = rest.to_ascii_lowercase();
    if !rest_lower.starts_with("users\\") {
        return None;
    }
    let after_users = &rest[6..];
    let mut splitter = after_users.splitn(2, '\\');
    let user_segment = splitter.next().unwrap_or("");
    if user_segment.is_empty() {
        return None;
    }
    let suffix = splitter.next().unwrap_or("");
    if suffix.is_empty() {
        return None;
    }
    Some(suffix.to_string())
}

fn resolve_game_dir_root(exe_path: Option<&str>) -> Option<String> {
    let exe_path = exe_path?.trim();
    if exe_path.is_empty() {
        return None;
    }
    let exe_dir = Path::new(exe_path).parent()?;
    let normalized = exe_dir.to_string_lossy().trim().replace('/', "\\");
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn contains_parent_dir_segment(path: &str) -> bool {
    path.split('\\')
        .filter(|segment| !segment.is_empty())
        .any(|segment| segment == "..")
}

fn normalize_confirmed_path_for_storage(path: &str, exe_path: Option<&str>) -> String {
    let trimmed = path.trim().replace('/', "\\");
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.eq_ignore_ascii_case(GAME_DIR_TOKEN) {
        return GAME_DIR_TOKEN.to_string();
    }
    if let Some(suffix) = strip_prefix_case_insensitive(&trimmed, GAME_DIR_TOKEN) {
        if suffix.is_empty() {
            return GAME_DIR_TOKEN.to_string();
        }
        if !contains_parent_dir_segment(&suffix) {
            return format!(r"{}\{}", GAME_DIR_TOKEN, suffix);
        }
    }
    if let Some(game_dir_root) = resolve_game_dir_root(exe_path) {
        if let Some(suffix) = strip_prefix_case_insensitive(&trimmed, &game_dir_root) {
            if suffix.is_empty() {
                return GAME_DIR_TOKEN.to_string();
            }
            if !contains_parent_dir_segment(&suffix) {
                return format!(r"{}\{}", GAME_DIR_TOKEN, suffix);
            }
        }
    }
    if trimmed.eq_ignore_ascii_case(USERPROFILE_TOKEN) {
        return USERPROFILE_TOKEN.to_string();
    }
    if let Some(suffix) = strip_prefix_case_insensitive(&trimmed, USERPROFILE_TOKEN) {
        if suffix.is_empty() {
            return USERPROFILE_TOKEN.to_string();
        }
        return format!(r"{}\{}", USERPROFILE_TOKEN, suffix);
    }
    if let Some(profile_root) = resolve_user_profile_path() {
        if let Some(suffix) = strip_prefix_case_insensitive(&trimmed, &profile_root) {
            if suffix.is_empty() {
                return USERPROFILE_TOKEN.to_string();
            }
            return format!(r"{}\{}", USERPROFILE_TOKEN, suffix);
        }
    }
    if let Some(suffix) = strip_windows_users_prefix(&trimmed) {
        return format!(r"{}\{}", USERPROFILE_TOKEN, suffix);
    }
    trimmed
}

fn expand_confirmed_path_for_runtime(path: &str, exe_path: Option<&str>) -> Result<String, String> {
    let trimmed = path.trim().replace('/', "\\");
    if trimmed.is_empty() {
        return Err("规则路径为空".to_string());
    }
    if let Some(suffix) = strip_prefix_case_insensitive(&trimmed, GAME_DIR_TOKEN) {
        if contains_parent_dir_segment(&suffix) {
            return Err(format!("规则路径非法：{} 包含越界片段", path.trim()));
        }
        let Some(game_dir_root) = resolve_game_dir_root(exe_path) else {
            return Err(format!("规则路径依赖 {}，但当前未绑定可用 EXE", GAME_DIR_TOKEN));
        };
        if suffix.is_empty() {
            return Ok(game_dir_root);
        }
        return Ok(format!(r"{}\{}", game_dir_root.trim_end_matches('\\'), suffix));
    }
    if let Some(suffix) = strip_prefix_case_insensitive(&trimmed, USERPROFILE_TOKEN) {
        if let Some(profile_root) = resolve_user_profile_path() {
            if suffix.is_empty() {
                return Ok(profile_root);
            }
            if contains_parent_dir_segment(&suffix) {
                return Err(format!("规则路径非法：{} 包含越界片段", path.trim()));
            }
            return Ok(format!(r"{}\{}", profile_root.trim_end_matches('\\'), suffix));
        }
    }
    if contains_parent_dir_segment(&trimmed) {
        return Err(format!("规则路径非法：{} 包含越界片段", path.trim()));
    }
    Ok(trimmed)
}

fn normalize_paths(paths: Vec<String>, exe_path: Option<&str>) -> Vec<String> {
    let mut dedup = HashSet::new();
    let mut output = Vec::new();
    for path in paths {
        let normalized = normalize_confirmed_path_for_storage(&path, exe_path);
        if normalized.is_empty() {
            continue;
        }
        let dedup_key = normalize_windows_path(&normalized);
        if dedup.insert(dedup_key) {
            output.push(normalized);
        }
    }
    output
}

fn append_session_log(session: &mut LauncherSession, message: &str) {
    session.logs.push(format!("[{}] {}", now_iso_string(), message));
}

fn join_managed_root_for_game(root: &str, game_id: &str) -> String {
    Path::new(root)
        .join(game_id.trim())
        .to_string_lossy()
        .to_string()
}

fn normalize_launch_mode(value: Option<&str>) -> String {
    match value.unwrap_or("backup").trim().to_ascii_lowercase().as_str() {
        "backup" => "backup".to_string(),
        "sandbox" => "sandbox".to_string(),
        _ => "inject".to_string(),
    }
}

fn ensure_directory_writable(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|err| format!("创建目录失败: {err}"))?;
    let probe = path.join(format!(".gamesaver_write_probe_{}.tmp", Uuid::new_v4()));
    fs::write(&probe, b"ok").map_err(|err| format!("写入测试文件失败: {err}"))?;
    let _ = fs::remove_file(&probe);
    Ok(())
}

fn choose_restore_writable_probe_path(source: &Path) -> PathBuf {
    if source.exists() {
        if source.is_dir() {
            return source.to_path_buf();
        }
        return source
            .parent()
            .map(|parent| parent.to_path_buf())
            .unwrap_or_else(|| source.to_path_buf());
    }
    if source.extension().is_some() {
        return source
            .parent()
            .map(|parent| parent.to_path_buf())
            .unwrap_or_else(|| source.to_path_buf());
    }
    source.to_path_buf()
}

fn validate_restore_targets_writable(rule: &GameSaveRule, exe_path: Option<&str>) -> Result<(), String> {
    for source_path in &rule.confirmed_paths {
        let runtime_path = expand_confirmed_path_for_runtime(source_path, exe_path)?;
        let source = Path::new(&runtime_path);
        let probe_root = choose_restore_writable_probe_path(source);
        ensure_directory_writable(&probe_root).map_err(|err| {
            format!(
                "目标路径不可写：{}（检查目录 {}）: {err}",
                source.to_string_lossy(),
                probe_root.to_string_lossy()
            )
        })?;
    }
    Ok(())
}

fn is_process_running(pid: u32) -> bool {
    let script = format!(
        "$p=Get-Process -Id {pid} -ErrorAction SilentlyContinue; if ($null -ne $p) {{ Write-Output {pid} }}"
    );
    let mut command = Command::new("powershell");
    command.args(["-NoProfile", "-Command", &script]);
    let output = apply_background_process_flags(&mut command).output();
    let Ok(out) = output else {
        return false;
    };
    parse_u32(&String::from_utf8_lossy(&out.stdout)).is_some_and(|value| value == pid)
}

fn collect_restore_running_session_candidates(
    store: &PersistedStore,
    rule: &GameSaveRule,
    game_id: &str,
) -> Vec<(String, u32, u64)> {
    let normalized_uid = normalize_game_uid(&rule.game_uid);
    let normalized_game_key = normalize_game_key(game_id);
    let mut output = Vec::new();
    for session in &store.launcher_sessions {
        if session.status != "running" {
            continue;
        }
        let Some(pid) = session.pid else {
            continue;
        };
        let uid_match = session
            .matched_game_uid
            .as_ref()
            .is_some_and(|uid| normalize_game_uid(uid) == normalized_uid);
        let game_match = session
            .matched_game_id
            .as_ref()
            .is_some_and(|id| normalize_game_key(id) == normalized_game_key);
        if uid_match || game_match {
            output.push((session.launcher_session_id.clone(), pid, session_updated_ts(session)));
        }
    }
    output.sort_by(|a, b| b.2.cmp(&a.2));
    output
}

fn drive_id_for_path(path: &Path) -> Option<String> {
    let text = path.to_string_lossy();
    let bytes = text.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        let letter = (bytes[0] as char).to_ascii_uppercase();
        return Some(format!("{letter}:"));
    }
    None
}

#[cfg(windows)]
fn query_drive_free_space_bytes(drive_id: &str) -> Result<u64, String> {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt};
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let trimmed = drive_id.trim().trim_end_matches(['\\', '/']);
    if trimmed.is_empty() {
        return Err("查询磁盘空间失败：磁盘标识为空".to_string());
    }
    let root = if trimmed.ends_with(':') {
        format!("{trimmed}\\")
    } else {
        format!("{trimmed}:\\")
    };
    let wide: Vec<u16> = OsStr::new(&root).encode_wide().chain(Some(0)).collect();
    let mut free_bytes_available: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut total_free_bytes: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_bytes_available,
            &mut total_bytes,
            &mut total_free_bytes,
        )
    };
    if ok == 0 {
        return Err(format!("查询磁盘空间失败：无法读取 {root} 可用空间"));
    }
    Ok(free_bytes_available)
}

#[cfg(not(windows))]
fn query_drive_free_space_bytes(drive_id: &str) -> Result<u64, String> {
    Err(format!("查询磁盘空间失败：当前平台不支持磁盘 {drive_id} 空间检查"))
}

fn format_bytes_short(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit_index = 0usize;
    while value >= 1024.0 && unit_index < UNITS.len() - 1 {
        value /= 1024.0;
        unit_index += 1;
    }
    if unit_index == 0 {
        format!("{bytes} {}", UNITS[unit_index])
    } else if value >= 100.0 {
        format!("{:.0} {}", value, UNITS[unit_index])
    } else if value >= 10.0 {
        format!("{:.1} {}", value, UNITS[unit_index])
    } else {
        format!("{:.2} {}", value, UNITS[unit_index])
    }
}

fn validate_restore_disk_space(
    rule: &GameSaveRule,
    manifest: &BackupManifest,
    exe_path: Option<&str>,
) -> Result<(), String> {
    let mut slot_to_drive: HashMap<String, String> = HashMap::new();
    for source_path in &rule.confirmed_paths {
        let runtime_path = expand_confirmed_path_for_runtime(source_path, exe_path)?;
        let source = Path::new(&runtime_path);
        let drive_id = drive_id_for_path(source)
            .ok_or_else(|| format!("无法识别目标路径所在磁盘: {}", source.to_string_lossy()))?;
        let slot = backup_slot_name(source_path);
        slot_to_drive.insert(slot, drive_id.clone());
        let legacy_slot = backup_slot_name_legacy(source_path);
        slot_to_drive.insert(legacy_slot, drive_id);
    }

    let mut required_by_drive: HashMap<String, u64> = HashMap::new();
    for item in &manifest.files {
        let normalized_path = normalize_windows_path(&item.path);
        let Some(slot_name) = normalized_path.split('\\').next() else {
            return Err(format!("备份 manifest 文件路径无效: {}", item.path));
        };
        let drive_id = slot_to_drive
            .get(slot_name)
            .cloned()
            .ok_or_else(|| format!("备份 manifest 槽位与规则不一致: {}", item.path))?;
        *required_by_drive.entry(drive_id).or_insert(0) += item.size;
    }

    for (drive_id, required_bytes) in required_by_drive {
        let free_bytes = query_drive_free_space_bytes(&drive_id)?;
        if free_bytes < required_bytes {
            return Err(format!(
                "磁盘空间不足：{} 盘可用 {}，恢复至少需要 {}",
                drive_id,
                format_bytes_short(free_bytes),
                format_bytes_short(required_bytes)
            ));
        }
    }
    Ok(())
}

fn verify_restored_targets(
    rule: &GameSaveRule,
    manifest: &BackupManifest,
    exe_path: Option<&str>,
) -> Result<RestoreVerificationSummary, String> {
    let mut slot_to_target: HashMap<String, PathBuf> = HashMap::new();
    for source_path in &rule.confirmed_paths {
        let runtime_path = expand_confirmed_path_for_runtime(source_path, exe_path)?;
        let target_root = PathBuf::from(runtime_path);
        let slot = backup_slot_name(source_path);
        slot_to_target.insert(slot, target_root.clone());
        let legacy_slot = backup_slot_name_legacy(source_path);
        slot_to_target.insert(legacy_slot, target_root);
    }

    let mut files_to_verify = Vec::new();
    for item in &manifest.files {
        let normalized_path = normalize_windows_path(&item.path);
        let mut pieces = normalized_path.splitn(2, '\\');
        let slot_name = pieces
            .next()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| format!("恢复后校验失败：manifest 路径非法 {}", item.path))?;
        let relative = pieces
            .next()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| format!("恢复后校验失败：manifest 路径非法 {}", item.path))?;
        let target_root = slot_to_target
            .get(slot_name)
            .ok_or_else(|| format!("恢复后校验失败：槽位映射不存在 {}", item.path))?;
        let mut target_file = target_root.to_path_buf();
        for segment in relative.split('\\').filter(|segment| !segment.is_empty()) {
            target_file = target_file.join(segment);
        }
        if !target_file.exists() {
            return Err(format!("恢复后校验失败：文件缺失 {}", target_file.to_string_lossy()));
        }
        let actual_size = target_file
            .metadata()
            .map_err(|err| format!("恢复后校验失败：读取文件元信息失败 {}: {err}", target_file.to_string_lossy()))?
            .len();
        if actual_size != item.size {
            return Err(format!(
                "恢复后校验失败：文件大小不一致 {} (expected={}, actual={})",
                target_file.to_string_lossy(),
                item.size,
                actual_size
            ));
        }
        files_to_verify.push((target_file, item.sha256.clone()));
    }

    let total = files_to_verify.len();
    if total == 0 {
        return Ok(RestoreVerificationSummary {
            verified_files: 0,
            hash_sample_count: 0,
        });
    }

    let sample_count = total.min(5);
    for sample_index in 0..sample_count {
        let idx = sample_index * total / sample_count;
        let (path, expected_sha) = &files_to_verify[idx];
        let actual_sha = file_sha256_hex_with_context(path, "恢复后文件")?;
        if !actual_sha.eq_ignore_ascii_case(expected_sha.trim()) {
            return Err(format!("恢复后校验失败：哈希不一致 {}", path.to_string_lossy()));
        }
    }

    Ok(RestoreVerificationSummary {
        verified_files: total,
        hash_sample_count: sample_count,
    })
}

fn sanitize_sandbox_box_name(game_id: &str) -> String {
    let mut out = String::new();
    for ch in game_id.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let compact = out.trim_matches('_').to_string();
    if compact.is_empty() {
        "GameSaver_Default".to_string()
    } else {
        format!("GameSaver_{compact}")
    }
}

fn resolve_sandboxie_start_path(config: &ExecutionConfig) -> Result<String, String> {
    let configured = config.sandboxie_start_exe.trim();
    if !configured.is_empty() && Path::new(configured).exists() {
        return Ok(configured.to_string());
    }
    let fallbacks = [
        "C:\\Program Files\\Sandboxie-Plus\\Start.exe",
        "C:\\Program Files\\Sandboxie\\Start.exe",
    ];
    for item in fallbacks {
        if Path::new(item).exists() {
            return Ok(item.to_string());
        }
    }
    Err("未找到 Sandboxie Start.exe，请先安装 Sandboxie Plus 或在配置中提供路径".to_string())
}

fn build_sandbox_mirror_path(
    original_path: &str,
    sandbox_root: &str,
    user_name: &str,
    box_name: &str,
) -> Result<PathBuf, String> {
    let normalized = original_path.trim().replace('/', "\\");
    if normalized.len() < 3 {
        return Err(format!("无效路径: {original_path}"));
    }
    let bytes = normalized.as_bytes();
    if !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
        return Err(format!("仅支持盘符绝对路径: {original_path}"));
    }
    let drive = (bytes[0] as char).to_ascii_uppercase().to_string();
    let remain = normalized[2..].trim_start_matches('\\').to_string();
    Ok(Path::new(sandbox_root)
        .join(user_name)
        .join(box_name)
        .join("drive")
        .join(drive)
        .join(remain))
}

fn sync_directory(source: &Path, target: &Path) -> Result<usize, String> {
    if !source.exists() {
        return Ok(0);
    }
    fs::create_dir_all(target).map_err(|err| format!("创建目录失败: {err}"))?;
    let mut copied = 0_usize;
    for entry in WalkDir::new(source).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = match entry.path().strip_prefix(source) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let dest = target.join(relative);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|err| format!("创建目录失败: {err}"))?;
        }
        fs::copy(entry.path(), &dest).map_err(|err| format!("复制文件失败: {err}"))?;
        copied += 1;
    }
    Ok(copied)
}

fn backup_game_root(backup_root: &str, game_uid: &str) -> PathBuf {
    Path::new(backup_root).join(game_uid.trim())
}

fn legacy_backup_game_root(backup_root: &str, game_id: &str) -> PathBuf {
    Path::new(backup_root).join(game_id.trim())
}

fn backup_slot_name(source_path: &str) -> String {
    let normalized = normalize_windows_path(&normalize_confirmed_path_for_storage(source_path, None));
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    let hash = hex::encode(hasher.finalize());
    format!("slot_{}", &hash[..12])
}

fn backup_slot_name_legacy(source_path: &str) -> String {
    let normalized = normalize_windows_path(source_path);
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    let hash = hex::encode(hasher.finalize());
    format!("slot_{}", &hash[..12])
}

fn normalize_game_key(game_id: &str) -> String {
    game_id.trim().to_ascii_lowercase()
}

fn normalize_game_uid(game_uid: &str) -> String {
    game_uid.trim().to_ascii_lowercase()
}

fn normalize_exe_hash(exe_hash: &str) -> String {
    exe_hash.trim().to_ascii_lowercase()
}

fn normalize_backup_keep_versions(keep: usize) -> usize {
    keep.clamp(1, MAX_BACKUP_KEEP_VERSIONS)
}

fn new_game_uid() -> String {
    Uuid::new_v4().to_string()
}

fn resolve_backup_keep_versions(execution_config: &ExecutionConfig, game_uid: &str) -> usize {
    let uid = normalize_game_uid(game_uid);
    if uid.is_empty() {
        return DEFAULT_BACKUP_KEEP_VERSIONS;
    }
    execution_config
        .backup_keep_versions_by_uid
        .get(&uid)
        .copied()
        .filter(|keep| *keep > 0)
        .map(normalize_backup_keep_versions)
        .unwrap_or(DEFAULT_BACKUP_KEEP_VERSIONS)
}

fn resolve_preferred_rule_id_for_exe_hash(
    execution_config: &ExecutionConfig,
    exe_hash: &str,
) -> Option<String> {
    let normalized_hash = normalize_exe_hash(exe_hash);
    if normalized_hash.is_empty() {
        return None;
    }
    execution_config
        .preferred_rule_id_by_exe_hash
        .get(&normalized_hash)
        .cloned()
        .filter(|rule_id| !rule_id.trim().is_empty())
}

fn match_enabled_rule_for_exe_hash(
    rules: &[GameSaveRule],
    execution_config: &ExecutionConfig,
    exe_hash: &str,
    expected_game_key: Option<&str>,
) -> (Option<GameSaveRule>, bool) {
    let normalized_hash = normalize_exe_hash(exe_hash);
    if normalized_hash.is_empty() {
        return (None, false);
    }

    let preferred_rule_id_for_hash =
        resolve_preferred_rule_id_for_exe_hash(execution_config, &normalized_hash);
    let mut hash_matched_any = false;
    let mut candidates = Vec::new();
    for rule in rules {
        if !rule.enabled || normalize_exe_hash(&rule.exe_hash) != normalized_hash {
            continue;
        }
        hash_matched_any = true;
        let game_matches = if let Some(game_key) = expected_game_key {
            normalize_game_key(&rule.game_id) == game_key
        } else {
            true
        };
        if game_matches {
            candidates.push(rule.clone());
        }
    }

    if let Some(preferred_rule_id) = preferred_rule_id_for_hash {
        if let Some(preferred) = candidates
            .iter()
            .find(|rule| rule.rule_id == preferred_rule_id)
            .cloned()
        {
            return (Some(preferred), hash_matched_any);
        }
    }
    (
        candidates
            .into_iter()
            .max_by_key(|rule| (rule.enabled as u8, rule_updated_ts(rule))),
        hash_matched_any,
    )
}

fn has_unresolved_primary_rule_conflict_for_exe_hash(
    rules: &[GameSaveRule],
    execution_config: &ExecutionConfig,
    exe_hash: &str,
    expected_game_key: Option<&str>,
) -> bool {
    let normalized_hash = normalize_exe_hash(exe_hash);
    if normalized_hash.is_empty() {
        return false;
    }
    let mut candidate_rule_ids = Vec::new();
    for rule in rules {
        if !rule.enabled || normalize_exe_hash(&rule.exe_hash) != normalized_hash {
            continue;
        }
        let game_matches = if let Some(game_key) = expected_game_key {
            normalize_game_key(&rule.game_id) == game_key
        } else {
            true
        };
        if game_matches {
            candidate_rule_ids.push(rule.rule_id.clone());
        }
    }
    if candidate_rule_ids.len() <= 1 {
        return false;
    }
    let Some(preferred_rule_id) = resolve_preferred_rule_id_for_exe_hash(execution_config, &normalized_hash) else {
        return true;
    };
    !candidate_rule_ids.iter().any(|rule_id| rule_id == &preferred_rule_id)
}

fn ensure_backup_root_for_rule(rule: &GameSaveRule, backup_root: &str) -> Result<PathBuf, String> {
    let game_uid = normalize_game_uid(&rule.game_uid);
    if game_uid.is_empty() {
        return Err(format!("规则 {} 缺少 gameUid", rule.rule_id));
    }
    let uid_root = backup_game_root(backup_root, &game_uid);
    let legacy_root = legacy_backup_game_root(backup_root, &rule.game_id);
    if !uid_root.exists() && legacy_root.exists() {
        let _ = sync_directory(&legacy_root, &uid_root);
    }
    Ok(uid_root)
}

fn unix_string_or_zero(value: &str) -> u64 {
    value.trim().parse::<u64>().ok().unwrap_or(0)
}

fn rule_updated_ts(rule: &GameSaveRule) -> u64 {
    let updated = rule.updated_at.trim();
    if !updated.is_empty() {
        return unix_string_or_zero(updated);
    }
    unix_string_or_zero(&rule.created_at)
}

fn session_updated_ts(session: &LauncherSession) -> u64 {
    let updated = session.updated_at.trim();
    if !updated.is_empty() {
        return unix_string_or_zero(updated);
    }
    unix_string_or_zero(&session.started_at)
}

fn build_game_library_items(store: &PersistedStore) -> Vec<GameLibraryItem> {
    let mut grouped: HashMap<String, GameLibraryAccumulator> = HashMap::new();
    for rule in &store.rules {
        let key = normalize_game_key(&rule.game_id);
        if key.is_empty() {
            continue;
        }
        let rule_ts = rule_updated_ts(rule);
        let rule_updated_at = if rule.updated_at.trim().is_empty() {
            rule.created_at.clone()
        } else {
            rule.updated_at.clone()
        };
        let entry = grouped.entry(key).or_default();
        entry.total_rules += 1;
        if rule.enabled {
            entry.enabled_rules += 1;
        }
        entry.confirmed_path_count += rule.confirmed_paths.len();
        if entry.game_id.trim().is_empty() || rule_ts >= entry.last_rule_ts {
            entry.game_id = rule.game_id.clone();
            entry.game_uid = normalize_game_uid(&rule.game_uid);
            entry.last_rule_ts = rule_ts;
            entry.last_rule_updated_at = rule_updated_at;
        }
    }

    for (game_key, entry) in &mut grouped {
        if let Some(primary_rule) = select_rule_for_game(store, game_key) {
            entry.game_uid = normalize_game_uid(&primary_rule.game_uid);
            entry.game_id = primary_rule.game_id;
        }
    }

    for session in &store.launcher_sessions {
        let Some(matched_game_id) = session.matched_game_id.as_ref() else {
            continue;
        };
        let key = normalize_game_key(matched_game_id);
        if key.is_empty() {
            continue;
        }
        let Some(entry) = grouped.get_mut(&key) else {
            continue;
        };
        let session_ts = session_updated_ts(session);
        if session_ts >= entry.last_session_ts {
            entry.last_session_ts = session_ts;
            entry.last_session_id = Some(session.launcher_session_id.clone());
            entry.last_session_status = Some(session.status.clone());
            entry.last_session_updated_at = Some(session.updated_at.clone());
            entry.last_injection_status = Some(session.injection_status.clone());
        }
    }

    for entry in grouped.values_mut() {
        if entry.game_uid.trim().is_empty() {
            continue;
        }
        if let Some(path) = store.execution_config.preferred_exe_by_uid.get(&entry.game_uid) {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                entry.preferred_exe_path = Some(trimmed.to_string());
            }
        }
    }

    let mut cards: Vec<(u64, GameLibraryItem)> = grouped
        .into_values()
        .map(|entry| {
            let activity_ts = entry.last_session_ts.max(entry.last_rule_ts);
            (
                activity_ts,
                GameLibraryItem {
                    game_id: entry.game_id,
                    total_rules: entry.total_rules,
                    enabled_rules: entry.enabled_rules,
                    confirmed_path_count: entry.confirmed_path_count,
                    last_rule_updated_at: entry.last_rule_updated_at,
                    preferred_exe_path: entry.preferred_exe_path,
                    last_session_id: entry.last_session_id,
                    last_session_status: entry.last_session_status,
                    last_session_updated_at: entry.last_session_updated_at,
                    last_injection_status: entry.last_injection_status,
                },
            )
        })
        .collect();
    cards.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.game_id.cmp(&b.1.game_id)));
    cards.into_iter().map(|(_, item)| item).collect()
}

fn build_rule_conflicts(store: &PersistedStore) -> Vec<RuleConflictItem> {
    let mut grouped: HashMap<String, Vec<&GameSaveRule>> = HashMap::new();
    for rule in &store.rules {
        let normalized_hash = normalize_exe_hash(&rule.exe_hash);
        if normalized_hash.is_empty() {
            continue;
        }
        grouped.entry(normalized_hash).or_default().push(rule);
    }

    let mut output = Vec::new();
    for (exe_hash, group_rules) in grouped {
        if group_rules.len() <= 1 {
            continue;
        }
        let mut rule_ids = group_rules
            .iter()
            .map(|rule| rule.rule_id.clone())
            .collect::<Vec<_>>();
        rule_ids.sort();
        let mut game_ids = group_rules
            .iter()
            .map(|rule| rule.game_id.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        game_ids.sort();
        let primary_rule_id = resolve_preferred_rule_id_for_exe_hash(&store.execution_config, &exe_hash)
            .filter(|rule_id| rule_ids.iter().any(|item| item == rule_id));
        output.push(RuleConflictItem {
            exe_hash,
            rule_ids: rule_ids.clone(),
            game_ids,
            primary_rule_id,
            conflict_count: rule_ids.len(),
        });
    }
    output.sort_by(|a, b| {
        b.conflict_count
            .cmp(&a.conflict_count)
            .then_with(|| a.exe_hash.cmp(&b.exe_hash))
    });
    output
}

fn select_rule_for_game(store: &PersistedStore, game_id: &str) -> Option<GameSaveRule> {
    let game_key = normalize_game_key(game_id);
    if game_key.is_empty() {
        return None;
    }

    let preferred_uid = store
        .execution_config
        .preferred_rule_uid_by_game
        .get(&game_key)
        .map(|uid| normalize_game_uid(uid))
        .filter(|uid| !uid.is_empty());

    if let Some(target_uid) = preferred_uid {
        let preferred = store
            .rules
            .iter()
            .filter(|rule| {
                normalize_game_key(&rule.game_id) == game_key
                    && normalize_game_uid(&rule.game_uid) == target_uid
            })
            .max_by_key(|rule| (rule.enabled as u8, rule_updated_ts(rule)))
            .cloned();
        if preferred.is_some() {
            return preferred;
        }
    }

    store
        .rules
        .iter()
        .filter(|rule| normalize_game_key(&rule.game_id) == game_key)
        .max_by_key(|rule| (rule.enabled as u8, rule_updated_ts(rule)))
        .cloned()
}

fn file_signature(path: &Path) -> Option<(u64, u64)> {
    let metadata = path.metadata().ok()?;
    let size = metadata.len();
    let modified = metadata.modified().ok().and_then(system_time_to_unix).unwrap_or(0);
    Some((size, modified))
}

fn restore_latest_backup_for_rule(
    rule: &GameSaveRule,
    backup_root: &str,
    exe_path: Option<&str>,
) -> Result<usize, String> {
    let base = ensure_backup_root_for_rule(rule, backup_root)?;
    let latest = base.join("latest");
    if !latest.exists() {
        return Ok(0);
    }
    let mut copied = 0_usize;
    for source in &rule.confirmed_paths {
        let runtime_path = expand_confirmed_path_for_runtime(source, exe_path)?;
        let target_path = Path::new(&runtime_path);
        let slot = backup_slot_name(source);
        let mut slot_source = latest.join(&slot);
        if !slot_source.exists() {
            let legacy_slot = backup_slot_name_legacy(source);
            slot_source = latest.join(legacy_slot);
        }
        copied += sync_directory(&slot_source, target_path)?;
    }
    Ok(copied)
}

fn list_version_directories(versions_dir: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    if !versions_dir.exists() {
        return Ok(Vec::new());
    }
    let mut dirs = fs::read_dir(versions_dir)
        .map_err(|err| format!("读取版本目录失败: {err}"))?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|entry| {
            let version_id = entry.file_name().to_string_lossy().to_string();
            if version_id.trim().is_empty() {
                return None;
            }
            Some((version_id, entry.path()))
        })
        .collect::<Vec<_>>();
    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(dirs)
}

fn directory_total_bytes(path: &Path) -> u64 {
    if !path.exists() {
        return 0;
    }
    WalkDir::new(path)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| entry.metadata().ok().map(|meta| meta.len()))
        .sum()
}

fn backup_version_label(version_id: &str) -> String {
    if version_id.starts_with("pre_restore_") {
        "回滚前备份".to_string()
    } else {
        "自动备份".to_string()
    }
}

fn normalize_backup_manifest_relative_path(relative: &Path) -> String {
    normalize_windows_path(&relative.to_string_lossy())
}

fn build_backup_manifest(version_root: &Path, snapshot_id: &str, game_uid: &str) -> Result<BackupManifest, String> {
    let mut files = Vec::new();
    for entry in WalkDir::new(version_root).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(version_root)
            .map_err(|err| format!("计算备份文件相对路径失败: {err}"))?;
        let relative_path = normalize_backup_manifest_relative_path(relative);
        if relative_path.is_empty() {
            continue;
        }
        let size = entry
            .metadata()
            .map_err(|err| format!("读取备份文件元信息失败: {err}"))?
            .len();
        let sha256 = file_sha256_hex_with_context(entry.path(), "备份文件")?;
        files.push(BackupManifestFileItem {
            path: relative_path,
            size,
            sha256,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(BackupManifest {
        format: "full-v2".to_string(),
        created_at: snapshot_id.to_string(),
        game_uid: normalize_game_uid(game_uid),
        files,
    })
}

fn verify_backup_manifest_integrity(version_dir: &Path) -> Result<BackupManifest, String> {
    let manifest_path = version_dir.join("manifest.json");
    if !manifest_path.exists() {
        return Err("备份版本缺少 manifest.json".to_string());
    }
    let manifest_raw = fs::read(&manifest_path).map_err(|err| format!("读取备份 manifest 失败: {err}"))?;
    let manifest_text = decode_text_bytes(&manifest_raw);
    let manifest: BackupManifest =
        serde_json::from_str(&manifest_text).map_err(|err| format!("解析备份 manifest 失败: {err}"))?;
    if manifest.files.is_empty() {
        return Err("该备份版本缺少完整性清单（旧格式），请先重新生成新备份再回滚".to_string());
    }

    let mut actual_files: HashMap<String, (u64, String)> = HashMap::new();
    for entry in WalkDir::new(version_dir).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(version_dir)
            .map_err(|err| format!("计算备份文件相对路径失败: {err}"))?;
        let relative_path = normalize_backup_manifest_relative_path(relative);
        if relative_path == "manifest.json" {
            continue;
        }
        let size = entry
            .metadata()
            .map_err(|err| format!("读取备份文件元信息失败: {err}"))?
            .len();
        let sha256 = file_sha256_hex_with_context(entry.path(), "备份文件")?;
        actual_files.insert(relative_path, (size, sha256));
    }

    let mut manifest_paths = HashSet::new();
    for item in &manifest.files {
        let normalized_path = normalize_windows_path(&item.path);
        if normalized_path.is_empty() {
            return Err("备份 manifest 包含空路径条目".to_string());
        }
        if !manifest_paths.insert(normalized_path.clone()) {
            return Err(format!("备份 manifest 存在重复文件条目: {}", item.path));
        }
        let Some((actual_size, actual_sha)) = actual_files.get(&normalized_path) else {
            return Err(format!("备份文件缺失: {}", item.path));
        };
        if *actual_size != item.size {
            return Err(format!(
                "备份文件大小不一致: {} (manifest={}, actual={})",
                item.path, item.size, actual_size
            ));
        }
        if !actual_sha.eq_ignore_ascii_case(item.sha256.trim()) {
            return Err(format!("备份文件校验失败: {}", item.path));
        }
    }

    for actual_path in actual_files.keys() {
        if !manifest_paths.contains(actual_path) {
            return Err(format!("备份版本存在未登记文件: {actual_path}"));
        }
    }
    Ok(manifest)
}

fn sync_source_to_backup_slot(source: &Path, latest_slot: &Path, version_slot: &Path) -> Result<usize, String> {
    if !source.exists() {
        if latest_slot.exists() {
            fs::remove_dir_all(latest_slot).map_err(|err| format!("清理旧缓存失败: {err}"))?;
        }
        return Ok(0);
    }
    fs::create_dir_all(latest_slot).map_err(|err| format!("创建目录失败: {err}"))?;

    let mut changed = 0_usize;
    let mut source_files = HashSet::new();
    for entry in WalkDir::new(source).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = match entry.path().strip_prefix(source) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let key = normalize_windows_path(&relative.to_string_lossy());
        source_files.insert(key);
        let latest_file = latest_slot.join(relative);
        let source_sig = file_signature(entry.path());
        let latest_sig = file_signature(&latest_file);
        if source_sig != latest_sig {
            if let Some(parent) = latest_file.parent() {
                fs::create_dir_all(parent).map_err(|err| format!("创建目录失败: {err}"))?;
            }
            fs::copy(entry.path(), &latest_file).map_err(|err| format!("复制文件失败: {err}"))?;
            let version_file = version_slot.join(relative);
            if let Some(parent) = version_file.parent() {
                fs::create_dir_all(parent).map_err(|err| format!("创建目录失败: {err}"))?;
            }
            fs::copy(entry.path(), &version_file).map_err(|err| format!("复制文件失败: {err}"))?;
            changed += 1;
        }
    }

    let mut stale_files = Vec::new();
    for entry in WalkDir::new(latest_slot).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = match entry.path().strip_prefix(latest_slot) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let key = normalize_windows_path(&relative.to_string_lossy());
        if !source_files.contains(&key) {
            stale_files.push(entry.path().to_path_buf());
        }
    }
    let stale_count = stale_files.len();
    for stale in stale_files {
        let _ = fs::remove_file(stale);
    }
    if stale_count > 0 {
        changed += stale_count;
    }

    if changed > 0 {
        if version_slot.exists() {
            fs::remove_dir_all(version_slot).map_err(|err| format!("清理版本快照失败: {err}"))?;
        }
        let _ = sync_directory(source, version_slot)?;
    }

    Ok(changed)
}

fn cleanup_backup_versions(versions_dir: &Path, keep: usize) -> Result<(), String> {
    if !versions_dir.exists() {
        return Ok(());
    }
    let mut dirs = fs::read_dir(versions_dir)
        .map_err(|err| format!("读取版本目录失败: {err}"))?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .collect::<Vec<_>>();
    dirs.sort_by_key(|entry| entry.file_name());
    if dirs.len() <= keep {
        return Ok(());
    }
    let remove_count = dirs.len() - keep;
    for entry in dirs.into_iter().take(remove_count) {
        fs::remove_dir_all(entry.path()).map_err(|err| format!("清理旧版本失败: {err}"))?;
    }
    Ok(())
}

fn backup_current_state_for_rule(
    rule: &GameSaveRule,
    backup_root: &str,
    keep_versions: usize,
    version_prefix: Option<&str>,
    exe_path: Option<&str>,
) -> Result<(usize, String), String> {
    let base = ensure_backup_root_for_rule(rule, backup_root)?;
    let latest = base.join("latest");
    let versions = base.join("versions");
    fs::create_dir_all(&latest).map_err(|err| format!("创建目录失败: {err}"))?;
    fs::create_dir_all(&versions).map_err(|err| format!("创建目录失败: {err}"))?;
    let snapshot_id = match version_prefix {
        Some(prefix) => format!("{}_{}", prefix.trim_matches('_'), now_unix()),
        None => now_unix().to_string(),
    };
    let version_root = versions.join(&snapshot_id);

    let mut changed_total = 0_usize;
    for source in &rule.confirmed_paths {
        let runtime_path = expand_confirmed_path_for_runtime(source, exe_path)?;
        let runtime_source = Path::new(&runtime_path);
        let slot = backup_slot_name(source);
        let changed = sync_source_to_backup_slot(
            runtime_source,
            &latest.join(&slot),
            &version_root.join(&slot),
        )?;
        changed_total += changed;
    }
    if changed_total == 0 && version_root.exists() {
        let _ = fs::remove_dir_all(&version_root);
    } else if changed_total > 0 {
        let manifest_path = version_root.join("manifest.json");
        let manifest = match build_backup_manifest(&version_root, &snapshot_id, &rule.game_uid) {
            Ok(value) => value,
            Err(err) => {
                let _ = fs::remove_dir_all(&version_root);
                return Err(err);
            }
        };
        if let Err(err) = write_pretty_json_file(&manifest_path, &manifest) {
            let _ = fs::remove_dir_all(&version_root);
            return Err(format!("写入备份 manifest 失败: {err}"));
        }
    }
    cleanup_backup_versions(&versions, keep_versions)?;
    Ok((changed_total, snapshot_id))
}

fn backup_after_exit_for_rule(
    rule: &GameSaveRule,
    backup_root: &str,
    keep_versions: usize,
    exe_path: Option<&str>,
) -> Result<(usize, String), String> {
    backup_current_state_for_rule(rule, backup_root, keep_versions, None, exe_path)
}

fn spawn_post_exit_backup_worker(
    app: AppHandle,
    session_id: String,
    mut child: std::process::Child,
    rule: GameSaveRule,
    backup_root: String,
    keep_versions: usize,
    exe_path: String,
) {
    std::thread::spawn(move || {
        let _ = child.wait();
        let backup_result = backup_after_exit_for_rule(&rule, &backup_root, keep_versions, Some(&exe_path));
        let state: State<AppState> = app.state();
        let lock_result = state.store.lock();
        if let Ok(mut store) = lock_result {
            if let Some(session) = store
                .launcher_sessions
                .iter_mut()
                .find(|item| item.launcher_session_id == session_id)
            {
                match backup_result {
                    Ok((count, snapshot)) => {
                        append_session_log(
                            session,
                            &format!(
                                "自动备份完成：文件 {}，版本 {}，保留最近 {} 版",
                                count, snapshot, keep_versions
                            ),
                        );
                    }
                    Err(err) => {
                        append_session_log(session, &format!("自动备份失败：{err}"));
                    }
                }
                session.status = "exited".to_string();
                session.updated_at = now_iso_string();
            }
            let _ = persist_store(&app, &store);
        };
    });
}

fn run_sandbox_launch_flow(
    exe_path: &str,
    rule: &GameSaveRule,
    execution_config: &ExecutionConfig,
    session: &mut LauncherSession,
) -> Result<SandboxLaunchResult, String> {
    let start_exe = resolve_sandboxie_start_path(execution_config)?;
    let user_name = std::env::var("USERNAME").unwrap_or_else(|_| "Default".to_string());
    let box_name = sanitize_sandbox_box_name(&rule.game_id);
    let redirect_root = join_managed_root_for_game(&execution_config.managed_save_root, &rule.game_id);
    fs::create_dir_all(&redirect_root).map_err(|err| format!("创建统一存档目录失败: {err}"))?;

    let mut mirror_paths = Vec::new();
    let mut copied_in = 0_usize;
    for original in &rule.confirmed_paths {
        let runtime_path = expand_confirmed_path_for_runtime(original, Some(exe_path))?;
        let mirror =
            build_sandbox_mirror_path(&runtime_path, &execution_config.sandbox_root, &user_name, &box_name)?;
        copied_in += sync_directory(Path::new(&redirect_root), &mirror)?;
        mirror_paths.push(mirror.to_string_lossy().to_string());
    }
    append_session_log(
        session,
        &format!(
            "沙盒预同步完成，文件 {}，沙盒 {}",
            copied_in, box_name
        ),
    );

    let box_arg = format!("/box:{box_name}");
    let child = Command::new(&start_exe)
        .args([&box_arg, exe_path])
        .spawn()
        .map_err(|err| format!("调用 Sandboxie 启动失败: {err}"))?;

    Ok(SandboxLaunchResult {
        pid: Some(child.id()),
        box_name,
        mirror_paths,
    })
}

fn resolve_redirector_artifacts(app: &AppHandle) -> RedirectArtifacts {
    let injector_name = "gamesaver-injector.exe";
    let dll_name = "gamesaver-hook.dll";

    let app_data_base = app
        .path()
        .app_data_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("redirector")
        .join("bin");
    let manifest_base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("redirector").join("bin");
    let exe_parent = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|v| v.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    let app_bin_base = exe_parent
        .join("redirector")
        .join("bin");
    let exe_resources_base = exe_parent.join("resources").join("redirector").join("bin");
    let sibling_resources_base = exe_parent
        .parent()
        .map(|v| v.join("resources").join("redirector").join("bin"))
        .unwrap_or_else(|| PathBuf::from(".").join("resources").join("redirector").join("bin"));

    let candidates = [
        app_data_base,
        manifest_base,
        app_bin_base,
        exe_resources_base,
        sibling_resources_base,
    ];
    for base in &candidates {
        let injector = base.join(injector_name);
        let dll = base.join(dll_name);
        if injector.exists() && dll.exists() {
            return RedirectArtifacts {
                injector_path: injector,
                dll_path: dll,
            };
        }
    }

    RedirectArtifacts {
        injector_path: candidates[1].join(injector_name),
        dll_path: candidates[1].join(dll_name),
    }
}

fn run_real_injection_flow(
    app: &AppHandle,
    pid: u32,
    rule: &GameSaveRule,
    execution_config: &ExecutionConfig,
    session: &mut LauncherSession,
) -> Result<InjectionRunResult, String> {
    let artifacts = resolve_redirector_artifacts(app);
    if !artifacts.injector_path.exists() {
        return Err(format!(
            "injector 不存在: {}",
            artifacts.injector_path.to_string_lossy()
        ));
    }
    if !artifacts.dll_path.exists() {
        return Err(format!("hook DLL 不存在: {}", artifacts.dll_path.to_string_lossy()));
    }

    let redirect_root = join_managed_root_for_game(&execution_config.managed_save_root, &rule.game_id);
    fs::create_dir_all(&redirect_root).map_err(|err| format!("创建重定向目录失败: {err}"))?;
    let config_path = write_redirect_config_file(pid, rule, &redirect_root)?;
    append_session_log(session, &format!("重定向根目录：{redirect_root}"));

    let output = {
        let mut command = Command::new(&artifacts.injector_path);
        command.args([
            "--pid",
            &pid.to_string(),
            "--dll",
            &artifacts.dll_path.to_string_lossy(),
            "--config",
            &config_path.to_string_lossy(),
        ]);
        apply_background_process_flags(&mut command)
            .output()
            .map_err(|err| format!("执行 injector 失败: {err}"))?
    };

    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stdout.is_empty() {
        append_session_log(session, &format!("injector: {stdout}"));
    }
    if !stderr.is_empty() {
        append_session_log(session, &format!("injector stderr: {stderr}"));
    }
    if !output.status.success() {
        return Err(format!("injector 返回失败 code={code}"));
    }

    Ok(InjectionRunResult {
        injector_exit_code: code,
        hook_version: "createfilew-iat-v1".to_string(),
    })
}

fn write_redirect_config_file(pid: u32, rule: &GameSaveRule, redirect_root: &str) -> Result<PathBuf, String> {
    let base = std::env::temp_dir().join("gamesaver");
    fs::create_dir_all(&base).map_err(|err| format!("创建临时目录失败: {err}"))?;
    let path = base.join(format!("redirect_config_{pid}.json"));
    let payload = serde_json::json!({
        "pid": pid,
        "gameId": rule.game_id,
        "gameUid": rule.game_uid,
        "confirmedPaths": rule.confirmed_paths,
        "redirectRoot": redirect_root,
        "logPath": base.join(format!("hook_{pid}.log")).to_string_lossy().to_string()
    });
    let content = serde_json::to_string_pretty(&payload).map_err(|err| format!("序列化注入配置失败: {err}"))?;
    fs::write(&path, content).map_err(|err| format!("写入注入配置失败: {err}"))?;
    Ok(path)
}

fn terminate_process(pid: u32) -> Result<(), String> {
    let output = {
        let mut command = Command::new("taskkill");
        command.args(["/PID", &pid.to_string(), "/T", "/F"]);
        apply_background_process_flags(&mut command)
            .output()
            .map_err(|err| format!("结束进程失败: {err}"))?
    };
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(format!("结束进程失败: {stderr}"))
    }
}

fn is_x64_pe(path: &Path) -> bool {
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    if bytes.len() < 0x40 {
        return false;
    }
    if bytes[0] != b'M' || bytes[1] != b'Z' {
        return false;
    }
    let pe_offset = u32::from_le_bytes([bytes[0x3c], bytes[0x3d], bytes[0x3e], bytes[0x3f]]) as usize;
    if bytes.len() < pe_offset + 6 {
        return false;
    }
    if bytes[pe_offset] != b'P' || bytes[pe_offset + 1] != b'E' {
        return false;
    }
    let machine = u16::from_le_bytes([bytes[pe_offset + 4], bytes[pe_offset + 5]]);
    machine == 0x8664
}

fn build_rule_key(game_id: &str, exe_hash: &str) -> String {
    format!(
        "{}::{}",
        game_id.trim().to_ascii_lowercase(),
        exe_hash.trim().to_ascii_lowercase()
    )
}

fn build_candidates(
    baseline: &Snapshot,
    final_snapshot: &Snapshot,
    game_id: &str,
    exe_path: &str,
    start_unix: u64,
    end_unix_with_grace: u64,
    related_files: Option<&HashSet<String>>,
) -> Vec<CandidatePath> {
    let mut grouped: HashMap<String, CandidateAccumulator> = HashMap::new();
    let game_id_lower = game_id.to_ascii_lowercase();
    let exe = Path::new(exe_path);
    let exe_dir = exe.parent().map(|path| normalize_windows_path(&path.to_string_lossy()));
    let exe_name = exe
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    for (path, final_meta) in &final_snapshot.files {
        if let Some(related) = related_files {
            if !related.is_empty() && !related.contains(&normalize_windows_path(path)) {
                continue;
            }
        }
        let changed = match baseline.files.get(path) {
            None => true,
            Some(base_meta) => {
                base_meta.modified_unix != final_meta.modified_unix || base_meta.size != final_meta.size
            }
        };
        if !changed {
            continue;
        }

        let raw_parent = Path::new(path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| path.clone());
        let parent = promote_candidate_parent(&raw_parent);
        let entry = grouped.entry(parent.clone()).or_insert_with(|| CandidateAccumulator {
            path: parent.clone(),
            ..CandidateAccumulator::default()
        });

        let is_added = !baseline.files.contains_key(path);
        if is_added {
            entry.added_files += 1;
        } else {
            entry.modified_files += 1;
        }
        entry.changed_files += 1;

        if final_meta.modified_unix >= start_unix && final_meta.modified_unix <= end_unix_with_grace {
            entry.time_hits += 1;
            entry.signals.insert("time-window".to_string());
        }

        if STRONG_SAVE_EXTENSIONS.contains(&final_meta.extension.as_str()) {
            entry.extension_hits += 1;
            entry.signals.insert(format!("extension:{}", final_meta.extension));
        } else if WEAK_SAVE_EXTENSIONS.contains(&final_meta.extension.as_str()) {
            entry.weak_extension_hits += 1;
            entry.signals.insert(format!("weak-extension:{}", final_meta.extension));
        }

        let lower_parent = parent.to_ascii_lowercase();
        if matches_save_path_keyword(&lower_parent) {
            entry.keyword_hits += 1;
            entry.signals.insert("save-path-keyword".to_string());
        }

        if matches_game_name_keyword(&lower_parent, &game_id_lower, &exe_name) {
            entry.game_name_hits += 1;
            entry.signals.insert("game-name-path".to_string());
        }

        if is_weak_candidate_path(&lower_parent) {
            entry.noise_hits += 1;
            entry.signals.insert("path-noise".to_string());
        }

        if is_user_save_root_path(&lower_parent) {
            entry.user_save_root_hits += 1;
            entry.signals.insert("user-save-root".to_string());
        }

        if exe_dir
            .as_ref()
            .is_some_and(|dir| normalize_windows_path(&parent).starts_with(dir))
        {
            entry.game_dir_hits += 1;
            entry.signals.insert("game-dir".to_string());
        }

        let file_name = Path::new(path)
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if matches_filename_keyword(&file_name, &FILENAME_SAVE_KEYWORDS) {
            entry.filename_hits += 1;
            entry.signals.insert("save-filename".to_string());
        }
        if matches_filename_keyword(&file_name, &NOISE_FILENAME_KEYWORDS) {
            entry.noise_filename_hits += 1;
            entry.signals.insert("filename-noise".to_string());
        }

        if final_meta.size > 0 && final_meta.size < 200 * 1024 * 1024 {
            entry.reasonable_size_hits += 1;
            entry.signals.insert("size-reasonable".to_string());
        }
    }

    let mut output = grouped
        .into_values()
        .map(CandidateAccumulator::into_candidate)
        .collect::<Vec<_>>();
    output.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| recommendation_rank(&b.recommendation).cmp(&recommendation_rank(&a.recommendation)))
            .then_with(|| b.changed_files.cmp(&a.changed_files))
            .then_with(|| a.path.cmp(&b.path))
    });
    output.truncate(10);
    output
}

fn collect_snapshot(game_id: &str, exe_path: &str) -> Result<Snapshot, String> {
    let roots = collect_scan_roots(exe_path)?;
    let mut files = HashMap::new();
    for root in roots {
        if !root.exists() {
            continue;
        }
        for entry in WalkDir::new(&root).into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            if should_ignore_candidate_path(entry.path()) {
                continue;
            }
            let metadata = match entry.metadata() {
                Ok(value) => value,
                Err(_) => continue,
            };
            let modified_unix = metadata
                .modified()
                .ok()
                .and_then(system_time_to_unix)
                .unwrap_or_default();
            let size = metadata.len();
            let extension = entry
                .path()
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            files.insert(
                entry.path().to_string_lossy().to_string(),
                FileMeta {
                    size,
                    modified_unix,
                    extension,
                },
            );
        }
    }

    Ok(Snapshot {
        snapshot_ref: format!("snapshot_{}_{}.json", game_id, now_unix()),
        created_at_unix: now_unix(),
        files,
    })
}

fn collect_scan_roots(exe_path: &str) -> Result<Vec<PathBuf>, String> {
    let profile = std::env::var("USERPROFILE").map_err(|_| "读取 USERPROFILE 失败".to_string())?;
    let mut roots = vec![
        Path::new(&profile).join("Documents"),
        Path::new(&profile).join("AppData").join("Local"),
        Path::new(&profile).join("AppData").join("LocalLow"),
        Path::new(&profile).join("AppData").join("Roaming"),
    ];
    if let Some(exe_dir) = Path::new(exe_path).parent() {
        roots.push(exe_dir.to_path_buf());
    }
    Ok(roots)
}

fn collect_process_tree_pids(root_pid: u32) -> Result<Vec<u32>, String> {
    let script = "Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId | ConvertTo-Json -Compress";
    let mut command = Command::new("powershell");
    command.args(["-NoProfile", "-Command", script]);
    let output = apply_background_process_flags(&mut command)
        .output()
        .map_err(|err| format!("读取进程列表失败: {err}"))?;
    if !output.status.success() {
        return Err("读取进程列表失败: powershell 命令返回异常".to_string());
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        return Ok(vec![root_pid]);
    }

    let rows = if stdout.starts_with('[') {
        serde_json::from_str::<Vec<CimProcessRow>>(&stdout)
            .map_err(|err| format!("解析进程列表失败: {err}"))?
    } else {
        let single = serde_json::from_str::<CimProcessRow>(&stdout)
            .map_err(|err| format!("解析进程列表失败: {err}"))?;
        vec![single]
    };

    let mut child_map: HashMap<u32, Vec<u32>> = HashMap::new();
    for row in rows {
        child_map
            .entry(row.parent_process_id)
            .or_default()
            .push(row.process_id);
    }

    let mut tracked = Vec::new();
    let mut visited = HashSet::new();
    let mut stack = vec![root_pid];

    while let Some(current) = stack.pop() {
        if !visited.insert(current) {
            continue;
        }
        tracked.push(current);
        if let Some(children) = child_map.get(&current) {
            for child in children {
                stack.push(*child);
            }
        }
    }

    tracked.sort_unstable();
    Ok(tracked)
}

fn try_start_etw_capture(app: &AppHandle, session_id: &str) -> Result<EventCaptureHandle, String> {
    if !is_running_as_admin() {
        return Err("当前非管理员权限，ETW 已自动降级为 snapshot 模式。".to_string());
    }

    let trace_name = format!("GameSaverTrace_{}", session_id.replace('-', "").chars().take(10).collect::<String>());
    let etl_path = event_logs_dir(app)?.join(format!("{trace_name}.etl"));
    let etl_path_str = etl_path.to_string_lossy().to_string();

    let _ = {
        let mut command = Command::new("logman");
        command.args(["stop", &trace_name, "-ets"]);
        apply_background_process_flags(&mut command).output()
    };

    let created = {
        let mut command = Command::new("logman");
        command.args([
            "create",
            "trace",
            &trace_name,
            "-o",
            &etl_path_str,
            "-p",
            "Microsoft-Windows-Kernel-File",
            "0x10",
            "5",
            "-ets",
        ]);
        apply_background_process_flags(&mut command)
            .output()
            .map_err(|err| format!("启动 ETW 失败（无法执行 logman）: {err}"))?
    };
    if !created.status.success() {
        let stderr = String::from_utf8_lossy(&created.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&created.stdout).trim().to_string();
        let msg = if !stderr.is_empty() { stderr } else { stdout };
        return Err(format!("启动 ETW 失败（logman create trace）: {msg}"));
    }

    Ok(EventCaptureHandle { trace_name, etl_path })
}

fn is_running_as_admin() -> bool {
    let mut command = Command::new("powershell");
    command.args([
        "-NoProfile",
        "-Command",
        "([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)",
    ]);
    let output = apply_background_process_flags(&mut command).output();
    let Ok(out) = output else {
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout).trim().to_ascii_lowercase();
    text.contains("true")
}

fn collect_related_files_by_trace(
    trace_name: Option<&str>,
    trace_path: Option<&str>,
    tracked_pids: &[u32],
) -> Result<HashSet<String>, String> {
    let mut files = HashSet::new();
    let Some(name) = trace_name else {
        return Ok(files);
    };
    let Some(etl_path) = trace_path else {
        return Ok(files);
    };

    let _ = {
        let mut command = Command::new("logman");
        command.args(["stop", name, "-ets"]);
        apply_background_process_flags(&mut command).output()
    };

    let csv_path = format!("{etl_path}.csv");
    let converted = {
        let mut command = Command::new("tracerpt");
        command.args([etl_path, "-of", "CSV", "-o", &csv_path, "-y"]);
        apply_background_process_flags(&mut command)
            .output()
            .map_err(|err| format!("解析 ETW 失败（无法执行 tracerpt）: {err}"))?
    };
    if !converted.status.success() {
        let stderr = String::from_utf8_lossy(&converted.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&converted.stdout).trim().to_string();
        let msg = if !stderr.is_empty() { stderr } else { stdout };
        return Err(format!("解析 ETW 失败（tracerpt）: {msg}"));
    }

    let raw = fs::read(&csv_path).map_err(|err| format!("读取 ETW CSV 失败: {err}"))?;
    let content = decode_text_bytes(&raw);
    let mut lines = content.lines();
    let Some(header_line) = lines.next() else {
        return Ok(files);
    };
    let headers = parse_csv_line(header_line);
    let pid_idx = find_header_index(&headers, &["processid", "process id", "pid"]);
    let path_idx = find_header_index(&headers, &["filename", "file name", "filepath", "file path", "pathname", "path"]);
    let op_idx = find_header_index(&headers, &["opcode", "opcode name", "task", "task name", "eventname", "event name", "operation"]);

    let pid_set = tracked_pids.iter().copied().collect::<HashSet<u32>>();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let row = parse_csv_line(trimmed);
        let Some(pid_index) = pid_idx else {
            continue;
        };
        let Some(pid_value) = row.get(pid_index) else {
            continue;
        };
        let Some(pid) = parse_u32(pid_value) else {
            continue;
        };
        if !pid_set.is_empty() && !pid_set.contains(&pid) {
            continue;
        }

        let op_text = op_idx
            .and_then(|idx| row.get(idx))
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        let is_write_like = op_text.is_empty()
            || op_text.contains("write")
            || op_text.contains("create")
            || op_text.contains("setinfo")
            || op_text.contains("rename");
        if !is_write_like {
            continue;
        }

        let extracted = path_idx
            .and_then(|idx| row.get(idx).cloned())
            .or_else(|| row.iter().find_map(|cell| extract_windows_path(cell)));
        let Some(path) = extracted else {
            continue;
        };
        let normalized = normalize_windows_path(&path);
        if !normalized.is_empty() && !should_ignore_candidate_path(Path::new(&normalized)) {
            files.insert(normalized);
        }
    }

    Ok(files)
}

fn extract_windows_path(text: &str) -> Option<String> {
    let chars = text.chars().collect::<Vec<_>>();
    for i in 0..chars.len().saturating_sub(2) {
        if chars[i].is_ascii_alphabetic() && chars[i + 1] == ':' && chars[i + 2] == '\\' {
            let mut end = i + 3;
            while end < chars.len() {
                let c = chars[end];
                if c == '"' || c == ',' {
                    break;
                }
                end += 1;
            }
            let path = chars[i..end].iter().collect::<String>();
            if path.len() > 4 {
                return Some(path);
            }
        }
    }
    None
}

fn normalize_windows_path(path: &str) -> String {
    path.replace('/', "\\").to_ascii_lowercase()
}

fn parse_csv_line(line: &str) -> Vec<String> {
    let mut output = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                if in_quotes && chars.peek().is_some_and(|next| *next == '"') {
                    current.push('"');
                    let _ = chars.next();
                } else {
                    in_quotes = !in_quotes;
                }
            }
            ',' if !in_quotes => {
                output.push(current.trim().trim_matches('"').to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    output.push(current.trim().trim_matches('"').to_string());
    output
}

fn find_header_index(headers: &[String], keywords: &[&str]) -> Option<usize> {
    headers.iter().position(|header| {
        let normalized = header
            .to_ascii_lowercase()
            .replace(' ', "")
            .replace('_', "");
        keywords.iter().any(|keyword| {
            let key = keyword
                .to_ascii_lowercase()
                .replace(' ', "")
                .replace('_', "");
            normalized == key || normalized.contains(&key)
        })
    })
}

fn parse_u32(text: &str) -> Option<u32> {
    let digits = text
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if digits.is_empty() {
        None
    } else {
        digits.parse::<u32>().ok()
    }
}

fn decode_text_bytes(raw: &[u8]) -> String {
    if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(&raw[3..]).to_string();
    }
    if raw.starts_with(&[0xFF, 0xFE]) {
        let mut units = Vec::new();
        let mut i = 2;
        while i + 1 < raw.len() {
            units.push(u16::from_le_bytes([raw[i], raw[i + 1]]));
            i += 2;
        }
        return String::from_utf16_lossy(&units);
    }
    if raw.starts_with(&[0xFE, 0xFF]) {
        let mut units = Vec::new();
        let mut i = 2;
        while i + 1 < raw.len() {
            units.push(u16::from_be_bytes([raw[i], raw[i + 1]]));
            i += 2;
        }
        return String::from_utf16_lossy(&units);
    }

    if let Ok(text) = String::from_utf8(raw.to_vec()) {
        return text;
    }

    if raw.len() >= 2 {
        let mut units = Vec::new();
        let mut i = 0;
        while i + 1 < raw.len() {
            units.push(u16::from_le_bytes([raw[i], raw[i + 1]]));
            i += 2;
        }
        let utf16 = String::from_utf16_lossy(&units);
        if utf16.chars().any(|ch| ch == ',' || ch == '\n') {
            return utf16;
        }
    }

    String::from_utf8_lossy(raw).to_string()
}

fn should_ignore_candidate_path(path: &Path) -> bool {
    let lower = path.to_string_lossy().to_ascii_lowercase();
    if lower.contains(APP_IDENTIFIER) {
        return true;
    }
    NOISE_PATH_FRAGMENTS
        .iter()
        .any(|fragment| lower.contains(fragment))
}

fn is_weak_candidate_path(lower_path: &str) -> bool {
    WEAK_PATH_FRAGMENTS
        .iter()
        .any(|fragment| lower_path.contains(fragment))
}

fn is_user_save_root_path(lower_path: &str) -> bool {
    lower_path.contains("\\appdata\\locallow\\")
        || lower_path.contains("\\appdata\\local\\")
        || lower_path.contains("\\appdata\\roaming\\")
        || lower_path.contains("\\documents\\")
        || lower_path.contains("\\saved games\\")
}

fn matches_save_path_keyword(lower_path: &str) -> bool {
    split_path_words(lower_path).iter().any(|segment| {
        PATH_KEYWORDS
            .iter()
            .any(|keyword| *segment == *keyword || segment.starts_with(keyword))
    })
}

fn matches_game_name_keyword(lower_path: &str, game_id_lower: &str, exe_name_lower: &str) -> bool {
    let segments = split_path_words(lower_path);
    let game_id_hit = if game_id_lower.trim().is_empty() {
        false
    } else {
        let compact_game_id = game_id_lower.replace(['-', '_', ' '], "");
        segments
            .iter()
            .any(|segment| segment.contains(game_id_lower) || segment.contains(&compact_game_id))
    };
    let exe_name_hit = if exe_name_lower.trim().is_empty() {
        false
    } else {
        let compact_exe = exe_name_lower.replace(['-', '_', ' '], "");
        segments
            .iter()
            .any(|segment| segment.contains(exe_name_lower) || segment.contains(&compact_exe))
    };
    game_id_hit || exe_name_hit
}

fn matches_filename_keyword(file_name_lower: &str, keywords: &[&str]) -> bool {
    let compact = file_name_lower.replace(['-', '_', ' ', '.'], "");
    keywords
        .iter()
        .any(|keyword| file_name_lower.contains(keyword) || compact.contains(keyword))
}

fn promote_candidate_parent(parent: &str) -> String {
    let normalized = parent.replace('/', "\\");
    let parts = normalized.split('\\').collect::<Vec<_>>();
    for (index, part) in parts.iter().enumerate().rev() {
        let lower = part.to_ascii_lowercase();
        if PATH_KEYWORDS
            .iter()
            .any(|keyword| lower == *keyword || lower.starts_with(keyword))
        {
            return parts[..=index].join("\\");
        }
    }
    parent.to_string()
}

fn split_path_words(lower_path: &str) -> Vec<String> {
    lower_path
        .replace('/', "\\")
        .split(|ch: char| ch == '\\' || ch == '_' || ch == '-' || ch == '.')
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.to_string())
        .collect::<Vec<_>>()
}

fn recommendation_rank(recommendation: &str) -> i32 {
    match recommendation {
        "strong" => 4,
        "recommended" => 3,
        "possible" => 2,
        _ => 1,
    }
}

fn store_file_path(app: &AppHandle) -> Result<PathBuf, String> {
    let base = app
        .path()
        .app_data_dir()
        .map_err(|err| format!("解析 app_data 目录失败: {err}"))?;
    fs::create_dir_all(&base).map_err(|err| format!("创建 app_data 目录失败: {err}"))?;
    Ok(base.join("store.json"))
}

fn store_backup_file_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(store_file_path(app)?.with_extension("json.bak"))
}

fn snapshots_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let base = app
        .path()
        .app_data_dir()
        .map_err(|err| format!("解析 snapshots 目录失败: {err}"))?
        .join("snapshots");
    fs::create_dir_all(&base).map_err(|err| format!("创建 snapshots 目录失败: {err}"))?;
    Ok(base)
}

fn event_logs_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let base = app
        .path()
        .app_data_dir()
        .map_err(|err| format!("解析 events 目录失败: {err}"))?
        .join("events");
    fs::create_dir_all(&base).map_err(|err| format!("创建 events 目录失败: {err}"))?;
    Ok(base)
}

fn write_snapshot(app: &AppHandle, snapshot_ref: &str, snapshot: &Snapshot) -> Result<(), String> {
    let path = snapshots_dir(app)?.join(snapshot_ref);
    let content = serde_json::to_string(snapshot).map_err(|err| format!("序列化快照失败: {err}"))?;
    fs::write(path, content).map_err(|err| format!("写入快照失败: {err}"))
}

fn read_snapshot(app: &AppHandle, snapshot_ref: &str) -> Result<Snapshot, String> {
    let path = snapshots_dir(app)?.join(snapshot_ref);
    let content = fs::read_to_string(path).map_err(|err| format!("读取快照失败: {err}"))?;
    serde_json::from_str(&content).map_err(|err| format!("反序列化快照失败: {err}"))
}

fn load_store(app: &AppHandle) -> Result<PersistedStore, String> {
    let path = store_file_path(app)?;
    if !path.exists() {
        return Ok(PersistedStore::default());
    }
    let raw = fs::read(&path).map_err(|err| format!("读取 store 失败: {err}"))?;
    let content = decode_text_bytes(&raw);
    let parsed = serde_json::from_str::<PersistedStore>(&content);
    match parsed {
        Ok(mut store) => {
            normalize_store(&mut store);
            Ok(store)
        }
        Err(primary_err) => {
            let backup = store_backup_file_path(app)?;
            if backup.exists() {
                let backup_raw = fs::read(&backup).map_err(|err| format!("读取 store 备份失败: {err}"))?;
                let backup_content = decode_text_bytes(&backup_raw);
                if let Ok(mut store) = serde_json::from_str::<PersistedStore>(&backup_content) {
                    normalize_store(&mut store);
                    return Ok(store);
                }
            }
            Err(format!("解析 store 失败: {primary_err}"))
        }
    }
}

fn persist_store(app: &AppHandle, store: &PersistedStore) -> Result<(), String> {
    let content = serde_json::to_string_pretty(store).map_err(|err| format!("序列化 store 失败: {err}"))?;
    let path = store_file_path(app)?;
    let backup = store_backup_file_path(app)?;
    if path.exists() {
        let _ = fs::copy(&path, &backup);
    }
    let temp_path = path.with_extension("json.tmp");
    fs::write(&temp_path, content).map_err(|err| format!("写入临时 store 失败: {err}"))?;
    if path.exists() {
        fs::remove_file(&path).map_err(|err| format!("替换 store 前删除旧文件失败: {err}"))?;
    }
    fs::rename(&temp_path, &path).map_err(|err| format!("替换 store 失败: {err}"))
}

fn default_true() -> bool {
    true
}

fn normalize_store(store: &mut PersistedStore) {
    let current = store.execution_config.managed_save_root.trim().to_string();
    if current.is_empty() || current.eq_ignore_ascii_case(&legacy_managed_save_root()) {
        store.execution_config.managed_save_root = default_managed_save_root();
    }
    if store.execution_config.sandbox_root.trim().is_empty() {
        store.execution_config.sandbox_root = default_sandbox_root();
    }
    if store.execution_config.sandboxie_start_exe.trim().is_empty() {
        store.execution_config.sandboxie_start_exe = default_sandboxie_start_exe();
    }
    if store.execution_config.backup_root.trim().is_empty() {
        store.execution_config.backup_root = default_backup_root();
    }
    let mut migration_candidates: HashMap<String, (u64, String, String)> = HashMap::new();
    let mut valid_uids_by_game: HashMap<String, HashSet<String>> = HashMap::new();
    for rule in &mut store.rules {
        let normalized_uid = normalize_game_uid(&rule.game_uid);
        if normalized_uid.is_empty() {
            rule.game_uid = new_game_uid();
        } else {
            rule.game_uid = normalized_uid;
        }
        if rule.updated_at.trim().is_empty() {
            rule.updated_at = rule.created_at.clone();
        }
        let game_key = normalize_game_key(&rule.game_id);
        if !game_key.is_empty() {
            let ts = rule_updated_ts(rule);
            let uid = rule.game_uid.clone();
            let game_id = rule.game_id.clone();
            valid_uids_by_game
                .entry(game_key.clone())
                .or_default()
                .insert(uid.clone());
            let should_replace = migration_candidates
                .get(&game_key)
                .is_none_or(|(current_ts, _, _)| ts >= *current_ts);
            if should_replace {
                migration_candidates.insert(game_key, (ts, uid, game_id));
            }
        }
    }
    let mut game_key_uid_map: HashMap<String, String> = HashMap::new();
    for (game_key, (_ts, uid, _game_id)) in &migration_candidates {
        game_key_uid_map.insert(game_key.clone(), normalize_game_uid(uid));
    }

    let mut normalized_preferred_exe_by_uid: HashMap<String, String> = HashMap::new();
    for (uid_key, exe_path) in store.execution_config.preferred_exe_by_uid.clone() {
        let normalized_uid = normalize_game_uid(&uid_key);
        let trimmed_exe = exe_path.trim();
        if normalized_uid.is_empty() || trimmed_exe.is_empty() {
            continue;
        }
        normalized_preferred_exe_by_uid.insert(normalized_uid, trimmed_exe.to_string());
    }
    for (game_key, exe_path) in store.execution_config.preferred_exe_by_game_legacy.clone() {
        let normalized_game_key = normalize_game_key(&game_key);
        let trimmed_exe = exe_path.trim();
        if normalized_game_key.is_empty() || trimmed_exe.is_empty() {
            continue;
        }
        if let Some(uid) = game_key_uid_map.get(&normalized_game_key) {
            normalized_preferred_exe_by_uid
                .entry(uid.clone())
                .or_insert_with(|| trimmed_exe.to_string());
        }
    }
    store.execution_config.preferred_exe_by_uid = normalized_preferred_exe_by_uid;
    store.execution_config.preferred_exe_by_game_legacy = HashMap::new();

    for rule in &mut store.rules {
        let normalized_exe = if rule.game_uid.trim().is_empty() {
            None
        } else {
            store
                .execution_config
                .preferred_exe_by_uid
                .get(&normalize_game_uid(&rule.game_uid))
                .map(|value| value.as_str())
        };
        rule.confirmed_paths = normalize_paths(rule.confirmed_paths.clone(), normalized_exe);
    }

    let mut normalized_preferred_rule_uid_by_game: HashMap<String, String> = HashMap::new();
    for (game_key, uid) in store.execution_config.preferred_rule_uid_by_game.clone() {
        let normalized_game_key = normalize_game_key(&game_key);
        let normalized_uid = normalize_game_uid(&uid);
        if normalized_game_key.is_empty() || normalized_uid.is_empty() {
            continue;
        }
        if valid_uids_by_game
            .get(&normalized_game_key)
            .is_some_and(|uids| uids.contains(&normalized_uid))
        {
            normalized_preferred_rule_uid_by_game.insert(normalized_game_key, normalized_uid);
        }
    }
    store.execution_config.preferred_rule_uid_by_game = normalized_preferred_rule_uid_by_game;

    let mut valid_rule_ids_by_hash: HashMap<String, HashSet<String>> = HashMap::new();
    for rule in &store.rules {
        let normalized_hash = normalize_exe_hash(&rule.exe_hash);
        if normalized_hash.is_empty() {
            continue;
        }
        valid_rule_ids_by_hash
            .entry(normalized_hash)
            .or_default()
            .insert(rule.rule_id.clone());
    }
    let mut normalized_preferred_rule_id_by_exe_hash: HashMap<String, String> = HashMap::new();
    for (exe_hash, rule_id) in store.execution_config.preferred_rule_id_by_exe_hash.clone() {
        let normalized_hash = normalize_exe_hash(&exe_hash);
        let normalized_rule_id = rule_id.trim().to_string();
        if normalized_hash.is_empty() || normalized_rule_id.is_empty() {
            continue;
        }
        if valid_rule_ids_by_hash
            .get(&normalized_hash)
            .is_some_and(|ids| ids.contains(&normalized_rule_id))
        {
            normalized_preferred_rule_id_by_exe_hash.insert(normalized_hash, normalized_rule_id);
        }
    }
    store.execution_config.preferred_rule_id_by_exe_hash = normalized_preferred_rule_id_by_exe_hash;

    let mut normalized_backup_keep_versions_by_uid: HashMap<String, usize> = HashMap::new();
    for (uid_key, keep_versions) in store.execution_config.backup_keep_versions_by_uid.clone() {
        let normalized_uid = normalize_game_uid(&uid_key);
        if normalized_uid.is_empty() || keep_versions == 0 {
            continue;
        }
        normalized_backup_keep_versions_by_uid
            .insert(normalized_uid, normalize_backup_keep_versions(keep_versions));
    }
    store.execution_config.backup_keep_versions_by_uid = normalized_backup_keep_versions_by_uid;

    let game_keys = valid_uids_by_game.keys().cloned().collect::<Vec<_>>();
    for game_key in game_keys {
        let selected_uid = select_rule_for_game(store, &game_key).map(|rule| normalize_game_uid(&rule.game_uid));
        if let Some(uid) = selected_uid {
            if !uid.is_empty() {
                store
                    .execution_config
                    .preferred_rule_uid_by_game
                    .insert(game_key, uid);
            }
        }
    }

    let rule_uid_by_id: HashMap<String, String> = store
        .rules
        .iter()
        .map(|rule| (rule.rule_id.clone(), rule.game_uid.clone()))
        .collect();
    for session in &mut store.launcher_sessions {
        if session.launch_mode.trim().is_empty() {
            session.launch_mode = "backup".to_string();
        }
        if session.updated_at.trim().is_empty() {
            session.updated_at = session.started_at.clone();
        }
        if session.matched_game_uid.as_deref().unwrap_or("").trim().is_empty() {
            if let Some(rule_id) = session.matched_rule_id.as_ref() {
                if let Some(game_uid) = rule_uid_by_id.get(rule_id) {
                    session.matched_game_uid = Some(game_uid.clone());
                }
            }
        }
    }
    store.rules.retain(|rule| {
        !rule.game_id.trim().is_empty()
            && !rule.game_uid.trim().is_empty()
            && !rule.exe_hash.trim().is_empty()
            && !rule.confirmed_paths.is_empty()
    });
}

fn default_execution_config() -> ExecutionConfig {
    ExecutionConfig {
        managed_save_root: default_managed_save_root(),
        backup_root: default_backup_root(),
        block_on_inject_fail: true,
        sandbox_root: default_sandbox_root(),
        sandboxie_start_exe: default_sandboxie_start_exe(),
        preferred_exe_by_uid: HashMap::new(),
        preferred_rule_uid_by_game: HashMap::new(),
        preferred_rule_id_by_exe_hash: HashMap::new(),
        backup_keep_versions_by_uid: HashMap::new(),
        preferred_exe_by_game_legacy: HashMap::new(),
    }
}

fn default_managed_save_root() -> String {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|v| v.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    exe_dir
        .join("GameSaverSaves")
        .to_string_lossy()
        .to_string()
}

fn legacy_managed_save_root() -> String {
    let profile = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".to_string());
    Path::new(&profile)
        .join("Saved Games")
        .join("GameSaver")
        .to_string_lossy()
        .to_string()
}

fn default_backup_root() -> String {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|v| v.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    exe_dir
        .join("GameSaverBackups")
        .to_string_lossy()
        .to_string()
}

fn default_sandbox_root() -> String {
    "C:\\Sandbox".to_string()
}

fn default_sandboxie_start_exe() -> String {
    "C:\\Program Files\\Sandboxie-Plus\\Start.exe".to_string()
}

fn now_unix() -> u64 {
    system_time_to_unix(SystemTime::now()).unwrap_or_default()
}

fn system_time_to_unix(value: SystemTime) -> Option<u64> {
    value.duration_since(UNIX_EPOCH).ok().map(|duration| duration.as_secs())
}

fn now_iso_string() -> String {
    now_unix().to_string()
}

fn apply_background_process_flags(command: &mut Command) -> &mut Command {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

fn iso_to_unix(value: &str) -> Option<u64> {
    value.parse::<u64>().ok()
}

fn file_sha256_hex(path: &Path) -> Result<String, String> {
    file_sha256_hex_with_context(path, "exe")
}

fn file_sha256_hex_with_context(path: &Path, context: &str) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|err| format!("读取{context}失败: {err}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|err| format!("读取{context}内容失败: {err}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            store: Mutex::new(PersistedStore::default()),
            tasks: Mutex::new(HashMap::new()),
        })
        .setup(|app| {
            let loaded = match load_store(app.handle()) {
                Ok(store) => store,
                Err(err) => {
                    eprintln!("[GameSaver] load_store failed, using default store: {err}");
                    PersistedStore::default()
                }
            };
            let state: State<AppState> = app.state();
            if let Ok(mut guard) = state.store.lock() {
                *guard = loaded;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_learning,
            launch_game,
            start_finish_learning_task,
            get_task,
            finish_learning,
            confirm_rule,
            list_rules,
            list_rule_conflicts,
            set_primary_rule,
            update_rule,
            delete_rule,
            export_rules,
            import_rules,
            start_export_migration_zip_task,
            export_migration_zip,
            start_import_migration_zip_task,
            import_migration_zip,
            open_candidate_path,
            resolve_rule_for_exe,
            launch_with_rule,
            launch_game_from_library,
            precheck_game_launch,
            get_launcher_session,
            list_launcher_sessions,
            list_game_library_items,
            set_preferred_exe_path,
            get_redirect_runtime_info,
            sync_sandbox_session,
            get_backup_stats,
            set_backup_keep_versions,
            prune_backup_versions,
            list_backup_versions,
            start_restore_backup_version_task,
            restore_backup_version,
            get_learning_session,
            get_runtime_status,
            restart_as_admin
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
