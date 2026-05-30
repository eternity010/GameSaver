use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LauncherSession {
    pub(crate) launcher_session_id: String,
    pub(crate) exe_path: String,
    pub(crate) exe_hash: String,
    pub(crate) matched_rule_id: Option<String>,
    pub(crate) matched_game_id: Option<String>,
    #[serde(default)]
    pub(crate) matched_game_uid: Option<String>,
    #[serde(default)]
    pub(crate) launch_mode: String,
    pub(crate) status: String,
    pub(crate) pid: Option<u32>,
    #[serde(default)]
    pub(crate) redirect_root: Option<String>,
    #[serde(default)]
    pub(crate) hook_version: Option<String>,
    pub(crate) started_at: String,
    pub(crate) updated_at: String,
    pub(crate) logs: Vec<String>,
}
