use crate::{
    app_state::AppState,
    domain::{AppTask, TaskSummary},
    services::TaskService,
};
use tauri::State;

#[tauri::command]
pub fn get_task(state: State<AppState>, task_id: String) -> Result<AppTask, String> {
    TaskService::get(&state, task_id.trim())
}

/// 任务列表。只返回 [`TaskSummary`]（不含 `result`）：列表界面不用它，而它占了整份
/// 负载的 99.5%（实测 2.32 MB）。完整任务用 [`get_task`]。
#[tauri::command]
pub fn list_tasks(state: State<AppState>) -> Result<Vec<TaskSummary>, String> {
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
