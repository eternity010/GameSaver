use crate::{app_state::AppState, domain::AppTask, services::TaskService};
use tauri::State;

#[tauri::command]
pub fn get_task(state: State<AppState>, task_id: String) -> Result<AppTask, String> {
    TaskService::get(&state, task_id.trim())
}

#[tauri::command]
pub fn list_tasks(state: State<AppState>) -> Result<Vec<AppTask>, String> {
    TaskService::list(&state)
}

#[tauri::command]
pub fn cancel_task(state: State<AppState>, task_id: String) -> Result<(), String> {
    TaskService::cancel(&state, task_id.trim())
}

#[tauri::command]
pub fn delete_tasks(state: State<AppState>, task_ids: Vec<String>) -> Result<usize, String> {
    let task_ids = task_ids
        .into_iter()
        .map(|task_id| task_id.trim().to_string())
        .filter(|task_id| !task_id.is_empty())
        .collect::<Vec<_>>();
    TaskService::delete_many(&state, &task_ids)
}
