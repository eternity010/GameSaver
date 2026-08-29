use crate::{
    app_state::AppState,
    domain::{GameRuntime, SaveVersion},
    services::{LaunchPrecheck, LaunchService},
};
use tauri::{AppHandle, State};

#[tauri::command]
pub fn precheck_game_launch(state: State<AppState>, game_uid: String) -> Result<LaunchPrecheck, String> {
    let store = state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())?;
    LaunchService::precheck(&store, game_uid.trim())
}

#[tauri::command]
pub fn launch_game(app: AppHandle, state: State<AppState>, game_uid: String) -> Result<String, String> {
    LaunchService::launch(&app, &state, game_uid)
}

#[tauri::command]
pub fn get_game_runtime(state: State<AppState>, game_uid: String) -> Result<Option<GameRuntime>, String> {
    Ok(state.running_games.lock().map_err(|_| "lock running game state failed".to_string())?.get(game_uid.trim()).cloned())
}

#[tauri::command]
pub fn list_save_versions(state: State<AppState>, game_uid: String) -> Result<Vec<SaveVersion>, String> {
    let game_uid = game_uid.trim();
    let store = state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())?;
    if !store.games.iter().any(|game| game.game_uid == game_uid) {
        return Err("游戏不存在".to_string());
    }
    let mut versions = store.save_versions.iter().filter(|version| version.game_uid == game_uid).cloned().collect::<Vec<_>>();
    versions.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    Ok(versions)
}
