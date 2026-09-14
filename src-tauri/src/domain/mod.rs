pub mod game;
pub mod learning;
pub mod path_safety;
pub mod path_utils;
pub mod save_candidate;
pub mod save_profile;
pub mod save_version;
pub mod store;
pub mod task;
pub mod timestamp;

pub use game::{
    CoverCrop, CoverPosition, Game, GameBodyVersion, GameCover, GameHealth, GameLifecycle,
    GameRuntime, GameRuntimeStatus,
};
pub use learning::{
    ActiveLearningSession, EtwCaptureHandle, FileFingerprint, LearningSessionView, LearningStatus,
    SaveCandidateEvidenceLevel, SaveLearningResult, SaveScopeDraft, SaveTransactionSummary,
    ScanRoot,
};
pub use path_safety::is_safe_path_segment;
pub use save_profile::{
    detection_evidence_for, SaveProfile, SaveRootType, SaveScope, UnknownFilePolicy,
    DEFAULT_EXCLUDE_DIRECTORIES, DEFAULT_EXCLUDE_PATTERNS, DEFAULT_MAX_FILE_BYTES,
};
pub use save_version::{SaveFileEntry, SaveVersion};
pub use store::AppStore;
pub use task::{AppTask, TaskCategory, TaskRetry, TaskStatus, TaskSummary};
pub use timestamp::{compare_created_at, compare_optional_created_at};
