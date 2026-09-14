use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Running,
    Success,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRetry {
    pub operation: String,
    pub game_uid: String,
    #[serde(default)]
    pub game_key: Option<String>,
    #[serde(default)]
    pub version_id: Option<String>,
    #[serde(default)]
    pub remote_path: Option<String>,
    #[serde(default)]
    pub remote_fs_id: Option<u64>,
}

/// 任务分类。
///
/// 这是**唯一**决定「任务在前端如何呈现」的依据：是否进入传输中心、能否取消、是否
/// 计入角标，全部由它派生。在此之前，这份分类是前端硬编码的两份逐字重复的字面量
/// 白名单，只覆盖 20 个任务类型里的 6 个，新增类型时会静默漏掉（`restore_save_version`
/// 这类写操作因此离开详情页就既看不到进度也无法取消）。
///
/// 新增任务类型时必须在 [`crate::services::TaskService::create`] 显式挑一个分类——
/// 那个参数是必填的，编译器会拦住遗漏。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskCategory {
    /// 本体传输：上传/下载/安装/删除云端本体、修复清单、更新与打包本体、卸载本体。
    /// 分片或文件级操作，可安全取消。
    BodyTransfer,
    /// 云存档同步：游戏退出后自动上传、手动还原云端存档。
    CloudSaveSync,
    /// 本地存档还原：把存档回滚到某个历史版本。
    ///
    /// **可见但不可取消**：还原前要先 commit 保护当前存档，还原本身又是对存档目录的
    /// 成批覆盖，中途放弃会留下写了一半的存档。要支持取消得先设计回滚，与此前登记的
    /// 「提交阶段取消无效」是同一类问题。
    SaveRestore,
    /// 游戏会话：`launch_game`。生命周期等于整场游戏，取消即终止整棵进程树。
    Session,
    /// 后台维护：识别存档、添加游戏、库迁移、账号同步、清理历史版本等，不进传输中心。
    Maintenance,
}

