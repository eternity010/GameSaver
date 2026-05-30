use serde::{Deserialize, Serialize};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BackupVersion {
    pub(crate) version_id: String,
    pub(crate) created_at: String,
    pub(crate) file_count: usize,
    pub(crate) label: String,
    pub(crate) restorable: bool,
}

pub(crate) struct BackupRunResult {
    pub(crate) changed_files: usize,
    pub(crate) version_id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RestoreBackupResult {
    pub(crate) game_id: String,
    pub(crate) version_id: String,
    pub(crate) restored_files: usize,
    pub(crate) pre_restore_version_id: Option<String>,
    pub(crate) verified_files: usize,
    pub(crate) hash_sample_count: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BackupStatsResult {
    pub(crate) game_id: String,
    pub(crate) game_uid: String,
    pub(crate) total_bytes: u64,
    pub(crate) version_count: usize,
    pub(crate) latest_version_id: Option<String>,
    pub(crate) keep_versions: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PruneBackupResult {
    pub(crate) game_id: String,
    pub(crate) game_uid: String,
    pub(crate) keep_versions: usize,
    pub(crate) deleted_versions: usize,
    pub(crate) freed_bytes: u64,
    pub(crate) remaining_versions: usize,
    pub(crate) remaining_bytes: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BackupManifestFileItem {
    pub(crate) path: String,
    pub(crate) size: u64,
    pub(crate) sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BackupManifest {
    pub(crate) format: String,
    pub(crate) created_at: String,
    pub(crate) game_uid: String,
    #[serde(default)]
    pub(crate) files: Vec<BackupManifestFileItem>,
}
