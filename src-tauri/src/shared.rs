#[path = "shared/backup.rs"]
mod backup;
#[path = "shared/config.rs"]
mod config;
#[path = "shared/launcher.rs"]
mod launcher;
#[path = "shared/learning.rs"]
mod learning;
#[path = "shared/migration.rs"]
mod migration;
#[path = "shared/precheck.rs"]
mod precheck;
#[path = "shared/rules.rs"]
mod rules;
#[path = "shared/runtime_info.rs"]
mod runtime_info;
#[path = "shared/store.rs"]
mod store;

pub(crate) use backup::{
    BackupManifest, BackupManifestFileItem, BackupRunResult, BackupStatsResult, BackupVersion,
    PruneBackupResult, RestoreBackupResult,
};
pub(crate) use config::ExecutionConfig;
pub(crate) use launcher::LauncherSession;
pub(crate) use learning::{CandidatePath, FileMeta, LearningSession, Snapshot};
pub(crate) use migration::{ExportMigrationZipResult, ImportMigrationZipResult};
pub(crate) use precheck::{
    GameLaunchPrecheck, GameLibraryItem, LaunchPrecheckCheck, LaunchSyncDecision,
    SaveLocationSummary,
};
pub(crate) use rules::{
    ExportRulesResult, GameSaveRule, ImportRuleInput, ImportRulesResult, RuleConflictItem,
};
pub(crate) use rules::ResolveRuleResult;
pub(crate) use runtime_info::RuntimeStatus;
pub(crate) use store::PersistedStore;
