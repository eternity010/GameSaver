use crate::{
    app_state::{AppState, BackgroundTask},
    path_utils::{expand_confirmed_path_for_runtime, normalize_confirmed_path_for_storage, normalize_windows_path},
    runtime::{file_sha256_hex, file_sha256_hex_with_context, now_iso_string, now_unix},
    shared::{
        BackupManifest, BackupManifestFileItem, BackupRunResult, BackupStatsResult, BackupVersion,
        ExecutionConfig, GameSaveRule, LauncherSession, PruneBackupResult, RestoreBackupResult,
    },
    storage::{
        has_unresolved_primary_rule_conflict_for_exe_hash, match_enabled_rule_for_exe_hash,
        normalize_game_key, normalize_game_uid, select_enabled_rule_for_game, JsonStoreRepository,
        normalize_backup_keep_versions, StoreRepository,
    },
    task_support::update_background_task,
};
use std::{collections::HashSet, fs, path::{Path, PathBuf}, process::Command};
use tauri::{AppHandle, Manager, State};
use walkdir::WalkDir;
use uuid::Uuid;

const MAX_BACKUP_FILE_BYTES: u64 = 100 * 1024 * 1024;

pub(crate) struct LaunchPreparation {
    pub(crate) session: LauncherSession,
    pub(crate) execution_config: ExecutionConfig,
    pub(crate) matched_rule: Option<GameSaveRule>,
    pub(crate) hash_matched_any: bool,
    pub(crate) unresolved_primary_conflict: bool,
    pub(crate) require_rule_match: bool,
    pub(crate) expected_game_key: Option<String>,
}

#[derive(Default)]
struct BackupSyncSummary {
    changed_files: usize,
    skipped_large_files: usize,
}

pub(crate) fn append_session_log(session: &mut LauncherSession, message: &str) {
    session.logs.push(format!("[{}] {}", now_iso_string(), message));
}

pub(crate) fn normalize_launch_mode(value: Option<&str>) -> String {
    match value.unwrap_or("backup").trim().to_ascii_lowercase().as_str() {
        "backup_direct" => "backup_direct".to_string(),
        _ => "backup".to_string(),
    }
}

pub(crate) fn new_launch_session(exe_path: String, launch_mode: &str) -> LauncherSession {
    let now = now_iso_string();
    LauncherSession {
        launcher_session_id: Uuid::new_v4().to_string(),
        exe_path: exe_path.trim().to_string(),
        exe_hash: String::new(),
        matched_rule_id: None,
        matched_game_id: None,
        matched_game_uid: None,
        launch_mode: launch_mode.to_string(),
        status: "idle".to_string(),
        pid: None,
        redirect_root: None,
        hook_version: None,
        started_at: now.clone(),
        updated_at: now,
        logs: vec![],
    }
}

pub(crate) fn resolve_preferred_exe_path_for_game(
    state: &State<AppState>,
    game_id: &str,
) -> Result<(String, String), String> {
    let normalized_game_key = normalize_game_key(game_id);
    if normalized_game_key.is_empty() {
        return Err("gameId cannot be empty".to_string());
    }

    let preferred_exe_path = {
        let store = state
            .store
            .lock()
            .map_err(|_| "failed to lock app state".to_string())?;
        let rule = select_enabled_rule_for_game(&store, game_id)
            .ok_or_else(|| format!("no enabled rule for game {}", game_id.trim()))?;
        let game_uid = normalize_game_uid(&rule.game_uid);
        if game_uid.is_empty() {
            return Err(format!("game {} rule is missing gameUid", game_id.trim()));
        }
        store
            .execution_config
            .preferred_exe_by_uid
            .get(&game_uid)
            .cloned()
            .ok_or_else(|| format!("game {} has no bound EXE yet", game_id.trim()))?
    };

    let trimmed = validate_bound_exe_path(&preferred_exe_path)?;
    Ok((normalized_game_key, trimmed))
}

pub(crate) fn validate_bound_exe_path(exe_path: &str) -> Result<String, String> {
    let trimmed = exe_path.trim().to_string();
    if trimmed.is_empty() {
        return Err("exePath cannot be empty".to_string());
    }
    let exe = Path::new(&trimmed);
    if !exe.exists() {
        return Err(format!("bound EXE does not exist: {trimmed}"));
    }
    if !exe.is_file() {
        return Err(format!("bound path is not a file: {trimmed}"));
    }
    if !trimmed.to_ascii_lowercase().ends_with(".exe") {
        return Err(format!("bound path is not an .exe file: {trimmed}"));
    }
    Ok(trimmed)
}

