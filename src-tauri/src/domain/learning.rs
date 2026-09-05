use super::save_profile::SaveScope;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc, Mutex},
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LearningStatus {
    Capturing,
    Finished,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LearningSessionView {
    pub session_id: String,
    pub game_uid: String,
    pub root_pid: u32,
    pub started_at: String,
    pub status: LearningStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveScopeDraft {
    pub scope: SaveScope,
    pub changed_files: Vec<String>,
    pub confidence: u8,
    pub evidence_level: SaveCandidateEvidenceLevel,
    pub evidence_reason: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SaveCandidateEvidenceLevel {
    Strong,
    Review,
    Noise,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveLearningResult {
    pub session_id: String,
    pub changed_files: Vec<String>,
    pub scope_drafts: Vec<SaveScopeDraft>,
    pub confidence: u8,
    pub notes: Vec<String>,
    #[serde(default)]
    pub event_capture_mode: String,
    #[serde(default)]
    pub transaction_summary: Option<SaveTransactionSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SaveTransactionSummary {
    pub status: String,
    pub confidence: u8,
    pub transaction_count: usize,
    pub affected_files: Vec<String>,
    pub affected_directories: Vec<String>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub ended_at: Option<String>,
    pub operation_count: usize,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct EtwCaptureHandle {
    pub trace_name: String,
    pub etl_path: PathBuf,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FileFingerprint {
    pub size: u64,
    pub modified_unix: u64,
}

#[derive(Debug, Clone)]
pub struct ScanRoot {
    pub root_type: super::save_profile::SaveRootType,
    pub physical_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ActiveLearningSession {
    pub view: LearningSessionView,
    pub roots: Vec<ScanRoot>,
    pub baseline: Option<HashMap<String, FileFingerprint>>,
    pub tracked_pids: Arc<Mutex<Vec<u32>>>,
    pub process_tracker_stop: Arc<AtomicBool>,
    pub process_tracker_done: Arc<AtomicBool>,
    pub etw_capture: Option<EtwCaptureHandle>,
    pub etw_start_error: Option<String>,
    pub validation_only: bool,
}
