pub mod game;
pub mod store;
pub mod task;
pub mod learning;
pub mod save_profile;
pub mod save_version;

pub use game::{CoverCrop, CoverPosition, Game, GameBodyVersion, GameCover, GameHealth, GameLifecycle, GameRuntime, GameRuntimeStatus};
pub use store::AppStore;
pub use task::{AppTask, TaskRetry, TaskStatus};
pub use learning::{ActiveLearningSession, EtwCaptureHandle, FileFingerprint, LearningSessionView, LearningStatus, SaveLearningResult, SaveScopeDraft, SaveTransactionSummary, ScanRoot};
pub use save_profile::{DEFAULT_EXCLUDE_DIRECTORIES, DEFAULT_EXCLUDE_PATTERNS, SaveProfile, SaveRootType, SaveScope, UnknownFilePolicy};
pub use save_version::{SaveFileEntry, SaveVersion};
