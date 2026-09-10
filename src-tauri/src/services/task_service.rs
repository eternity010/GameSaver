use crate::{
    app_state::AppState,
    domain::{AppTask, TaskCategory, TaskRetry, TaskStatus},
    repositories::TaskRepository,
};
use uuid::Uuid;

pub struct TaskService;

impl TaskService {
    /// 创建一个后台任务。
    ///
    /// `category` 是**必填**的：它决定任务在前端是否可见、能否取消、是否计入角标。
    /// 做成必填参数而不是按 `task_type` 猜，是为了让「新增任务类型」在编译期就被
    /// 逼着表态——此前前端两份硬编码白名单就是因为新增类型时没人记得同步而漏掉。
    pub fn create(
        state: &AppState,
        task_type: &str,
        category: TaskCategory,
        game_uid: Option<String>,
        message: &str,
    ) -> Result<String, String> {
        let task_id = Uuid::new_v4().to_string();
        let task = AppTask {
            task_id: task_id.clone(),
            task_type: task_type.to_string(),
            category: Some(category),
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
        let mut tasks = state
            .tasks
            .lock()
            .map_err(|_| "lock task state failed".to_string())?;
        tasks.insert(task_id.clone(), task);
        if let Err(error) = TaskRepository::persist(&state.tasks_path, &tasks) {
            tasks.remove(&task_id);
            return Err(error);
        }
        drop(tasks);
        notify_changed(state, &task_id, "created");
        Ok(task_id)
    }

    pub fn set_retry(state: &AppState, task_id: &str, retry: TaskRetry) -> Result<(), String> {
        {
            let mut tasks = state
                .tasks
                .lock()
                .map_err(|_| "lock task state failed".to_string())?;
            let task = tasks
                .get_mut(task_id)
                .ok_or_else(|| "task not found".to_string())?;
            task.retry = Some(retry);
            persist_locked(state, &tasks);
        }
        notify_changed(state, task_id, "updated");
        Ok(())
    }

    /// 推进进度或状态。
    ///
    /// 刻意**不**落盘：进度更新可以到每个分片一次，高频写盘不划算，重启后由
    /// `TaskRepository::load` 把运行中的任务兜底成「异常中断」。但变化会推送事件
    /// （按 2% 粒度节流），前端因此不必再用高频轮询盯着进度。
    pub fn update(
        state: &AppState,
        task_id: &str,
        status: TaskStatus,
        progress: u8,
        message: impl Into<String>,
        error: Option<String>,
    ) {
        let progress = progress.min(100);
        let mut should_notify = false;
        let updated = if let Ok(mut tasks) = state.tasks.lock() {
            if let Some(task) = tasks.get_mut(task_id) {
                // 存档提交会按文件逐个回调，若每次回调都推一条，一场大存档会发出
                // 上千条消息。这里按 2% 粒度节流；前端另有 80ms 合并窗口兜底。
                should_notify = task.status != status
                    || progress == 100
                    || progress < task.progress
                    || progress.saturating_sub(task.progress) >= 2;
                task.status = status;
                task.progress = progress;
                task.message = message.into();
                task.error = error;
                true
            } else {
                false
            }
        } else {
            false
        };
        if updated && should_notify {
            notify_changed(state, task_id, "updated");
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
        let updated = if let Ok(mut tasks) = state.tasks.lock() {
            if let Some(task) = tasks.get_mut(task_id) {
                task.status = status;
                task.progress = progress.min(100);
                task.message = message;
                task.result = result;
                task.error = error;
                persist_locked(state, &tasks);
                true
            } else {
                false
            }
        } else {
            false
        };
        if updated {
            notify_changed(state, task_id, "finished");
        }
    }

    pub fn is_cancelled(state: &AppState, task_id: &str) -> bool {
        state
            .tasks
            .lock()
            .ok()
            .and_then(|tasks| tasks.get(task_id).map(|task| task.cancel_requested))
            .unwrap_or(true)
    }

    pub fn cancel(state: &AppState, task_id: &str) -> Result<(), String> {
        let requested = {
            let mut tasks = state
                .tasks
                .lock()
                .map_err(|_| "lock task state failed".to_string())?;
            let task = tasks
                .get_mut(task_id)
                .ok_or_else(|| "task not found".to_string())?;
            if matches!(task.status, TaskStatus::Pending | TaskStatus::Running) {
                task.cancel_requested = true;
                persist_locked(state, &tasks);
                true
            } else {
                false
            }
        };
        // 这里只是「请求取消」：真正的终态由执行线程收尾时用 finish 落盘。
        if requested {
            notify_changed(state, task_id, "updated");
        }
        Ok(())
    }

    pub fn delete_many(state: &AppState, task_ids: &[String]) -> Result<usize, String> {
        if task_ids.is_empty() {
            return Ok(0);
        }
        let mut tasks = state
            .tasks
            .lock()
            .map_err(|_| "lock task state failed".to_string())?;
        let mut unique_ids = std::collections::HashSet::<String>::new();
        for task_id in task_ids {
            if !unique_ids.insert(task_id.clone()) {
                continue;
            }
            let task = tasks
                .get(task_id)
                .ok_or_else(|| "task not found".to_string())?;
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
        drop(tasks);
        // 批量变更：task_id 留空，让前端整体刷新即可。
        notify_changed(state, "", "deleted");
        Ok(removed)
    }

    pub fn get(state: &AppState, task_id: &str) -> Result<AppTask, String> {
        state
            .tasks
            .lock()
            .map_err(|_| "lock task state failed".to_string())?
            .get(task_id)
            .cloned()
            .ok_or_else(|| "task not found".to_string())
    }

    pub fn list(state: &AppState) -> Result<Vec<AppTask>, String> {
        let mut tasks = state
            .tasks
            .lock()
            .map_err(|_| "lock task state failed".to_string())?
            .values()
            .cloned()
            .collect::<Vec<_>>();
        tasks.sort_by(|left, right| right.created_at.cmp(&left.created_at));
        Ok(tasks)
    }
}

/// 广播一次任务变更。**必须在释放任务锁之后调用**：本函数会再次加锁读取
/// `game_uid`，在持锁状态下调用会自锁死。
///
/// `task_id` 为空字符串表示批量变更（例如批量删除）。
fn notify_changed(state: &AppState, task_id: &str, kind: &'static str) {
    let game_uid = state
        .tasks
        .lock()
        .ok()
        .and_then(|tasks| tasks.get(task_id).and_then(|task| task.game_uid.clone()));
    crate::events::task_changed(state, task_id, kind, game_uid.as_deref());
}

fn persist_locked(state: &AppState, tasks: &std::collections::HashMap<String, AppTask>) {
    if let Err(error) = TaskRepository::persist(&state.tasks_path, tasks) {
        crate::logging::error(format!("任务记录持久化失败：{error}"));
        eprintln!("GameSaver 任务记录持久化失败：{error}");
    }
}

/// 当前时间的毫秒字符串，与前端 `Date.parse` 的口径一致。
fn now_millis() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::AppStore;

    fn test_state(dir: &std::path::Path) -> AppState {
        AppState::new(
            AppStore::default(),
            dir.to_path_buf(),
            std::collections::HashMap::new(),
            dir.join("tasks.json"),
        )
    }

    /// 云存档同步的"失败要响"就建立在这条契约上：终态、错误原因、重试参数都必须落盘。
    ///
    /// 如果这里退回 `TaskService::update`（只改内存、不落盘），重启后 `TaskRepository::load`
    /// 的兜底会把任务标成「异常中断」并覆盖掉 error —— 一次成功的同步看起来像故障，
    /// 一次真实的失败则连原因都丢了。
    #[test]
    fn failed_sync_task_keeps_status_error_and_retry_on_disk() {
        let dir =
            std::env::temp_dir().join(format!("gamesaver-task-service-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let state = test_state(&dir);

        let task_id = TaskService::create(
            &state,
            "sync_cloud_save",
            TaskCategory::CloudSaveSync,
            Some("game-1".to_string()),
            "上传【样例】游戏存档至百度网盘",
        )
        .expect("create task");
        // 进度更新走 update 是刻意的：高频写盘不划算，只有终态必须落盘。
        TaskService::update(
            &state,
            &task_id,
            TaskStatus::Running,
            40,
            "正在准备上传存档",
            None,
        );
        TaskService::set_retry(
            &state,
            &task_id,
            TaskRetry {
                operation: "sync_cloud_save".to_string(),
                game_uid: "game-1".to_string(),
                game_key: None,
                version_id: Some("version-1".to_string()),
                remote_path: None,
                remote_fs_id: None,
            },
        )
        .expect("set retry");
        TaskService::finish(
            &state,
            &task_id,
            TaskStatus::Failed,
            100,
            "【样例】游戏存档云端同步失败：token 已失效",
            None,
            Some("token 已失效".to_string()),
        );

        let reloaded = TaskRepository::load(&state.tasks_path).expect("reload tasks");
        let task = reloaded.get(&task_id).expect("task survives reload");
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.error.as_deref(), Some("token 已失效"));
        let retry = task.retry.as_ref().expect("retry payload survives reload");
        assert_eq!(retry.operation, "sync_cloud_save");
        assert_eq!(retry.version_id.as_deref(), Some("version-1"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 进度推送的节流只作用于事件，绝不能作用于任务状态本身——否则小步推进的
    /// 进度条会丢掉中间态，重启后读到的也是错的。
    #[test]
    fn progress_below_the_notify_threshold_still_updates_the_task() {
        let dir =
            std::env::temp_dir().join(format!("gamesaver-task-throttle-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let state = test_state(&dir);

        let task_id = TaskService::create(
            &state,
            "upload_game_body_package",
            TaskCategory::BodyTransfer,
            None,
            "准备上传",
        )
        .expect("create task");
        TaskService::update(&state, &task_id, TaskStatus::Running, 0, "开始上传", None);
        // 1% 的推进不会触发事件，但必须落到内存。
        TaskService::update(&state, &task_id, TaskStatus::Running, 1, "已完成 1%", None);

        let task = TaskService::get(&state, &task_id).expect("task exists");
        assert_eq!(task.progress, 1);
        assert_eq!(task.message, "已完成 1%");
        assert_eq!(task.status, TaskStatus::Running);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 分类是前端分流任务的唯一依据，创建时必须落盘——否则前端拿到 `null`，
    /// 任务会静默落到「不展示」分支。
    #[test]
    fn created_tasks_carry_their_category_on_disk() {
        let dir =
            std::env::temp_dir().join(format!("gamesaver-task-category-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let state = test_state(&dir);

        let task_id = TaskService::create(
            &state,
            "launch_game",
            TaskCategory::Session,
            Some("game-1".to_string()),
            "准备启动游戏",
        )
        .expect("create task");

        let reloaded = TaskRepository::load(&state.tasks_path).expect("reload tasks");
        assert_eq!(
            reloaded.get(&task_id).and_then(|task| task.category),
            Some(TaskCategory::Session)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