pub(crate) fn prepare_launch(
    state: &State<AppState>,
    exe_path: String,
    launch_mode: Option<&str>,
    expected_game_key: Option<String>,
    require_rule_match: bool,
) -> Result<LaunchPreparation, String> {
    let launch_mode_value = normalize_launch_mode(launch_mode);
    let mut session = new_launch_session(exe_path, &launch_mode_value);
    validate_session_exe(&mut session)?;

    let (execution_config, matched_rule, hash_matched_any, unresolved_primary_conflict) =
        resolve_launch_rule_context(state, &session.exe_hash, expected_game_key.as_deref())?;

    apply_rule_match_guards(
        &mut session,
        &matched_rule,
        hash_matched_any,
        unresolved_primary_conflict,
        expected_game_key.as_deref(),
        require_rule_match,
    )?;
    populate_session_match_metadata(&mut session, &execution_config, &matched_rule, &launch_mode_value);

    Ok(LaunchPreparation {
        session,
        execution_config,
        matched_rule,
        hash_matched_any,
        unresolved_primary_conflict,
        require_rule_match,
        expected_game_key,
    })
}

#[tauri::command]
pub(crate) fn launch_with_rule(
    app: AppHandle,
    state: State<AppState>,
    exe_path: String,
    launch_mode: Option<String>,
) -> Result<LauncherSession, String> {
    let prepared = prepare_launch(&state, exe_path, launch_mode.as_deref(), None, false)?;
    finalize_prepared_launch(&app, &state, prepared)
}

#[tauri::command]
pub(crate) fn launch_game_from_library(
    app: AppHandle,
    state: State<AppState>,
    game_id: String,
    launch_mode: Option<String>,
) -> Result<LauncherSession, String> {
    let (normalized_game_key, exe_path) = resolve_preferred_exe_path_for_game(&state, &game_id)?;
    let prepared = prepare_launch(
        &state,
        exe_path,
        launch_mode.as_deref(),
        Some(normalized_game_key),
        true,
    )?;
    finalize_prepared_launch(&app, &state, prepared)
}

fn validate_session_exe(session: &mut LauncherSession) -> Result<(), String> {
    if session.exe_path.is_empty() {
        mark_session_failed(session, "Launch failed: exePath is empty");
        return Err("exePath cannot be empty".to_string());
    }

    let exe = Path::new(&session.exe_path);
    if !exe.exists() {
        mark_session_failed(session, "Launch failed: target exe does not exist");
        return Err("exePath does not exist".to_string());
    }
    if !exe.is_file() {
        mark_session_failed(session, "Launch failed: target exe is not a file");
        return Err("exePath is not a file".to_string());
    }

    match file_sha256_hex(exe) {
        Ok(hash) => {
            session.exe_hash = hash;
            Ok(())
        }
        Err(err) => {
            mark_session_failed(session, "Launch failed: could not hash exe");
            Err(format!("failed to hash exe: {err}"))
        }
    }
}

fn resolve_launch_rule_context(
    state: &State<AppState>,
    exe_hash: &str,
    expected_game_key: Option<&str>,
) -> Result<(ExecutionConfig, Option<GameSaveRule>, bool, bool), String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    let execution_config = store.execution_config.clone();
    let (matched_rule, hash_matched_any) = match_enabled_rule_for_exe_hash(
        &store.rules,
        &store.execution_config,
        exe_hash,
        expected_game_key,
    );
    let unresolved_primary_conflict = has_unresolved_primary_rule_conflict_for_exe_hash(
        &store.rules,
        &store.execution_config,
        exe_hash,
        expected_game_key,
    );
    Ok((
        execution_config,
        matched_rule,
        hash_matched_any,
        unresolved_primary_conflict,
    ))
}

fn finalize_prepared_launch(
    app: &AppHandle,
    state: &State<AppState>,
    prepared: LaunchPreparation,
) -> Result<LauncherSession, String> {
    let LaunchPreparation {
        mut session,
        execution_config,
        matched_rule,
        hash_matched_any: _hash_matched_any,
        unresolved_primary_conflict: _unresolved_primary_conflict,
        require_rule_match: _require_rule_match,
        expected_game_key: _expected_game_key,
    } = prepared;

    execute_basic_launch(app, &mut session, &execution_config, matched_rule.as_ref())?;
    persist_launcher_session(app, state, &session)?;
    Ok(session)
}

fn apply_rule_match_guards(
    session: &mut LauncherSession,
    matched_rule: &Option<GameSaveRule>,
    hash_matched_any: bool,
    unresolved_primary_conflict: bool,
    expected_game_key: Option<&str>,
    require_rule_match: bool,
) -> Result<(), String> {
    if !require_rule_match {
        return Ok(());
    }

    if unresolved_primary_conflict {
        let message =
            "Launch blocked: multiple enabled rules match this EXE and no primary rule is set.".to_string();
        mark_session_failed(session, &message);
        return Err(message);
    }

    if matched_rule.is_none() {
        let message = if let Some(game_key) = expected_game_key {
            if hash_matched_any {
                format!("Launch blocked: bound EXE does not match game {game_key}")
            } else {
                format!("Launch blocked: no enabled rule matched game {game_key}")
            }
        } else {
            "Launch blocked: no enabled rule matched this EXE".to_string()
        };
        mark_session_failed(session, &message);
        return Err(message);
    }

    Ok(())
}

