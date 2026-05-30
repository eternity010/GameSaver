use crate::{
    app_state::AppState,
    path_utils::normalize_paths,
    runtime::{file_sha256_hex, now_iso_string},
    shared::{GameLibraryItem, LauncherSession, PersistedStore, ResolveRuleResult},
    storage::{
        normalize_game_key, normalize_game_uid, rule_updated_ts,
        select_rule_for_game, JsonStoreRepository, StoreRepository,
    },
};
use std::path::Path;
use tauri::{AppHandle, State};

#[tauri::command]
pub(crate) fn resolve_rule_for_exe(
    state: State<AppState>,
    exe_path: String,
) -> Result<ResolveRuleResult, String> {
    let trimmed = exe_path.trim();
    if trimmed.is_empty() {
        return Err("exePath cannot be empty".to_string());
    }
    let exe = Path::new(trimmed);
    if !exe.exists() {
        return Err("exePath does not exist".to_string());
    }
    if !exe.is_file() {
        return Err("exePath is not a file".to_string());
    }

    let exe_hash = file_sha256_hex(exe)?;
    let store = state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    let matched_rule = store
        .rules
        .iter()
        .find(|rule| rule.enabled && rule.exe_hash.eq_ignore_ascii_case(&exe_hash))
        .cloned();
    Ok(ResolveRuleResult { exe_hash, matched_rule })
}

#[tauri::command]
pub(crate) fn get_launcher_session(
    state: State<AppState>,
    launcher_session_id: String,
) -> Result<LauncherSession, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    let session = store
        .launcher_sessions
        .iter()
        .find(|item| item.launcher_session_id == launcher_session_id)
        .ok_or_else(|| "launcherSessionId not found".to_string())?;
    Ok(session.clone())
}

#[tauri::command]
pub(crate) fn list_launcher_sessions(state: State<AppState>) -> Result<Vec<LauncherSession>, String> {
    let mut sessions = state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?
        .launcher_sessions
        .clone();
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(sessions)
}

#[tauri::command]
pub(crate) fn list_game_library_items(state: State<AppState>) -> Result<Vec<GameLibraryItem>, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    Ok(build_game_library_items(&store))
}

#[tauri::command]
pub(crate) fn set_preferred_exe_path(
    app: AppHandle,
    state: State<AppState>,
    game_id: String,
    exe_path: String,
) -> Result<GameLibraryItem, String> {
    let normalized_game_key = normalize_game_key(&game_id);
    if normalized_game_key.is_empty() {
        return Err("gameId cannot be empty".to_string());
    }
    let trimmed_exe_path = exe_path.trim().to_string();
    if trimmed_exe_path.is_empty() {
        return Err("exePath cannot be empty".to_string());
    }
    let exe = Path::new(&trimmed_exe_path);
    if !exe.exists() {
        return Err("exePath does not exist".to_string());
    }
    if !exe.is_file() {
        return Err("exePath is not a file".to_string());
    }
    if !trimmed_exe_path.to_ascii_lowercase().ends_with(".exe") {
        return Err("exePath must point to an .exe file".to_string());
    }

    let mut store = state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    let Some(rule) = select_rule_for_game(&store, &game_id) else {
        return Err("no available rule for this game".to_string());
    };
    let game_uid = normalize_game_uid(&rule.game_uid);
    if game_uid.is_empty() {
        return Err("selected rule is missing gameUid".to_string());
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
    JsonStoreRepository::new().normalize(&mut store);
    JsonStoreRepository::new().persist(&app, &store)?;

    build_game_library_items(&store)
        .into_iter()
        .find(|item| normalize_game_key(&item.game_id) == normalized_game_key)
        .ok_or_else(|| "game library item not found after update".to_string())
}

fn build_game_library_items(store: &PersistedStore) -> Vec<GameLibraryItem> {
    let mut grouped: std::collections::HashMap<String, Vec<_>> = std::collections::HashMap::new();
    for rule in &store.rules {
        let game_key = normalize_game_key(&rule.game_id);
        if game_key.is_empty() {
            continue;
        }
        grouped.entry(game_key).or_default().push(rule);
    }

    let mut items = grouped
        .into_iter()
        .map(|(game_key, rules)| {
            let total_rules = rules.len();
            let enabled_rules = rules.iter().filter(|rule| rule.enabled).count();
            let confirmed_path_count = rules.iter().map(|rule| rule.confirmed_paths.len()).sum();
            let latest_rule = rules
                .iter()
                .max_by_key(|rule| rule_updated_ts(rule))
                .copied();
            let preferred_exe_path = latest_rule.and_then(|rule| {
                let uid = normalize_game_uid(&rule.game_uid);
                if uid.is_empty() {
                    None
                } else {
                    store.execution_config.preferred_exe_by_uid.get(&uid).cloned()
                }
            });
            let last_session = store
                .launcher_sessions
                .iter()
                .filter(|session| {
                    session
                        .matched_game_id
                        .as_ref()
                        .is_some_and(|game_id| normalize_game_key(game_id) == game_key)
                })
                .max_by(|a, b| a.updated_at.cmp(&b.updated_at));

            GameLibraryItem {
                game_id: latest_rule
                    .map(|rule| rule.game_id.clone())
                    .unwrap_or_else(|| game_key.clone()),
                total_rules,
                enabled_rules,
                confirmed_path_count,
                last_rule_updated_at: latest_rule
                    .map(|rule| rule.updated_at.clone())
                    .unwrap_or_default(),
                preferred_exe_path,
                last_session_id: last_session.map(|session| session.launcher_session_id.clone()),
                last_session_status: last_session.map(|session| session.status.clone()),
                last_session_updated_at: last_session.map(|session| session.updated_at.clone()),
            }
        })
        .collect::<Vec<_>>();

    items.sort_by(|a, b| a.game_id.to_ascii_lowercase().cmp(&b.game_id.to_ascii_lowercase()));
    items
}
