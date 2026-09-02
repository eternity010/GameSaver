use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SaveRootType {
    ManagedGame,
    AppData,
    LocalAppData,
    LocalLow,
    Documents,
    SavedGames,
    UserProfile,
    Custom,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UnknownFilePolicy {
    Protect,
    Ignore,
}

impl Default for UnknownFilePolicy {
    fn default() -> Self {
        Self::Protect
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveScope {
    pub root_type: SaveRootType,
    pub root_path: String,
    #[serde(default)]
    pub confirmed_files: Vec<String>,
    #[serde(default)]
    pub include_directories: Vec<String>,
    #[serde(default)]
    pub exclude_exact: Vec<String>,
    #[serde(default)]
    pub exclude_patterns: Vec<String>,
    #[serde(default)]
    pub exclude_directories: Vec<String>,
    #[serde(default)]
    pub unknown_file_policy: UnknownFilePolicy,
    #[serde(default = "default_max_file_bytes")]
    pub max_file_bytes: Option<u64>,
}

fn default_max_file_bytes() -> Option<u64> {
    Some(10 * 1024 * 1024)
}

fn default_keep_versions() -> usize {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveProfile {
    pub profile_id: String,
    pub game_uid: String,
    pub executable_hash: String,
    pub scopes: Vec<SaveScope>,
    pub detection_evidence: Vec<String>,
    pub confidence: u8,
    pub enabled: bool,
    #[serde(default = "default_keep_versions")]
    pub keep_versions: usize,
    pub created_at: String,
    pub updated_at: String,
}

impl SaveProfile {
    pub fn new(game_uid: String, executable_hash: String, scopes: Vec<SaveScope>, confidence: u8, now: String) -> Self {
        Self {
            profile_id: Uuid::new_v4().to_string(),
            game_uid,
            executable_hash,
            scopes,
            detection_evidence: vec!["snapshot_diff".to_string(), "folder_grouping".to_string()],
            confidence,
            enabled: true,
            keep_versions: 5,
            created_at: now.clone(),
            updated_at: now,
        }
    }
}
