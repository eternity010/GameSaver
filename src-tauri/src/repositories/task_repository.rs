use crate::domain::{compare_created_at, AppTask, TaskCategory, TaskStatus};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use super::store_file::{atomic_replace, load_with_recovery};

const TASK_SCHEMA_VERSION: u32 = 1;
const TASK_LABEL: &str = "任务记录";

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
        let Some(raw) = load_with_recovery(path, TASK_LABEL, |bytes: &[u8]| {
            serde_json::from_slice::<TaskStore>(bytes).is_ok()
        })?
        else {
            return Ok(HashMap::new());
        };
        let store = serde_json::from_slice::<TaskStore>(&raw)
            .map_err(|err| format!("解析{TASK_LABEL}失败：{err}"))?;
        if store.schema_version != TASK_SCHEMA_VERSION {
            return Err(format!(
                "不支持的{TASK_LABEL}版本：{}",
                store.schema_version
            ));
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
                // 升级前落盘的记录没有 `category`：补一次推断并写回内存态，否则前端
                // 拿不到分类，历史同步任务的「存档未同步」红标会在重启后消失。
                if task.category.is_none() {
                    task.category = Some(TaskCategory::infer(&task.task_type));
                }
                (task.task_id.clone(), task)
            })
            .collect::<HashMap<_, _>>();
        trim(&mut tasks);
        Ok(tasks)
    }

    pub fn persist(path: &Path, tasks: &HashMap<String, AppTask>) -> Result<(), String> {
        let mut snapshot = tasks.clone();
        trim(&mut snapshot);
        let mut entries = snapshot.values().cloned().collect::<Vec<_>>();
        entries.sort_by(|left, right| compare_created_at(&left.created_at, &right.created_at));
        let store = TaskStore {
            schema_version: TASK_SCHEMA_VERSION,
            tasks: entries,
        };
        let bytes = serde_json::to_vec_pretty(&store)
            .map_err(|err| format!("序列化{TASK_LABEL}失败：{err}"))?;
        atomic_replace(path, &bytes, TASK_LABEL)
    }
}

fn trim(tasks: &mut HashMap<String, AppTask>) {
    const MAX_TASKS: usize = 100;
    let mut finished = tasks
        .values()
        .filter(|task| !matches!(task.status, TaskStatus::Pending | TaskStatus::Running))
        .map(|task| (task.created_at.clone(), task.task_id.clone()))
        .collect::<Vec<_>>();
    finished.sort_by(|left, right| compare_created_at(&right.0, &left.0));
    for (_, task_id) in finished.into_iter().skip(MAX_TASKS) {
        tasks.remove(&task_id);
    }
}

#[cfg(test)]
mod tests {
    use super::TaskRepository;
    use crate::domain::{AppTask, TaskCategory, TaskRetry, TaskStatus};
    use std::{collections::HashMap, fs};
    use uuid::Uuid;

    fn task(status: TaskStatus) -> AppTask {
        AppTask {
            task_id: Uuid::new_v4().to_string(),
            task_type: "download_game_body_package".to_string(),
            category: Some(TaskCategory::BodyTransfer),
            status,
            progress: 42,
            message: "下载中".to_string(),
            game_uid: Some("game-1".to_string()),
            error: None,
            result: None,
            retry: Some(TaskRetry {
                operation: "download_game_body_package".to_string(),
                game_uid: "game-1".to_string(),
                game_key: None,
                version_id: None,
                remote_path: Some("/apps/GameSaver/games/game-1/body/v.zip".to_string()),
                remote_fs_id: Some(7),
            }),
            created_at: "1".to_string(),
            cancel_requested: false,
            cancellable: None,
        }
    }

