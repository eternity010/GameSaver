use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GameSaveRule {
    pub(crate) rule_id: String,
    pub(crate) game_id: String,
    #[serde(default)]
    pub(crate) game_uid: String,
    pub(crate) exe_hash: String,
    pub(crate) confirmed_paths: Vec<String>,
    pub(crate) created_at: String,
    pub(crate) confidence: i64,
    #[serde(default = "default_true")]
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) updated_at: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExportRulesResult {
    pub(crate) count: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportRulesResult {
    pub(crate) imported: usize,
    pub(crate) overwritten: usize,
    pub(crate) skipped: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResolveRuleResult {
    pub(crate) exe_hash: String,
    pub(crate) matched_rule: Option<GameSaveRule>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuleConflictItem {
    pub(crate) exe_hash: String,
    pub(crate) rule_ids: Vec<String>,
    pub(crate) game_ids: Vec<String>,
    pub(crate) primary_rule_id: Option<String>,
    pub(crate) conflict_count: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportRuleInput {
    pub(crate) rule_id: Option<String>,
    pub(crate) game_id: String,
    pub(crate) game_uid: Option<String>,
    pub(crate) exe_hash: String,
    pub(crate) confirmed_paths: Vec<String>,
    pub(crate) created_at: Option<String>,
    pub(crate) updated_at: Option<String>,
    pub(crate) confidence: Option<i64>,
    pub(crate) enabled: Option<bool>,
}
