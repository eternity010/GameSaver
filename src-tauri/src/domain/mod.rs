pub mod game;
pub mod learning;
pub mod save_profile;
pub mod save_version;
pub mod store;
pub mod task;

pub use game::{
    CoverCrop, CoverPosition, Game, GameBodyVersion, GameCover, GameHealth, GameLifecycle,
    GameRuntime, GameRuntimeStatus,
};
pub use learning::{
    ActiveLearningSession, EtwCaptureHandle, FileFingerprint, LearningSessionView, LearningStatus,
    SaveCandidateEvidenceLevel, SaveLearningResult, SaveScopeDraft, SaveTransactionSummary,
    ScanRoot,
};
pub use save_profile::{
    SaveProfile, SaveRootType, SaveScope, UnknownFilePolicy, DEFAULT_EXCLUDE_DIRECTORIES,
    DEFAULT_EXCLUDE_PATTERNS,
};
pub use save_version::{SaveFileEntry, SaveVersion};
pub use store::AppStore;
pub use task::{AppTask, TaskRetry, TaskStatus};