    /// 建一棵带 `Drop` 清理的测试目录树，理由见 `crate::test_support::TempWorkspace`。
    ///
    /// 此前四处测试各自 `create_dir_all` + 末尾 `remove_dir_all`：前者是必要的（`persist`
    /// 不建父目录），后者在 `assert!` 失败时执行不到，于是 `%TEMP%` 下攒下残留。
    fn temp_root() -> crate::test_support::TempWorkspace {
        crate::test_support::TempWorkspace::new("task-repository")
    }

    #[test]
    fn missing_repository_loads_as_empty_without_creating_a_file() {
        let root = temp_root();
        let path = root.join("tasks.json");
        assert!(TaskRepository::load(&path).unwrap().is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn corrupt_repository_is_rejected_and_preserved() {
        let root = temp_root();
        let path = root.join("tasks.json");
        let corrupt = b"{\"schemaVersion\":1,\"tasks\":[";
        fs::write(&path, corrupt).unwrap();
        assert!(TaskRepository::load(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), corrupt);
    }

    #[test]
    fn unsupported_repository_schema_is_rejected_and_preserved() {
        let root = temp_root();
        let path = root.join("tasks.json");
        let unsupported = br#"{"schemaVersion":999,"tasks":[]}"#;
        fs::write(&path, unsupported).unwrap();
        let error = TaskRepository::load(&path).unwrap_err();
        assert!(error.contains("999"));
        assert_eq!(fs::read(&path).unwrap(), unsupported);
    }

    #[test]
    fn restart_interrupts_only_unfinished_tasks_and_preserves_terminal_results() {
        let root = temp_root();
        let path = root.join("tasks.json");
        let mut tasks = HashMap::new();
        for status in [
            TaskStatus::Pending,
            TaskStatus::Running,
            TaskStatus::Success,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
            TaskStatus::Interrupted,
        ] {
            let mut current = task(status);
            current.error = Some("previous error".into());
            current.result = Some(serde_json::json!({"versionId": "version-1"}));
            current.cancel_requested = true;
            tasks.insert(current.task_id.clone(), current);
        }
        TaskRepository::persist(&path, &tasks).unwrap();
        let loaded = TaskRepository::load(&path).unwrap();
        assert_eq!(loaded.len(), tasks.len());
        for (id, before) in &tasks {
            let after = &loaded[id];
            if matches!(before.status, TaskStatus::Pending | TaskStatus::Running) {
                assert_eq!(after.status, TaskStatus::Interrupted);
                assert!(after.error.as_deref().unwrap().contains("中断"));
            } else {
                assert_eq!(after.status, before.status);
                assert_eq!(after.message, before.message);
                assert_eq!(after.error, before.error);
            }
            assert_eq!(after.progress, before.progress);
            assert_eq!(after.game_uid, before.game_uid);
            assert_eq!(after.result, before.result);
            assert_eq!(
                serde_json::to_value(&after.retry).unwrap(),
                serde_json::to_value(&before.retry).unwrap()
            );
            assert!(!after.cancel_requested);
        }
    }

    #[test]
    fn history_retention_keeps_the_latest_numeric_timestamps() {
        let root = temp_root();
        let path = root.join("tasks.json");
        let mut tasks = HashMap::new();
        for index in 1..=105 {
            let mut current = task(TaskStatus::Success);
            current.task_id = format!("task-{index}");
            current.created_at = index.to_string();
            tasks.insert(current.task_id.clone(), current);
        }
        TaskRepository::persist(&path, &tasks).unwrap();
        assert_eq!(tasks.len(), 105);
        let loaded = TaskRepository::load(&path).unwrap();
        assert_eq!(loaded.len(), 100);
        for index in 1..=105 {
            assert_eq!(
                loaded.contains_key(&format!("task-{index}")),
                index > 5,
                "unexpected retention for timestamp {index}"
            );
        }
    }

    #[test]
    fn persistence_preserves_all_active_tasks_beyond_the_history_limit() {
        let root = temp_root();
        let path = root.join("tasks.json");
        let mut tasks = HashMap::new();
        for index in 0..215 {
            let status = match index {
                0..=54 => TaskStatus::Pending,
                55..=109 => TaskStatus::Running,
                _ => TaskStatus::Success,
            };
            let mut current = task(status);
            current.created_at = format!("{index:04}");
            tasks.insert(current.task_id.clone(), current);
        }
        TaskRepository::persist(&path, &tasks).unwrap();
        let persisted: super::TaskStore =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted.tasks.len(), 210);
        assert_eq!(
            persisted
                .tasks
                .iter()
                .filter(|task| matches!(task.status, TaskStatus::Pending | TaskStatus::Running))
                .count(),
            110
        );
        assert_eq!(tasks.len(), 215);
    }

    #[test]
    fn interrupted_tasks_are_recovered_with_retry_data() {
        let root = temp_root();
        let path = root.join("tasks.json");
        let pending = task(TaskStatus::Running);
        let task_id = pending.task_id.clone();
        let tasks = HashMap::from([(task_id.clone(), pending)]);
        TaskRepository::persist(&path, &tasks).expect("persist task repository");

        let loaded = TaskRepository::load(&path).expect("load task repository");
        let recovered = loaded.get(&task_id).expect("recovered task");
        assert_eq!(recovered.status, TaskStatus::Interrupted);
        assert!(recovered
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("中断"));
        assert_eq!(
            recovered
                .retry
                .as_ref()
                .and_then(|retry| retry.remote_fs_id),
            Some(7)
        );
        fs::remove_dir_all(root).expect("cleanup task repository");
    }

    #[test]
    fn completed_task_history_is_trimmed() {
        let root = temp_root();
        let path = root.join("tasks.json");
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

    #[test]
    fn tasks_are_recovered_when_last_write_crashed_mid_replace() {
        let root = temp_root();
        let path = root.join("tasks.json");
        let saved = task(TaskStatus::Success);
        let task_id = saved.task_id.clone();
        TaskRepository::persist(&path, &HashMap::from([(task_id.clone(), saved)]))
            .expect("persist");

        // 模拟崩溃现场：主文件被改名为备份，新文件尚未到位。
        let backup = root.join(".tasks.json.bak-0000");
        fs::rename(&path, &backup).expect("stage crash");

        let loaded = TaskRepository::load(&path).expect("load task repository");

        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains_key(&task_id));
        fs::remove_dir_all(root).expect("cleanup task repository");
    }

    /// 升级前的 `tasks.json` 没有 `category` 字段，加载时必须补齐，否则前端拿不到
    /// 分类，历史同步任务的「存档未同步」红标会在重启后消失。
    #[test]
    fn legacy_tasks_without_category_get_one_inferred() {
        let root = temp_root();
        let path = root.join("tasks.json");
        let legacy = r#"{"schemaVersion":1,"tasks":[
            {"taskId":"legacy-sync","taskType":"sync_cloud_save","status":"failed","progress":100,"message":"同步失败","createdAt":"1"},
            {"taskId":"legacy-launch","taskType":"launch_game","status":"success","progress":100,"message":"已结束","createdAt":"2"},
            {"taskId":"legacy-unknown","taskType":"brand_new_type","status":"success","progress":100,"message":"x","createdAt":"3"}
        ]}"#;
        fs::write(&path, legacy).expect("write legacy task store");

        let loaded = TaskRepository::load(&path).expect("load legacy task store");

        let category = |task_id: &str| loaded.get(task_id).and_then(|task| task.category);
        assert_eq!(category("legacy-sync"), Some(TaskCategory::CloudSaveSync));
        assert_eq!(category("legacy-launch"), Some(TaskCategory::Session));
        // 不认识的类型必须落到「不展示」，而不是随手塞进某个可见分类。
        assert_eq!(category("legacy-unknown"), Some(TaskCategory::Maintenance));
        fs::remove_dir_all(root).expect("cleanup task repository");
    }
}
