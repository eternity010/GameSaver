use crate::{
    app_state::AppState,
    runtime::now_iso_string,
};
use tauri::{AppHandle, Manager, State};

pub(crate) fn update_background_task(
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
