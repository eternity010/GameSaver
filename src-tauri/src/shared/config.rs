use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExecutionConfig {
    pub(crate) managed_save_root: String,
    pub(crate) backup_root: String,
    #[serde(default)]
    pub(crate) preferred_exe_by_uid: HashMap<String, String>,
    #[serde(default)]
    pub(crate) preferred_rule_uid_by_game: HashMap<String, String>,
    #[serde(default)]
    pub(crate) preferred_rule_id_by_exe_hash: HashMap<String, String>,
    #[serde(default)]
    pub(crate) backup_keep_versions_by_uid: HashMap<String, usize>,
    #[serde(default)]
    pub(crate) extra_learning_scan_roots: Vec<String>,
    #[serde(default, alias = "preferredExeByGame", skip_serializing)]
    pub(crate) preferred_exe_by_game_legacy: HashMap<String, String>,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            managed_save_root: String::new(),
            backup_root: String::new(),
            preferred_exe_by_uid: HashMap::new(),
            preferred_rule_uid_by_game: HashMap::new(),
            preferred_rule_id_by_exe_hash: HashMap::new(),
            backup_keep_versions_by_uid: HashMap::new(),
            extra_learning_scan_roots: Vec::new(),
            preferred_exe_by_game_legacy: HashMap::new(),
        }
    }
}
