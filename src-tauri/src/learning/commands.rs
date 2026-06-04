use crate::app_state::{AppState, BackgroundTask};
use crate::path_utils::{normalize_confirmed_path_for_storage, normalize_paths};
use crate::runtime::{apply_background_process_flags, file_sha256_hex, iso_to_unix, now_iso_string, now_unix};
use crate::shared::{CandidatePath, GameSaveRule, LearningSession, PersistedStore, Snapshot};
use crate::storage::{new_game_uid, JsonStoreRepository, StoreRepository};
use crate::task_support::update_background_task;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use tauri::{AppHandle, Manager, State};
use uuid::Uuid;

use super::analysis::{build_candidates, default_confidence};
use super::capture::collect_process_tree_pids;
use super::snapshot::{collect_snapshot, normalize_learning_scan_root, read_snapshot, write_snapshot};

fn build_rule_key(game_id: &str, exe_hash: &str) -> String {
    format!(
        "{}::{}",
        game_id.trim().to_ascii_lowercase(),
        exe_hash.trim().to_ascii_lowercase()
    )
}

fn persist_store(app: &AppHandle, store: &PersistedStore) -> Result<(), String> {
    JsonStoreRepository::new().persist(app, store)
}

fn normalize_learning_scan_roots(raw_roots: Vec<String>, state: &State<AppState>) -> Result<Vec<String>, String> {
    let mut normalized = Vec::new();
    let mut seen = HashSet::new();
    {
        let store = state
            .store
            .lock()
            .map_err(|_| "failed to lock app state".to_string())?;
        for root in &store.execution_config.extra_learning_scan_roots {
            if let Some(item) = normalize_learning_scan_root(root) {
                if seen.insert(item.clone()) {
                    normalized.push(item);
                }
            }
        }
    }
    for root in raw_roots {
        if let Some(item) = normalize_learning_scan_root(&root) {
            if seen.insert(item.clone()) {
                normalized.push(item);
            }
        }
    }
    Ok(normalized)
}

fn finish_learning_impl(
    app: &AppHandle,
    app_state: &AppState,
    session_id: &str,
    force_rerun: bool,
) -> Result<Vec<CandidatePath>, String> {
    let mut store = app_state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    let session = store
        .sessions
        .iter_mut()
        .find(|item| item.session_id == session_id)
        .ok_or_else(|| "sessionId not found".to_string())?;
    if let Some(root_pid) = session.pid {
        if let Ok(pids) = collect_process_tree_pids(root_pid) {
            session.tracked_pids = pids;
        }
    }

    if session.status != "running" && !force_rerun {
        return Ok(session.candidates.clone());
    }
    if force_rerun {
        session.status = "running".to_string();
        session.ended_at = None;
        session.final_snapshot_ref = None;
        session.candidates.clear();
        session.event_capture_mode = "snapshot".to_string();
        session.event_trace_name = None;
        session.event_trace_path = None;
        session.captured_event_count = 0;
        session.event_capture_error = None;
    }

    let end_unix = now_unix();
    let final_snapshot_ref = format!("final_{session_id}.json");
    let final_snapshot = collect_snapshot(&session.game_id, &session.exe_path, &session.extra_scan_roots)?;
    write_snapshot(app, &final_snapshot_ref, &final_snapshot)?;

    let baseline_snapshot: Snapshot = read_snapshot(app, &session.baseline_snapshot_ref)?;
    let candidates = build_candidates(
        &baseline_snapshot,
        &final_snapshot,
        &session.game_id,
        &session.exe_path,
        iso_to_unix(&session.started_at).unwrap_or(end_unix),
        end_unix + 120,
        None,
    );

    session.status = "finished".to_string();
    session.ended_at = Some(now_iso_string());
    session.final_snapshot_ref = Some(final_snapshot_ref);
    session.candidates = candidates.clone();

    persist_store(app, &store)?;
    Ok(candidates)
}

fn start_finish_learning_task_impl(
    app: AppHandle,
    state: State<AppState>,
    session_id: String,
    force_rerun: bool,
) -> Result<String, String> {
    let trimmed_session_id = session_id.trim().to_string();
    if trimmed_session_id.is_empty() {
        return Err("sessionId cannot be empty".to_string());
    }
    {
        let store = state
            .store
            .lock()
            .map_err(|_| "failed to lock app state".to_string())?;
        if !store
            .sessions
            .iter()
            .any(|item| item.session_id == trimmed_session_id)
        {
            return Err("sessionId not found".to_string());
        }
    }

    let task_id = Uuid::new_v4().to_string();
    let now = now_iso_string();
    let task = BackgroundTask {
        task_id: task_id.clone(),
        task_type: if force_rerun {
            "retry_finish_learning".to_string()
        } else {
            "finish_learning".to_string()
        },
        status: "pending".to_string(),
        progress: Some(0),
        message: Some("task created, waiting to run".to_string()),
        result: None,
        error: None,
        started_at: now.clone(),
        updated_at: now,
    };
    {
        let mut tasks = state
            .tasks
            .lock()
            .map_err(|_| "failed to lock tasks".to_string())?;
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
            Some(if force_rerun {
                "reanalyzing save changes...".to_string()
            } else {
                "analyzing save changes...".to_string()
            }),
            None,
            None,
        );
        let app_state: State<AppState> = app_handle.state();
        match finish_learning_impl(&app_handle, app_state.inner(), &session_id_for_thread, force_rerun) {
            Ok(candidates) => {
                update_background_task(
                    &app_handle,
                    &task_id_for_thread,
                    "success",
                    Some(100),
                    Some(format!("analysis completed, found {} candidates", candidates.len())),
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
                    Some("analysis failed".to_string()),
                    None,
                    Some(err),
                );
            }
        }
    });

    Ok(task_id)
}

