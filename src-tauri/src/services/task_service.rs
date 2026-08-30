use crate::{app_state::AppState, domain::{AppTask, TaskRetry, TaskStatus}, repositories::TaskRepository};
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
            retry: None,
            created_at: now_millis(),
            cancel_requested: false,
        };
        let mut tasks = state.tasks.lock().map_err(|_| "lock task state failed".to_string())?;
        tasks.insert(task_id.clone(), task);
        if let Err(error) = TaskRepository::persist(&state.tasks_path, &tasks) {
            tasks.remove(&task_id);
            return Err(error);
        }
        Ok(task_id)
    }

    pub fn set_retry(state: &AppState, task_id: &str, retry: TaskRetry) -> Result<(), String> {
        let mut tasks = state.tasks.lock().map_err(|_| "lock task state failed".to_string())?;
        let task = tasks.get_mut(task_id).ok_or_else(|| "task not found".to_string())?;
        task.retry = Some(retry);
        persist_locked(state, &tasks);
        Ok(())
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
        let message = message.into();
        if matches!(&status, TaskStatus::Failed) {
            crate::logging::error(format!(
                "后台任务失败：task_id={task_id} message={} error={}",
                message,
                error.clone().unwrap_or_default()
            ));
        }
        if let Ok(mut tasks) = state.tasks.lock() {
            if let Some(task) = tasks.get_mut(task_id) {
                task.status = status;
                task.progress = progress.min(100);
                task.message = message;
                task.result = result;
                task.error = error;
                persist_locked(state, &tasks);
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
            persist_locked(state, &tasks);
        }
        Ok(())
    }

    pub fn delete_many(state: &AppState, task_ids: &[String]) -> Result<usize, String> {
        if task_ids.is_empty() {
            return Ok(0);
        }
        let mut tasks = state.tasks.lock().map_err(|_| "lock task state failed".to_string())?;
        let mut unique_ids = std::collections::HashSet::<String>::new();
        for task_id in task_ids {
            if !unique_ids.insert(task_id.clone()) {
                continue;
            }
            let task = tasks.get(task_id).ok_or_else(|| "task not found".to_string())?;
            if matches!(task.status, TaskStatus::Pending | TaskStatus::Running) {
                return Err("进行中的任务不能删除，请先取消任务".to_string());
            }
        }
        let previous = tasks.clone();
        let removed = unique_ids
            .iter()
            .filter(|task_id| tasks.remove(task_id.as_str()).is_some())
            .count();
        if let Err(error) = TaskRepository::persist(&state.tasks_path, &tasks) {
            *tasks = previous;
            return Err(error);
        }
        Ok(removed)
    }

    pub fn get(state: &AppState, task_id: &str) -> Result<AppTask, String> {
        state.tasks.lock().map_err(|_| "lock task state failed".to_string())?.get(task_id).cloned().ok_or_else(|| "task not found".to_string())
    }

    pub fn list(state: &AppState) -> Result<Vec<AppTask>, String> {
        let mut tasks = state.tasks.lock().map_err(|_| "lock task state failed".to_string())?.values().cloned().collect::<Vec<_>>();
        tasks.sort_by(|left, right| right.created_at.cmp(&left.created_at));
        Ok(tasks)
    }
}

fn persist_locked(state: &AppState, tasks: &std::collections::HashMap<String, AppTask>) {
    if let Err(error) = TaskRepository::persist(&state.tasks_path, tasks) {
        crate::logging::error(format!("任务记录持久化失败：{error}"));
        eprintln!("GameSaver 任务记录持久化失败：{error}");
    }
}

fn now_millis() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis().to_string())
        .unwrap_or_else(|_| "0".to_string())
}
