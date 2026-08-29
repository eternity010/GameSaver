use crate::{app_state::AppState, domain::{AppTask, TaskStatus}};
use uuid::Uuid;

pub struct TaskService;

impl TaskService {
    pub fn create(state: &AppState, task_type: &str, game_uid: Option<String>, message: &str) -> Result<String, String> {
        let task_id = Uuid::new_v4().to_string();
        let task = AppTask {
            task_id: task_id.clone(),
            task_type: task_type.to_string(),
            status: TaskStatus::Pending,
            progress: 0,
            message: message.to_string(),
            game_uid,
            error: None,
            result: None,
            cancel_requested: false,
        };
        state.tasks.lock().map_err(|_| "lock task state failed".to_string())?.insert(task_id.clone(), task);
        Ok(task_id)
    }

    pub fn update(state: &AppState, task_id: &str, status: TaskStatus, progress: u8, message: impl Into<String>, error: Option<String>) {
        if let Ok(mut tasks) = state.tasks.lock() {
            if let Some(task) = tasks.get_mut(task_id) {
                task.status = status;
                task.progress = progress.min(100);
                task.message = message.into();
                task.error = error;
            }
        }
    }

    pub fn finish(
        state: &AppState,
        task_id: &str,
        status: TaskStatus,
        progress: u8,
        message: impl Into<String>,
        result: Option<serde_json::Value>,
        error: Option<String>,
    ) {
        if let Ok(mut tasks) = state.tasks.lock() {
            if let Some(task) = tasks.get_mut(task_id) {
                task.status = status;
                task.progress = progress.min(100);
                task.message = message.into();
                task.result = result;
                task.error = error;
            }
        }
    }

    pub fn is_cancelled(state: &AppState, task_id: &str) -> bool {
        state.tasks.lock().ok().and_then(|tasks| tasks.get(task_id).map(|task| task.cancel_requested)).unwrap_or(true)
    }

    pub fn cancel(state: &AppState, task_id: &str) -> Result<(), String> {
        let mut tasks = state.tasks.lock().map_err(|_| "lock task state failed".to_string())?;
        let task = tasks.get_mut(task_id).ok_or_else(|| "task not found".to_string())?;
        if matches!(task.status, TaskStatus::Pending | TaskStatus::Running) {
            task.cancel_requested = true;
        }
        Ok(())
    }

    pub fn get(state: &AppState, task_id: &str) -> Result<AppTask, String> {
        state.tasks.lock().map_err(|_| "lock task state failed".to_string())?.get(task_id).cloned().ok_or_else(|| "task not found".to_string())
    }

    pub fn list(state: &AppState) -> Result<Vec<AppTask>, String> {
        let mut tasks = state.tasks.lock().map_err(|_| "lock task state failed".to_string())?.values().cloned().collect::<Vec<_>>();
        tasks.sort_by(|left, right| left.task_id.cmp(&right.task_id));
        Ok(tasks)
    }
}