fn populate_session_match_metadata(
    session: &mut LauncherSession,
    execution_config: &ExecutionConfig,
    matched_rule: &Option<GameSaveRule>,
    launch_mode_value: &str,
) {
    if let Some(rule) = matched_rule {
        session.matched_rule_id = Some(rule.rule_id.clone());
        session.matched_game_id = Some(rule.game_id.clone());
        session.matched_game_uid = Some(rule.game_uid.clone());
        session.hook_version = Some(if launch_mode_value == "backup" {
            "backup-auto-v1".to_string()
        } else {
            "backup-direct-v1".to_string()
        });
        session.redirect_root = Some(
            Path::new(&execution_config.backup_root)
                .join(rule.game_uid.trim())
                .to_string_lossy()
                .to_string(),
        );
        append_session_log(
            session,
            &format!("Matched rule: {} ({}) mode={launch_mode_value}", rule.game_id, rule.rule_id),
        );
    } else {
        append_session_log(session, "No enabled rule matched. Will launch in plain mode.");
    }
}

fn mark_session_failed(session: &mut LauncherSession, log_message: &str) {
    session.status = "failed".to_string();
    append_session_log(session, log_message);
    session.updated_at = now_iso_string();
}

fn execute_basic_launch(
    app: &AppHandle,
    session: &mut LauncherSession,
    execution_config: &ExecutionConfig,
    matched_rule: Option<&GameSaveRule>,
) -> Result<(), String> {
    let is_backup_mode = session.launch_mode == "backup" || session.launch_mode == "backup_direct";
    let restore_before_launch = session.launch_mode == "backup";
    if restore_before_launch {
        if let Some(rule) = matched_rule {
            match restore_latest_backup_for_rule(rule, &execution_config.backup_root, Some(&session.exe_path)) {
                Ok(copied) => append_session_log(
                    session,
                    &format!(
                        "Automatic pre-launch restore completed: {copied} files, backup root {}",
                        execution_config.backup_root
                    ),
                ),
                Err(err) => {
                    let message = format!("Automatic pre-launch restore failed: {err}");
                    mark_session_failed(session, &message);
                    return Err(message);
                }
            }
        }
    } else if session.launch_mode == "backup_direct" && matched_rule.is_some() {
        append_session_log(
            session,
            "Skipped pre-launch restore by sync policy, will launch directly and auto-back up after exit.",
        );
    }

    session.status = "launching".to_string();
    append_session_log(session, "Starting target process");
    let mut command = Command::new(&session.exe_path);
    if let Some(exe_dir) = Path::new(&session.exe_path).parent() {
        command.current_dir(exe_dir);
    }
    let mut child = command.spawn().map_err(|err| {
        let message = format!("Failed to launch process: {err}");
        mark_session_failed(session, &message);
        message
    })?;

    session.pid = Some(child.id());
    if is_backup_mode {
        if let Some(rule) = matched_rule.cloned() {
            let keep_versions = resolve_backup_keep_versions(execution_config, &rule.game_uid);
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
                session,
                &format!("Enabled incremental auto-backup after exit, keeping latest {keep_versions} versions"),
            );
        } else {
            let _ = child.kill();
            let message = "Backup mode requires a matched rule.".to_string();
            mark_session_failed(session, &message);
            return Err(message);
        }
    }
    session.status = "running".to_string();
    session.updated_at = now_iso_string();
    append_session_log(session, "Launch completed");
    Ok(())
}

fn resolve_backup_keep_versions(execution_config: &ExecutionConfig, game_uid: &str) -> usize {
    let uid = normalize_game_uid(game_uid);
    if uid.is_empty() {
        return 10;
    }
    execution_config
        .backup_keep_versions_by_uid
        .get(&uid)
        .copied()
        .filter(|keep| *keep > 0)
        .map(normalize_backup_keep_versions)
        .unwrap_or(10)
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
    let mut copied = 0usize;
    for source in &rule.confirmed_paths {
        let runtime_path = expand_confirmed_path_for_runtime(source, exe_path)?;
        let target_path = Path::new(&runtime_path);
        let slot = backup_slot_name(source);
        let mut slot_source = latest.join(&slot);
        if !slot_source.exists() {
            slot_source = latest.join(backup_slot_name_legacy(source));
        }
        copied += sync_directory(&slot_source, target_path)?;
    }
    Ok(copied)
}

fn ensure_backup_root_for_rule(rule: &GameSaveRule, backup_root: &str) -> Result<PathBuf, String> {
    let game_uid = normalize_game_uid(&rule.game_uid);
    if game_uid.is_empty() {
        return Err(format!("rule {} is missing gameUid", rule.rule_id));
    }
    let uid_root = backup_game_root(backup_root, &game_uid);
    let legacy_root = Path::new(backup_root).join(rule.game_id.trim());
    if !uid_root.exists() && legacy_root.exists() {
        let _ = sync_directory(&legacy_root, &uid_root);
    }
    Ok(uid_root)
}