impl TaskCategory {
    /// 旧 `tasks.json` 没有 `category` 字段时的兜底推断。
    ///
    /// 只服务历史数据。新任务一律由 `TaskService::create` 显式指定，刻意不走这里，
    /// 免得又退化成「靠名字猜」的隐式白名单。
    pub fn infer(task_type: &str) -> Self {
        match task_type {
            "upload_game_body_package"
            | "download_game_body_package"
            | "install_cloud_game"
            | "delete_remote_body_package"
            | "repair_cloud_body_manifest"
            | "update_game_body"
            | "package_game_body"
            | "uninstall_game_body"
            | "delete_game_body_package" => Self::BodyTransfer,
            "sync_cloud_save" => Self::CloudSaveSync,
            "restore_save_version" => Self::SaveRestore,
            "launch_game" => Self::Session,
            _ => Self::Maintenance,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppTask {
    pub task_id: String,
    pub task_type: String,
    /// 任务分类。旧数据可能缺这个字段，`TaskRepository::load` 会用
    /// [`TaskCategory::infer`] 补齐；新建任务一律显式给定。
    #[serde(default)]
    pub category: Option<TaskCategory>,
    pub status: TaskStatus,
    pub progress: u8,
    pub message: String,
    #[serde(default)]
    pub game_uid: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub retry: Option<TaskRetry>,
    #[serde(default)]
    pub created_at: String,
    #[serde(skip)]
    pub cancel_requested: bool,
}

/// `list_tasks` 的列表视图：**刻意不含 `result`**。
///
/// `result` 是任意 JSON，没有上限。实测本机数据：42 条任务合计 2.32 MB，其中
/// **99.5% 是 `result`** —— 单个 `analyze_saves` 任务的 `transactionSummary` 就能占到
/// 600 KB。而任务列表界面一个 `result` 字段都不用，却每次轮询都要把这 2.32 MB 搬过
/// IPC 并在 WebView 里重新解析：空闲 10s 一次、有活跃任务 2s 一次（`taskFeed.ts`），
/// 而且每个 `task-changed` 进度事件（后端只带 id，前端收到就整体刷新）都会再触发一次。
///
/// 一次空闲轮询的实际代价（Performance trace 实测）：WebView 主线程一次 **14.72 ms**
/// 的任务（整段录制里最大的一次），紧接着一次 **V8 MajorGC**（2.3 MB 的 JS 对象分配）。
/// 任务在跑时这个频率还会翻几倍，正好表现为「滚的时候一阵一阵卡」。
///
/// 需要 `result` 的界面走 `get_task` 取完整 [`AppTask`]（`AddGameWizard` 就是这么做的）。
/// 字段与 [`AppTask`] 一一对应（除 `result` 外），保证前端类型不用动。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskSummary {
    pub task_id: String,
    pub task_type: String,
    /// 旧数据可能缺这个字段，`TaskCategory::infer` 会补齐（见 [`AppTask::category`]）。
    #[serde(default)]
    pub category: Option<TaskCategory>,
    pub status: TaskStatus,
    pub progress: u8,
    pub message: String,
    #[serde(default)]
    pub game_uid: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub retry: Option<TaskRetry>,
    #[serde(default)]
    pub created_at: String,
}

impl From<&AppTask> for TaskSummary {
    /// 刻意逐字段拷，而不是 `task.clone()` 再清空 `result`：后者会先把那 2.32 MB 的
    /// JSON 深拷贝一遍再丢掉，白白付出一次分配。这里 `result` 完全不碰。
    fn from(task: &AppTask) -> Self {
        Self {
            task_id: task.task_id.clone(),
            task_type: task.task_type.clone(),
            category: task.category,
            status: task.status.clone(),
            progress: task.progress,
            message: task.message.clone(),
            game_uid: task.game_uid.clone(),
            error: task.error.clone(),
            retry: task.retry.clone(),
            created_at: task.created_at.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_with_big_result() -> AppTask {
        AppTask {
            task_id: "t-1".to_string(),
            task_type: "analyze_saves".to_string(),
            category: Some(TaskCategory::Maintenance),
            status: TaskStatus::Success,
            progress: 100,
            message: "分析完成".to_string(),
            game_uid: Some("g-1".to_string()),
            error: None,
            result: Some(serde_json::json!({
                "transactionSummary": { "blob": "x".repeat(200_000) },
                "changedFiles": ["a", "b", "c"],
            })),
            retry: None,
            created_at: "1789050204989".to_string(),
            cancel_requested: false,
        }
    }

    /// `list_tasks` 的返回值必须**不含 `result`**，且体积与 `result` 无关。
    ///
    /// 这是实测挖出来的缺陷：任务列表每次轮询都搬运 2.32 MB，其中 99.5% 是各任务的
    /// `result`，列表界面一个字段都不用 —— 代价是主线程一次 14.72ms 的任务加一次 V8
    /// MajorGC。谁要是把 `result` 加回摘要，这条测试会立刻失败。
    #[test]
    fn task_summary_omits_result() {
        let task = task_with_big_result();
        let json = serde_json::to_string(&TaskSummary::from(&task)).expect("摘要应当可序列化");
        assert!(
            !json.contains("result") && !json.contains("transactionSummary"),
            "任务摘要里不允许出现 result（列表界面不用它，代价是每次轮询 2.32 MB）：{json}"
        );
        assert!(
            json.len() < 1_024,
            "任务摘要应当只有几百字节，实测 {} 字节 —— 说明又混进了大字段：{json}",
            json.len()
        );
        // 完整任务仍然带着 result（get_task 要用），确认两个视图确实不同。
        let full = serde_json::to_string(&task).expect("完整任务应当可序列化");
        assert!(
            full.len() > 200_000,
            "完整任务应当仍带 result，实测 {} 字节",
            full.len()
        );
    }

    /// 摘要必须保留列表界面真正要用的字段，别为了瘦身把功能删了。
    #[test]
    fn task_summary_keeps_list_fields() {
        let json =
            serde_json::to_value(TaskSummary::from(&task_with_big_result())).expect("可序列化");
        for key in [
            "taskId",
            "taskType",
            "category",
            "status",
            "progress",
            "message",
            "gameUid",
            "error",
            "retry",
            "createdAt",
        ] {
            assert!(json.get(key).is_some(), "摘要缺字段 {key}：{json}");
        }
    }
}
