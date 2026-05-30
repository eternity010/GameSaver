use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GameLibraryItem {
    pub(crate) game_id: String,
    pub(crate) total_rules: usize,
    pub(crate) enabled_rules: usize,
    pub(crate) confirmed_path_count: usize,
    pub(crate) last_rule_updated_at: String,
    pub(crate) preferred_exe_path: Option<String>,
    pub(crate) last_session_id: Option<String>,
    pub(crate) last_session_status: Option<String>,
    pub(crate) last_session_updated_at: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LaunchPrecheckCheck {
    pub(crate) key: String,
    pub(crate) label: String,
    pub(crate) ok: bool,
    pub(crate) detail: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SaveLocationSummary {
    pub(crate) exists: bool,
    pub(crate) file_count: usize,
    pub(crate) total_bytes: u64,
    pub(crate) latest_modified_at: Option<String>,
    pub(crate) resolved_paths: Vec<String>,
    pub(crate) latest_version_id: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LaunchSyncDecision {
    pub(crate) status: String,
    pub(crate) message: String,
    pub(crate) recommended_action: String,
    pub(crate) local_summary: Option<SaveLocationSummary>,
    pub(crate) backup_summary: Option<SaveLocationSummary>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GameLaunchPrecheck {
    pub(crate) game_id: String,
    pub(crate) preferred_exe_path: Option<String>,
    pub(crate) exe_hash: Option<String>,
    pub(crate) matched_rule_id: Option<String>,
    pub(crate) backup_ready: bool,
    pub(crate) sync_decision: Option<LaunchSyncDecision>,
    pub(crate) checks: Vec<LaunchPrecheckCheck>,
    pub(crate) checked_at: String,
}
