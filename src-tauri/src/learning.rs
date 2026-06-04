#[path = "learning/analysis.rs"]
mod analysis;
#[path = "learning/capture.rs"]
mod capture;
#[path = "learning/commands.rs"]
pub(crate) mod commands;
#[path = "learning/shared.rs"]
mod shared;
#[path = "learning/snapshot.rs"]
mod snapshot;

#[allow(unused_imports)]
pub(crate) use capture::is_running_as_admin;
#[allow(unused_imports)]
pub(crate) use commands::{
    cancel_learning, confirm_rule, finish_learning, get_learning_session, launch_game, open_candidate_path,
    start_finish_learning_task, start_learning, start_retry_finish_learning_task,
};
#[allow(unused_imports)]
pub(crate) use snapshot::normalize_learning_scan_root;