fn backup_game_root(backup_root: &str, game_uid: &str) -> PathBuf {
    Path::new(backup_root).join(game_uid.trim())
}

fn backup_slot_name(source_path: &str) -> String {
    let normalized = normalize_windows_path(&normalize_confirmed_path_for_storage(source_path, None));
    let mut hasher = sha2::Sha256::new();
    use sha2::Digest;
    hasher.update(normalized.as_bytes());
    let hash = hex::encode(hasher.finalize());
    format!("slot_{}", &hash[..12])
}

fn backup_slot_name_legacy(source_path: &str) -> String {
    let normalized = normalize_windows_path(source_path);
    let mut hasher = sha2::Sha256::new();
    use sha2::Digest;
    hasher.update(normalized.as_bytes());
    let hash = hex::encode(hasher.finalize());
    format!("slot_{}", &hash[..12])
}

fn sync_directory(source: &Path, target: &Path) -> Result<usize, String> {
    if !source.exists() {
        return Ok(0);
    }
    fs::create_dir_all(target).map_err(|err| format!("create directory failed: {err}"))?;
    let mut copied = 0usize;
    for entry in WalkDir::new(source).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = match entry.path().strip_prefix(source) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let dest = target.join(relative);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|err| format!("create directory failed: {err}"))?;
        }
        fs::copy(entry.path(), &dest).map_err(|err| format!("copy file failed: {err}"))?;
        copied += 1;
    }
    Ok(copied)
}

fn is_backup_file_too_large(path: &Path) -> bool {
    path.metadata()
        .map(|metadata| metadata.len() > MAX_BACKUP_FILE_BYTES)
        .unwrap_or(false)
}

fn sync_backup_directory(source: &Path, target: &Path) -> Result<usize, String> {
    if !source.exists() {
        return Ok(0);
    }
    fs::create_dir_all(target).map_err(|err| format!("create directory failed: {err}"))?;
    let mut copied = 0usize;
    for entry in WalkDir::new(source).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() || is_backup_file_too_large(entry.path()) {
            continue;
        }
        let relative = match entry.path().strip_prefix(source) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let dest = target.join(relative);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|err| format!("create directory failed: {err}"))?;
        }
        fs::copy(entry.path(), &dest).map_err(|err| format!("copy file failed: {err}"))?;
        copied += 1;
    }
    Ok(copied)
}

fn file_signature(path: &Path) -> Option<(u64, u64)> {
    let metadata = path.metadata().ok()?;
    let size = metadata.len();
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| crate::runtime::system_time_to_unix(value))
        .unwrap_or(0);
    Some((size, modified))
}

fn sync_source_to_backup_slot(source: &Path, latest_slot: &Path, version_slot: &Path) -> Result<BackupSyncSummary, String> {
    if !source.exists() {
        if latest_slot.exists() {
            fs::remove_dir_all(latest_slot).map_err(|err| format!("clean old latest failed: {err}"))?;
        }
        return Ok(BackupSyncSummary::default());
    }
    fs::create_dir_all(latest_slot).map_err(|err| format!("create directory failed: {err}"))?;

    let mut summary = BackupSyncSummary::default();
    let mut source_files = HashSet::new();
    for entry in WalkDir::new(source).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = match entry.path().strip_prefix(source) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if is_backup_file_too_large(entry.path()) {
            summary.skipped_large_files += 1;
            continue;
        }
        let key = normalize_windows_path(&relative.to_string_lossy());
        source_files.insert(key);
        let latest_file = latest_slot.join(relative);
        if file_signature(entry.path()) != file_signature(&latest_file) {
            if let Some(parent) = latest_file.parent() {
                fs::create_dir_all(parent).map_err(|err| format!("create directory failed: {err}"))?;
            }
            fs::copy(entry.path(), &latest_file).map_err(|err| format!("copy file failed: {err}"))?;
            let version_file = version_slot.join(relative);
            if let Some(parent) = version_file.parent() {
                fs::create_dir_all(parent).map_err(|err| format!("create directory failed: {err}"))?;
            }
            fs::copy(entry.path(), &version_file).map_err(|err| format!("copy file failed: {err}"))?;
            summary.changed_files += 1;
        }
    }

    let mut stale_files = Vec::new();
    for entry in WalkDir::new(latest_slot).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = match entry.path().strip_prefix(latest_slot) {
            Ok(value) => value,
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
        summary.changed_files += stale_count;
    }

    if summary.changed_files > 0 {
        if version_slot.exists() {
            fs::remove_dir_all(version_slot).map_err(|err| format!("cleanup version snapshot failed: {err}"))?;
        }
        let _ = sync_backup_directory(source, version_slot)?;
    }

    Ok(summary)
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
            .map_err(|err| format!("compute backup relative path failed: {err}"))?;
        let relative_path = normalize_windows_path(&relative.to_string_lossy());
        if relative_path.is_empty() {
            continue;
        }
        let size = entry
            .metadata()
            .map_err(|err| format!("read backup file metadata failed: {err}"))?
            .len();
        let sha256 = file_sha256_hex_with_context(entry.path(), "backup file")?;
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

