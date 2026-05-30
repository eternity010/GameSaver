use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExportMigrationZipResult {
    pub(crate) rule_count: usize,
    pub(crate) backup_games: usize,
    pub(crate) exported_files: usize,
    pub(crate) skipped_backup_games: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportMigrationZipResult {
    pub(crate) imported_rules: usize,
    pub(crate) overwritten_rules: usize,
    pub(crate) skipped_rules: usize,
    pub(crate) imported_backup_games: usize,
    pub(crate) copied_backup_files: usize,
    pub(crate) skipped_backup_games: usize,
}
