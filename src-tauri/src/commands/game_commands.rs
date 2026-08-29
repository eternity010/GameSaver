use crate::{app_state::AppState, services::GameLibraryService};
use tauri::State;

#[tauri::command]
pub fn list_games(state: State<AppState>) -> Result<Vec<crate::domain::Game>, String> {
    let store = state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())?;
    Ok(GameLibraryService::list(&store))
}

#[tauri::command]
pub fn get_game(state: State<AppState>, game_uid: String) -> Result<Option<crate::domain::Game>, String> {
    let store = state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())?;
    Ok(GameLibraryService::find(&store, game_uid.trim()))
}
