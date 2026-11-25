pub mod add_game_service;
pub(crate) mod baidu_auth_service;
pub mod baidu_netdisk_service;
pub mod cloud_account_service;
pub mod cloud_manifest_service;
pub mod cloud_save_service;
pub(crate) mod cloud_store_cache;
pub mod cover_capture_service;
pub mod disk_space;
pub mod game_body_install_service;
pub mod game_body_package_service;
pub mod game_body_update_service;
pub(crate) mod game_cover_service;
pub mod game_library_service;
pub mod game_removal_service;
pub mod instance_service;
pub mod launch_service;
pub(crate) mod learning;
pub mod library_service;
pub mod process_service;
pub mod save_learning_service;
pub mod task_service;
#[cfg(target_os = "windows")]
pub(crate) mod window_placement;

pub use add_game_service::AddGameService;
#[cfg(test)]
pub use baidu_netdisk_service::BaiduToken;
pub use baidu_netdisk_service::{
    BaiduConnectionStatus, BaiduNetdiskClient, BaiduQuota, RemoteFile,
};
pub use cloud_account_service::{CloudAccountProfile, CloudAccountService};
pub use cloud_manifest_service::{
    CloudGamePage, CloudGameSummary, CloudManifestService,
    RemoteBodyPackageList,
};
pub use cloud_save_service::{
    CloudSaveManifestVersion, CloudSaveOverview, CloudSaveService, CloudSaveSyncStatusView,
};
pub use cover_capture_service::{CaptureArmView, CoverCaptureService};
pub use game_body_install_service::GameBodyInstallService;
pub use game_body_package_service::BodyPackageService;
pub use game_body_update_service::GameBodyUpdateService;
pub use game_library_service::GameLibraryService;
pub use game_removal_service::{GameRemovalService, RemovalKind, RemovalOutcome, RemovalRecovery};
pub use instance_service::InstanceService;
pub use launch_service::LaunchPrecheck;
pub use launch_service::LaunchService;
pub use library_service::LibraryService;
pub use save_learning_service::SaveLearningService;
pub use task_service::TaskService;