#[tauri::command]
pub(crate) fn start_learning(
    app: AppHandle,
    state: State<AppState>,
    game_id: String,
    exe_path: String,
    extra_scan_roots: Option<Vec<String>>,
) -> Result<String, String> {
    if game_id.trim().is_empty() {
        return Err("gameId cannot be empty".to_string());
    }
    if exe_path.trim().is_empty() {
        return Err("exePath cannot be empty".to_string());
    }
    if !Path::new(&exe_path).exists() {
        return Err("exePath does not exist".to_string());
    }

    let normalized_extra_scan_roots =
        normalize_learning_scan_roots(extra_scan_roots.unwrap_or_default(), &state)?;

    let session_id = Uuid::new_v4().to_string();
    let snapshot_ref = format!("baseline_{session_id}.json");
    let snapshot = collect_snapshot(&game_id, &exe_path, &normalized_extra_scan_roots)?;
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
        extra_scan_roots: normalized_extra_scan_roots,
    };

    let mut store = state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    store.sessions.push(session);
    persist_store(&app, &store)?;
    Ok(session_id)
}

#[tauri::command]
pub(crate) fn launch_game(app: AppHandle, state: State<AppState>, session_id: String) -> Result<u32, String> {
    let mut store = state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    let session = store
        .sessions
        .iter_mut()
        .find(|item| item.session_id == session_id)
        .ok_or_else(|| "sessionId not found".to_string())?;

    let mut command = Command::new(&session.exe_path);
    if let Some(exe_dir) = PathBuf::from(&session.exe_path).parent().map(Path::to_path_buf) {
        command.current_dir(exe_dir);
    }
    let child = command
        .spawn()
        .map_err(|err| format!("failed to launch game: {err}"))?;

    let pid = child.id();
    session.pid = Some(pid);
    session.tracked_pids = collect_process_tree_pids(pid).unwrap_or_else(|_| vec![pid]);
    session.event_capture_mode = "snapshot".to_string();
    session.event_trace_name = None;
    session.event_trace_path = None;
    session.captured_event_count = 0;
    session.event_capture_error = None;
    persist_store(&app, &store)?;
    Ok(pid)
}

#[tauri::command]
pub(crate) fn start_finish_learning_task(
    app: AppHandle,
    state: State<AppState>,
    session_id: String,
) -> Result<String, String> {
    start_finish_learning_task_impl(app, state, session_id, false)
}

#[tauri::command]
pub(crate) fn start_retry_finish_learning_task(
    app: AppHandle,
    state: State<AppState>,
    session_id: String,
) -> Result<String, String> {
    start_finish_learning_task_impl(app, state, session_id, true)
}

#[tauri::command]
pub(crate) fn finish_learning(
    app: AppHandle,
    state: State<AppState>,
    session_id: String,
) -> Result<Vec<CandidatePath>, String> {
    finish_learning_impl(&app, state.inner(), &session_id, false)
}

#[tauri::command]
pub(crate) fn cancel_learning(
    app: AppHandle,
    state: State<AppState>,
    session_id: String,
) -> Result<(), String> {
    let mut store = state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    let session = store
        .sessions
        .iter_mut()
        .find(|item| item.session_id == session_id)
        .ok_or_else(|| "sessionId not found".to_string())?;
    session.status = "cancelled".to_string();
    session.ended_at = Some(now_iso_string());
    persist_store(&app, &store)
}

#[tauri::command]
pub(crate) fn confirm_rule(
    app: AppHandle,
    state: State<AppState>,
    session_id: String,
    selected_paths: Vec<String>,
) -> Result<String, String> {
    let mut store = state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    let session = store
        .sessions
        .iter()
        .find(|item| item.session_id == session_id)
        .ok_or_else(|| "sessionId not found".to_string())?;

    let session_game_id = session.game_id.clone();
    let session_exe_path = session.exe_path.clone();
    let normalized_paths = normalize_paths(selected_paths, Some(&session_exe_path));
    if normalized_paths.is_empty() {
        return Err("selectedPaths cannot be empty".to_string());
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
        default_confidence()
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
pub(crate) fn get_learning_session(
    state: State<AppState>,
    session_id: String,
) -> Result<LearningSession, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    let session = store
        .sessions
        .iter()
        .find(|item| item.session_id == session_id)
        .ok_or_else(|| "sessionId not found".to_string())?;
    Ok(session.clone())
}

#[tauri::command]
pub(crate) fn open_candidate_path(path: String) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("path cannot be empty".to_string());
    }
    let target = Path::new(&path);
    if !target.exists() {
        return Err("candidate path does not exist".to_string());
    }
    if !target.is_dir() {
        return Err("candidate path is not a directory".to_string());
    }
    let mut command = Command::new("explorer");
    command.arg(target);
    apply_background_process_flags(&mut command)
        .spawn()
        .map_err(|err| format!("open directory failed: {err}"))?;
    Ok(())
}
