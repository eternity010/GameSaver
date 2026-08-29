use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Running,
    Success,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppTask {
    pub task_id: String,
    pub task_type: String,
    pub status: TaskStatus,
    pub progress: u8,
    pub message: String,
    #[serde(default)]
    pub game_uid: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(skip)]
    pub cancel_requested: bool,
}