fn write_pretty_json_file<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|err| format!("serialize json failed: {err}"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("create directory failed: {err}"))?;
    }
    fs::write(path, text).map_err(|err| format!("write json failed: {err}"))
}

fn cleanup_backup_versions(versions_dir: &Path, keep: usize) -> Result<(), String> {
    if !versions_dir.exists() {
        return Ok(());
    }
    let mut dirs = fs::read_dir(versions_dir)
        .map_err(|err| format!("read versions directory failed: {err}"))?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .collect::<Vec<_>>();
    dirs.sort_by_key(|entry| entry.file_name());
    if dirs.len() <= keep {
        return Ok(());
    }
    let prune_count = dirs.len() - keep;
    for entry in dirs.into_iter().take(prune_count) {
        fs::remove_dir_all(entry.path()).map_err(|err| format!("cleanup old version failed: {err}"))?;
    }
    Ok(())
}

fn backup_current_state_for_rule(
    rule: &GameSaveRule,
    backup_root: &str,
    keep_versions: usize,
    exe_path: Option<&str>,
) -> Result<BackupRunResult, String> {
    let base = ensure_backup_root_for_rule(rule, backup_root)?;
    let latest = base.join("latest");
    let versions = base.join("versions");
    fs::create_dir_all(&latest).map_err(|err| format!("create directory failed: {err}"))?;
    fs::create_dir_all(&versions).map_err(|err| format!("create directory failed: {err}"))?;
    let snapshot_id = now_unix().to_string();
    let version_root = versions.join(&snapshot_id);

    let mut summary = BackupSyncSummary::default();
    for source in &rule.confirmed_paths {
        let runtime_path = expand_confirmed_path_for_runtime(source, exe_path)?;
        let runtime_source = Path::new(&runtime_path);
        let slot = backup_slot_name(source);
        let source_summary = sync_source_to_backup_slot(runtime_source, &latest.join(&slot), &version_root.join(&slot))?;
        summary.changed_files += source_summary.changed_files;
        summary.skipped_large_files += source_summary.skipped_large_files;
    }
    if summary.changed_files == 0 && version_root.exists() {
        let _ = fs::remove_dir_all(&version_root);
    } else if summary.changed_files > 0 {
        let manifest = build_backup_manifest(&version_root, &snapshot_id, &rule.game_uid)?;
        write_pretty_json_file(&version_root.join("manifest.json"), &manifest)?;
    }
    cleanup_backup_versions(&versions, keep_versions)?;
    Ok(BackupRunResult {
        changed_files: summary.changed_files,
        skipped_large_files: summary.skipped_large_files,
        version_id: if summary.changed_files > 0 { Some(snapshot_id) } else { None },
    })
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
        let backup_result = backup_current_state_for_rule(&rule, &backup_root, keep_versions, Some(&exe_path));
        let state: State<AppState> = app.state();
        let lock_result = state.store.lock();
        if let Ok(mut store) = lock_result {
            if let Some(session) = store
                .launcher_sessions
                .iter_mut()
                .find(|item| item.launcher_session_id == session_id)
            {
                match backup_result {
                    Ok(result) => {
                        if let Some(snapshot) = result.version_id {
                            let skipped_note = if result.skipped_large_files > 0 {
                                format!(", skipped {} files over 100 MB", result.skipped_large_files)
                            } else {
                                String::new()
                            };
                            append_session_log(
                                session,
                                &format!(
                                    "Automatic backup completed: detected {} changed files{}, created version {}, keep {}",
                                    result.changed_files, skipped_note, snapshot, keep_versions
                                ),
                            );
                        } else {
                            let skipped_note = if result.skipped_large_files > 0 {
                                format!(" (skipped {} files over 100 MB)", result.skipped_large_files)
                            } else {
                                String::new()
                            };
                            append_session_log(
                                session,
                                &format!("No save changes detected, skipped creating a new backup version{skipped_note}"),
                            );
                        }
                    }
                    Err(err) => append_session_log(session, &format!("Automatic backup failed: {err}")),
                }
                session.status = "exited".to_string();
                session.updated_at = now_iso_string();
            }
            let _ = JsonStoreRepository::new().persist(&app, &store);
        };
    });
}

fn backup_version_label(version_id: &str) -> String {
    if version_id.starts_with("pre_restore_") {
        "Pre-restore backup".to_string()
    } else {
        "Backup snapshot".to_string()
    }
}

fn list_version_directories(versions_dir: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    if !versions_dir.exists() {
        return Ok(vec![]);
    }
    let mut output = fs::read_dir(versions_dir)
        .map_err(|err| format!("read versions directory failed: {err}"))?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|entry| {
            let version_id = entry.file_name().to_string_lossy().to_string();
            (version_id, entry.path())
        })
        .collect::<Vec<_>>();
    output.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(output)
}

