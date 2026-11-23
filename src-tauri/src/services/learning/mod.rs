mod etw_capture;
mod native_etw;
mod trace_evidence;
mod transactions;
mod unity_player_prefs;

pub(crate) use etw_capture::{
    cleanup_stale_captures, collect_related_files_by_trace, extend_tracked_process_tree,
    stop_etw_capture, try_start_etw_capture,
};
#[cfg(test)]
pub(crate) use transactions::{analyze_save_transactions, analyze_save_transactions_for, FileOperation};
pub(crate) use transactions::{FileOperationKind, FileOperationTrace};
pub(crate) use unity_player_prefs::detect_unity_player_prefs;
