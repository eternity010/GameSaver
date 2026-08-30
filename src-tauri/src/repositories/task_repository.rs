use crate::domain::{AppTask, TaskStatus};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, io::Write, path::{Path, PathBuf}};
use uuid::Uuid;

const TASK_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskStore {
    schema_version: u32,
    tasks: Vec<AppTask>,
}

pub struct TaskRepository;

impl TaskRepository {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("tasks.json")
    }

    pub fn load(path: &Path) -> Result<HashMap<String, AppTask>, String> {
        if !path.is_file() {
            return Ok(HashMap::new());
        }
        let raw = fs::read_to_string(path).map_err(|err| format!("读取任务记录失败：{err}"))?;
        let store = serde_json::from_str::<TaskStore>(&raw)
            .map_err(|err| format!("解析任务记录失败：{err}"))?;
        if store.schema_version != TASK_SCHEMA_VERSION {
            return Err(format!("不支持的任务记录版本：{}", store.schema_version));
        }
        let mut tasks = store
            .tasks
            .into_iter()
            .map(|mut task| {
                if matches!(task.status, TaskStatus::Pending | TaskStatus::Running) {
                    task.status = TaskStatus::Interrupted;
                    task.message = "应用上次未正常完成此任务".to_string();
                    task.error = Some("任务因应用关闭或异常退出而中断，可重新发起".to_string());
                }
                (task.task_id.clone(), task)
            })
            .collect::<HashMap<_, _>>();
        trim(&mut tasks);
        Ok(tasks)
    }

    pub fn persist(path: &Path, tasks: &HashMap<String, AppTask>) -> Result<(), String> {
        let parent = path.parent().ok_or_else(|| "任务记录路径无父目录".to_string())?;
        fs::create_dir_all(parent).map_err(|err| format!("创建任务记录目录失败：{err}"))?;
        let mut snapshot = tasks.clone();
        trim(&mut snapshot);
        let mut entries = snapshot.values().cloned().collect::<Vec<_>>();
        entries.sort_by(|left, right| left.created_at.cmp(&right.created_at));
        let store = TaskStore { schema_version: TASK_SCHEMA_VERSION, tasks: entries };
        let bytes = serde_json::to_vec_pretty(&store).map_err(|err| format!("序列化任务记录失败：{err}"))?;
        atomic_replace(path, &bytes)
    }
}

fn trim(tasks: &mut HashMap<String, AppTask>) {
    const MAX_TASKS: usize = 100;
    let mut finished = tasks
        .values()
        .filter(|task| !matches!(task.status, TaskStatus::Pending | TaskStatus::Running))
        .map(|task| (task.created_at.clone(), task.task_id.clone()))
        .collect::<Vec<_>>();
    finished.sort_by(|left, right| right.0.cmp(&left.0));
    for (_, task_id) in finished.into_iter().skip(MAX_TASKS) {
        tasks.remove(&task_id);
    }
}

fn atomic_replace(target: &Path, bytes: &[u8]) -> Result<(), String> {
    let temp = target.with_file_name(format!(".{}.tmp-{}", target.file_name().unwrap_or_default().to_string_lossy(), Uuid::new_v4().simple()));
    let backup = target.with_file_name(format!(".{}.bak-{}", target.file_name().unwrap_or_default().to_string_lossy(), Uuid::new_v4().simple()));
    let result = (|| -> Result<(), String> {
        let mut file = fs::File::create(&temp).map_err(|err| format!("创建任务记录临时文件失败：{err}"))?;
        file.write_all(bytes).map_err(|err| format!("写入任务记录临时文件失败：{err}"))?;
        file.sync_all().map_err(|err| format!("刷新任务记录临时文件失败：{err}"))?;
        let had_target = target.exists();
        if had_target {
            fs::rename(target, &backup).map_err(|err| format!("暂存任务记录失败：{err}"))?;
        }
        if let Err(err) = fs::rename(&temp, target) {
            if had_target {
                let _ = fs::rename(&backup, target);
            }
            return Err(format!("提交任务记录失败：{err}"));
        }
        if had_target {
            let _ = fs::remove_file(&backup);
        }
        Ok(())
    })();
    let _ = fs::remove_file(&temp);
    result
}

#[cfg(test)]
mod tests {
    use super::TaskRepository;
    use crate::domain::{AppTask, TaskRetry, TaskStatus};
    use std::{collections::HashMap, fs};
    use uuid::Uuid;

    fn task(status: TaskStatus) -> AppTask {
        AppTask {
            task_id: Uuid::new_v4().to_string(),
            task_type: "download_game_body_package".to_string(),
            status,
            progress: 42,
            message: "下载中".to_string(),
            game_uid: Some("game-1".to_string()),
            error: None,
            result: None,
            retry: Some(TaskRetry {
                operation: "download_game_body_package".to_string(),
                game_uid: "game-1".to_string(),
                version_id: None,
                remote_path: Some("/apps/GameSaver/games/game-1/body/v.zip".to_string()),
                remote_fs_id: Some(7),
            }),
            created_at: "1".to_string(),
            cancel_requested: false,
        }
    }

    #[test]
    fn interrupted_tasks_are_recovered_with_retry_data() {
        let root = std::env::temp_dir().join(format!("gamesaver-task-repository-{}", Uuid::new_v4()));
        let path = root.join("tasks.json");
        fs::create_dir_all(&root).expect("create task repository directory");
        let pending = task(TaskStatus::Running);
        let task_id = pending.task_id.clone();
        let tasks = HashMap::from([(task_id.clone(), pending)]);
        TaskRepository::persist(&path, &tasks).expect("persist task repository");

        let loaded = TaskRepository::load(&path).expect("load task repository");
        let recovered = loaded.get(&task_id).expect("recovered task");
        assert_eq!(recovered.status, TaskStatus::Interrupted);
        assert!(recovered.error.as_deref().unwrap_or_default().contains("中断"));
        assert_eq!(recovered.retry.as_ref().and_then(|retry| retry.remote_fs_id), Some(7));
        fs::remove_dir_all(root).expect("cleanup task repository");
    }

    #[test]
    fn completed_task_history_is_trimmed() {
        let root = std::env::temp_dir().join(format!("gamesaver-task-repository-{}", Uuid::new_v4()));
        let path = root.join("tasks.json");
        fs::create_dir_all(&root).expect("create task repository directory");
        let mut tasks = HashMap::new();
        for index in 0..105 {
            let mut current = task(TaskStatus::Success);
            current.created_at = index.to_string();
            tasks.insert(current.task_id.clone(), current);
        }
        TaskRepository::persist(&path, &tasks).expect("persist task repository");
        let loaded = TaskRepository::load(&path).expect("load task repository");
        assert_eq!(loaded.len(), 100);
        fs::remove_dir_all(root).expect("cleanup task repository");
    }
}
