use super::save_profile::SaveScope;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
    /// 范围目录里看起来也是存档、但**本次学习没有发生变化**的文件（相对路径）。
    ///
    /// 只在范围根目录不是命名存档容器时才有值。这类范围当前只认「学习那一刻看到的文件
    /// 清单」（`confirmed_files`），之后用户新建档位、游戏写带时间戳的自动存档都不会再进
    /// 来，而且没有任何提示。这里把目录里现存的疑似存档先列出来交给用户确认 ——
    /// **刻意不直接写进规则**：`is_save_candidate` 是启发式，误收别人的存档（典型是模拟器
    /// 共享的 `SAVEDATA` 目录）比漏收更难收拾，所以先让人看一眼。
    #[serde(default)]
    pub proposed_files: Vec<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_save_key: Option<String>,
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
    pub(crate) timed_out: std::sync::Arc<std::sync::atomic::AtomicBool>,
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
pub(crate) struct ProcessLifetime {
    pub pid: u32,
    pub started_at: i64,
    pub ended_at: i64,
}

impl ProcessLifetime {
    pub(crate) fn contains(&self, event_time: i64) -> bool {
        self.started_at <= event_time && event_time < self.ended_at
    }
}
