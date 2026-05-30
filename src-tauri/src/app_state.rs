use crate::shared::PersistedStore;
use serde::Serialize;
use std::{collections::HashMap, sync::Mutex};

pub(crate) struct AppState {
    pub(crate) store: Mutex<PersistedStore>,
    pub(crate) tasks: Mutex<HashMap<String, BackgroundTask>>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BackgroundTask {
    pub(crate) task_id: String,
    pub(crate) task_type: String,
    pub(crate) status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) progress: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    pub(crate) started_at: String,
    pub(crate) updated_at: String,
}
