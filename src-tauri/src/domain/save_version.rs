use super::save_profile::SaveRootType;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveFileEntry {
    pub root_type: SaveRootType,
    pub root_path: Option<String>,
    pub relative_path: String,
    pub object_hash: Option<String>,
    pub size: u64,
    pub deleted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveVersion {
    pub version_id: String,
    pub game_uid: String,
    pub created_at: String,
    pub files: Vec<SaveFileEntry>,
    pub total_bytes: u64,
}

impl SaveVersion {
    pub fn new(game_uid: String, created_at: String, files: Vec<SaveFileEntry>) -> Self {
        Self {
            version_id: Uuid::new_v4().to_string(),
            game_uid,
            created_at,
            total_bytes: files.iter().map(|file| file.size).sum(),
            files,
        }
    }
}
