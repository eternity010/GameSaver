use crate::app_state::{AppState, BackgroundTask};
use tauri::State;

#[tauri::command]
pub(crate) fn get_task(state: State<AppState>, task_id: String) -> Result<BackgroundTask, String> {
    let tasks = state
        .tasks
        .lock()
        .map_err(|_| "failed to lock tasks".to_string())?;
    tasks
        .get(task_id.trim())
        .cloned()
        .ok_or_else(|| "taskId not found".to_string())
}