fn directory_total_bytes(root: &Path) -> u64 {
    if !root.exists() {
        return 0;
    }
    WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| entry.metadata().ok().map(|meta| meta.len()))
        .sum()
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

#[tauri::command]
pub(crate) fn list_backup_versions(
    state: State<AppState>,
    game_id: String,
) -> Result<Vec<BackupVersion>, String> {
    let trimmed_game_id = game_id.trim();
    if trimmed_game_id.is_empty() {
        return Err("gameId cannot be empty".to_string());
    }
    let store = state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    let rule = select_enabled_rule_for_game(&store, trimmed_game_id)
        .ok_or_else(|| "no enabled rule for this game".to_string())?;
    let base = ensure_backup_root_for_rule(&rule, &store.execution_config.backup_root)?;
    let versions_dir = base.join("versions");
    let mut versions = list_version_directories(&versions_dir)?
        .into_iter()
        .map(|(version_id, path)| BackupVersion {
            created_at: version_id.clone(),
            label: backup_version_label(&version_id),
            restorable: !version_id.starts_with('_'),
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
pub(crate) fn get_backup_stats(state: State<AppState>, game_id: String) -> Result<BackupStatsResult, String> {
    let trimmed_game_id = game_id.trim();
    if trimmed_game_id.is_empty() {
        return Err("gameId cannot be empty".to_string());
    }
    let (rule, backup_root, keep_versions) = {
        let store = state
            .store
            .lock()
            .map_err(|_| "failed to lock app state".to_string())?;
        let rule = select_enabled_rule_for_game(&store, trimmed_game_id)
            .ok_or_else(|| "no enabled rule for this game".to_string())?;
        let keep_versions = resolve_backup_keep_versions(&store.execution_config, &rule.game_uid);
        (rule, store.execution_config.backup_root.clone(), keep_versions)
    };
    build_backup_stats_for_rule(&rule, &backup_root, keep_versions)
}

#[tauri::command]
pub(crate) fn set_backup_keep_versions(
    app: AppHandle,
    state: State<AppState>,
    game_id: String,
    keep_versions: usize,
) -> Result<BackupStatsResult, String> {
    let trimmed_game_id = game_id.trim();
    if trimmed_game_id.is_empty() {
        return Err("gameId cannot be empty".to_string());
    }
    if keep_versions == 0 {
        return Err("keepVersions must be >= 1".to_string());
    }

    let normalized_keep = normalize_backup_keep_versions(keep_versions);
    let (rule, backup_root) = {
        let mut store = state
            .store
            .lock()
            .map_err(|_| "failed to lock app state".to_string())?;
        let rule = select_enabled_rule_for_game(&store, trimmed_game_id)
            .ok_or_else(|| "no enabled rule for this game".to_string())?;
        let game_uid = normalize_game_uid(&rule.game_uid);
        if game_uid.is_empty() {
            return Err("selected rule is missing gameUid".to_string());
        }
        store
            .execution_config
            .backup_keep_versions_by_uid
            .insert(game_uid, normalized_keep);
        let backup_root = store.execution_config.backup_root.clone();
        JsonStoreRepository::new().persist(&app, &store)?;
        (rule, backup_root)
    };
    build_backup_stats_for_rule(&rule, &backup_root, normalized_keep)
}

#[tauri::command]
pub(crate) fn prune_backup_versions(
    app: AppHandle,
    state: State<AppState>,
    game_id: String,
    keep_versions: Option<usize>,
) -> Result<PruneBackupResult, String> {
    let trimmed_game_id = game_id.trim();
    if trimmed_game_id.is_empty() {
        return Err("gameId cannot be empty".to_string());
    }
    if keep_versions.is_some_and(|value| value == 0) {
        return Err("keepVersions must be >= 1".to_string());
    }

    let (rule, backup_root, keep) = {
        let mut store = state
            .store
            .lock()
            .map_err(|_| "failed to lock app state".to_string())?;
        let rule = select_enabled_rule_for_game(&store, trimmed_game_id)
            .ok_or_else(|| "no enabled rule for this game".to_string())?;
        let game_uid = normalize_game_uid(&rule.game_uid);
        if game_uid.is_empty() {
            return Err("selected rule is missing gameUid".to_string());
        }
        let keep = if let Some(value) = keep_versions {
            let normalized = normalize_backup_keep_versions(value);
            store
                .execution_config
                .backup_keep_versions_by_uid
                .insert(game_uid, normalized);
            JsonStoreRepository::new().persist(&app, &store)?;
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
    let mut freed_bytes = 0u64;
    for (_, path) in version_dirs.into_iter().take(deleted_versions) {
        freed_bytes += directory_total_bytes(&path);
        fs::remove_dir_all(&path).map_err(|err| format!("cleanup old version failed: {err}"))?;
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

struct RestorePlanItem {
    source_path: String,
    slot_name: String,
    stage_slot: PathBuf,
    rollback_slot: PathBuf,
    target_exists: bool,
}

fn cleanup_restore_temp_dirs(staging_root: &Path, rollback_root: &Path) {
    if staging_root.exists() {
        let _ = fs::remove_dir_all(staging_root);
    }
    if rollback_root.exists() {
        let _ = fs::remove_dir_all(rollback_root);
    }
}

fn remove_path_if_exists(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    if path.is_dir() {
        fs::remove_dir_all(path).map_err(|err| format!("cleanup target directory failed: {err}"))
    } else {
        fs::remove_file(path).map_err(|err| format!("cleanup target file failed: {err}"))
    }
}

fn rollback_applied_restore_plans(plans: &[RestorePlanItem], applied_indices: &[usize]) -> Result<(), String> {
    let mut rollback_errors = Vec::new();
    for idx in applied_indices.iter().rev() {
        let plan = &plans[*idx];
        let target = Path::new(&plan.source_path);
        if let Err(err) = remove_path_if_exists(target) {
            rollback_errors.push(format!("slot {} cleanup failed: {err}", plan.slot_name));
            continue;
        }
        if plan.target_exists {
            if let Err(err) = sync_directory(&plan.rollback_slot, target) {
                rollback_errors.push(format!("slot {} rollback failed: {err}", plan.slot_name));
            }
        }
    }
    if rollback_errors.is_empty() {
        Ok(())
    } else {
        Err(rollback_errors.join("; "))
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
    fs::create_dir_all(&staging_root).map_err(|err| format!("create restore temp dir failed: {err}"))?;
    fs::create_dir_all(&rollback_root).map_err(|err| format!("create restore rollback dir failed: {err}"))?;

    let mut plans = Vec::new();
    for source_path in &rule.confirmed_paths {
        let slot = backup_slot_name(source_path);
        let mut slot_source = version_dir.join(&slot);
        if !slot_source.exists() {
            slot_source = version_dir.join(backup_slot_name_legacy(source_path));
        }
        if !slot_source.exists() {
            cleanup_restore_temp_dirs(&staging_root, &rollback_root);
            return Err(format!("backup version is incomplete, missing slot {slot}"));
        }

        let stage_slot = staging_root.join(&slot);
        sync_directory(&slot_source, &stage_slot).map_err(|err| {
            cleanup_restore_temp_dirs(&staging_root, &rollback_root);
            format!("prepare restore slot {slot} failed: {err}")
        })?;

        let runtime_path = expand_confirmed_path_for_runtime(source_path, exe_path)?;
        let target = Path::new(&runtime_path);
        let rollback_slot = rollback_root.join(&slot);
        let target_exists = target.exists();
        if target_exists {
            sync_directory(target, &rollback_slot).map_err(|err| {
                cleanup_restore_temp_dirs(&staging_root, &rollback_root);
                format!("create rollback snapshot failed for slot {slot}: {err}")
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

    let mut restored_files = 0usize;
    let mut applied_indices = Vec::new();
    for (idx, plan) in plans.iter().enumerate() {
        let target = Path::new(&plan.source_path);
        if let Err(err) = remove_path_if_exists(target) {
            let rollback_result = rollback_applied_restore_plans(&plans, &applied_indices);
            cleanup_restore_temp_dirs(&staging_root, &rollback_root);
            return Err(match rollback_result {
                Ok(_) => format!("restore failed at slot {} and rollback succeeded: {err}", plan.slot_name),
                Err(rollback_err) => format!(
                    "restore failed at slot {} and rollback failed: {err}; rollback error: {rollback_err}",
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
                    Ok(_) => format!("restore failed at slot {} and rollback succeeded: {err}", plan.slot_name),
                    Err(rollback_err) => format!(
                        "restore failed at slot {} and rollback failed: {err}; rollback error: {rollback_err}",
                        plan.slot_name
                    ),
                });
            }
        }
    }

    cleanup_restore_temp_dirs(&staging_root, &rollback_root);
    Ok(restored_files)
}

fn restore_backup_version_impl(
    app: &AppHandle,
    app_state: &AppState,
    game_id: &str,
    version_id: &str,
    mut on_progress: impl FnMut(u8, String),
) -> Result<RestoreBackupResult, String> {
    let trimmed_game_id = game_id.trim().to_string();
    let trimmed_version_id = version_id.trim().to_string();
    if trimmed_game_id.is_empty() {
        return Err("gameId cannot be empty".to_string());
    }
    if trimmed_version_id.is_empty() {
        return Err("versionId cannot be empty".to_string());
    }

    on_progress(10, "Checking backup metadata".to_string());
    let (backup_root, rule, keep_versions, restore_exe_path) = {
        let store = app_state
            .store
            .lock()
            .map_err(|_| "failed to lock app state".to_string())?;
        let rule = select_enabled_rule_for_game(&store, &trimmed_game_id)
            .ok_or_else(|| "no enabled rule for this game".to_string())?;
        let keep_versions = resolve_backup_keep_versions(&store.execution_config, &rule.game_uid);
        let normalized_uid = normalize_game_uid(&rule.game_uid);
        let restore_exe_path = if normalized_uid.is_empty() {
            None
        } else {
            store.execution_config.preferred_exe_by_uid.get(&normalized_uid).cloned()
        };
        (
            store.execution_config.backup_root.clone(),
            rule,
            keep_versions,
            restore_exe_path,
        )
    };

    let base = ensure_backup_root_for_rule(&rule, &backup_root)?;
    let version_dir = base.join("versions").join(&trimmed_version_id);
    if !version_dir.exists() {
        return Err("target backup version does not exist".to_string());
    }

    on_progress(25, "Creating pre-restore backup snapshot".to_string());
    let pre_restore_version_id =
        match backup_current_state_for_rule(&rule, &backup_root, keep_versions + 1, restore_exe_path.as_deref()) {
            Ok(result) if result.changed_files > 0 => result.version_id,
            Ok(_) => None,
            Err(err) => return Err(format!("failed to create pre-restore backup: {err}")),
        };

    on_progress(55, "Restoring selected backup version".to_string());
    let restored_files =
        restore_backup_version_transactional(&rule, &version_dir, &base, restore_exe_path.as_deref())?;

    on_progress(85, "Updating launcher session logs".to_string());
    {
        let mut store = app_state
            .store
            .lock()
            .map_err(|_| "failed to lock app state".to_string())?;
        if let Some(session) = store
            .launcher_sessions
            .iter_mut()
            .filter(|item| {
                item.matched_game_uid
                    .as_ref()
                    .is_some_and(|uid| normalize_game_uid(uid) == normalize_game_uid(&rule.game_uid))
                    || item
                        .matched_game_id
                        .as_ref()
                        .is_some_and(|id| id.eq_ignore_ascii_case(&trimmed_game_id))
            })
            .max_by_key(|item| item.updated_at.clone())
        {
            append_session_log(
                session,
                &format!(
                    "Backup restore completed: game={}, version={}, restored_files={}",
                    trimmed_game_id, trimmed_version_id, restored_files
                ),
            );
            session.updated_at = now_iso_string();
        }
        JsonStoreRepository::new().persist(app, &store)?;
    }

    on_progress(100, "Restore completed".to_string());
    Ok(RestoreBackupResult {
        game_id: trimmed_game_id,
        version_id: trimmed_version_id,
        restored_files,
        pre_restore_version_id,
        verified_files: restored_files,
        hash_sample_count: 0,
    })
}


#[tauri::command]
pub(crate) fn restore_backup_version(
    app: AppHandle,
    state: State<AppState>,
    game_id: String,
    version_id: String,
) -> Result<RestoreBackupResult, String> {
    restore_backup_version_impl(&app, state.inner(), &game_id, &version_id, |_progress, _message| {})
}

#[tauri::command]
pub(crate) fn start_restore_backup_version_task(
    app: AppHandle,
    state: State<AppState>,
    game_id: String,
    version_id: String,
) -> Result<String, String> {
    if game_id.trim().is_empty() || version_id.trim().is_empty() {
        return Err("gameId/versionId cannot be empty".to_string());
    }
    let task_id = Uuid::new_v4().to_string();
    let now = now_iso_string();
    let task = BackgroundTask {
        task_id: task_id.clone(),
        task_type: "restore_backup_version".to_string(),
        status: "pending".to_string(),
        progress: Some(0),
        message: Some("task created".to_string()),
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
    let game_id_for_thread = game_id.trim().to_string();
    let version_id_for_thread = version_id.trim().to_string();
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        update_background_task(
            &app_handle,
            &task_id_for_thread,
            "running",
            Some(5),
            Some("starting restore".to_string()),
            None,
            None,
        );
        let app_state: State<AppState> = app_handle.state();
        match restore_backup_version_impl(
            &app_handle,
            app_state.inner(),
            &game_id_for_thread,
            &version_id_for_thread,
            |progress, message| {
                update_background_task(
                    &app_handle,
                    &task_id_for_thread,
                    "running",
                    Some(progress),
                    Some(message),
                    None,
                    None,
                );
            },
        ) {
            Ok(summary) => {
                update_background_task(
                    &app_handle,
                    &task_id_for_thread,
                    "success",
                    Some(100),
                    Some("restore completed".to_string()),
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
                    Some("restore failed".to_string()),
                    None,
                    Some(err),
                );
            }
        }
    });
    Ok(task_id)
}

fn persist_launcher_session(
    app: &AppHandle,
    state: &State<AppState>,
    session: &LauncherSession,
) -> Result<(), String> {
    let mut store = state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    store.launcher_sessions.push(session.clone());
    JsonStoreRepository::new().persist(app, &store)
}
