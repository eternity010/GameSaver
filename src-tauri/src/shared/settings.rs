use serde::{Deserialize, Serialize};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SettingsPaths {
    pub(crate) backup_root: String,
    pub(crate) default_backup_root: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateSettingsPathsInput {
    pub(crate) backup_root: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
pub(crate) enum DataPathKind {
    BackupRoot,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DataPathMigrationResult {
    pub(crate) kind: DataPathKind,
    pub(crate) source_path: String,
    pub(crate) target_path: String,
    pub(crate) copied_files: usize,
    pub(crate) created_directories: usize,
    pub(crate) kept_original: bool,
}
