use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileMeta {
    pub(crate) size: u64,
    pub(crate) modified_unix: u64,
    pub(crate) extension: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Snapshot {
    pub(crate) snapshot_ref: String,
    pub(crate) created_at_unix: u64,
    pub(crate) files: HashMap<String, FileMeta>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CandidatePath {
    pub(crate) path: String,
    pub(crate) score: i64,
    pub(crate) changed_files: usize,
    pub(crate) added_files: usize,
    pub(crate) modified_files: usize,
    pub(crate) matched_signals: Vec<String>,
    pub(crate) recommendation: String,
    pub(crate) collapsed: bool,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LearningSession {
    pub(crate) session_id: String,
    pub(crate) game_id: String,
    pub(crate) exe_path: String,
    pub(crate) started_at: String,
    pub(crate) ended_at: Option<String>,
    pub(crate) status: String,
    pub(crate) baseline_snapshot_ref: String,
    pub(crate) final_snapshot_ref: Option<String>,
    pub(crate) candidates: Vec<CandidatePath>,
    pub(crate) pid: Option<u32>,
    #[serde(default)]
    pub(crate) tracked_pids: Vec<u32>,
    #[serde(default)]
    pub(crate) event_capture_mode: String,
    #[serde(default)]
    pub(crate) event_trace_name: Option<String>,
    #[serde(default)]
    pub(crate) event_trace_path: Option<String>,
    #[serde(default)]
    pub(crate) captured_event_count: usize,
    #[serde(default)]
    pub(crate) event_capture_error: Option<String>,
    #[serde(default)]
    pub(crate) extra_scan_roots: Vec<String>,
}
