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
