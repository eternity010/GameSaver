use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SaveRootType {
    ManagedGame,
    AppData,
    LocalAppData,
    LocalLow,
    Documents,
    SavedGames,
    UserProfile,
    Custom,
    /// `%PROGRAMDATA%`：全用户安装 / 老游戏 / 部分日系游戏的存档落点。
    ///
    /// 单独成一个变体而不是复用 `Custom`：它是环境变量可重定位的标准根，云端回传时
    /// 靠 `root_type + sub_path` 就能在另一台机器上重建路径，而 `Custom` 只能携带源
    /// 机器的绝对路径。
    ProgramData,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UnknownFilePolicy {
    Protect,
    Ignore,
}

impl Default for UnknownFilePolicy {
    fn default() -> Self {
        Self::Protect
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveScope {
    pub root_type: SaveRootType,
    pub root_path: String,
    #[serde(default)]
    pub confirmed_files: Vec<String>,
    #[serde(default)]
    pub include_directories: Vec<String>,
    #[serde(default)]
    pub exclude_exact: Vec<String>,
    #[serde(default)]
    pub exclude_patterns: Vec<String>,
    #[serde(default)]
    pub exclude_directories: Vec<String>,
    #[serde(default)]
    pub unknown_file_policy: UnknownFilePolicy,
    #[serde(default = "default_max_file_bytes")]
    pub max_file_bytes: Option<u64>,
}

pub const DEFAULT_EXCLUDE_PATTERNS: [&str; 8] = [
    "*.tmp", "*.temp", "*.log", "*.dmp", "*.bak", "*.etl", "*.csv", "*.cache",
];

pub const DEFAULT_EXCLUDE_DIRECTORIES: [&str; 9] = [
    "logs",
    "crashdumps",
    "cache",
    "shadercache",
    "shader_cache",
    "webcache",
    "gpucache",
    "d3dscache",
    "vulkan",
];

impl SaveScope {
    #[allow(dead_code)]
    pub fn new_manual(root_path: String) -> Self {
        Self {
            root_type: SaveRootType::Custom,
            root_path,
            confirmed_files: Vec::new(),
            include_directories: vec![".".to_string()],
            exclude_exact: Vec::new(),
            exclude_patterns: DEFAULT_EXCLUDE_PATTERNS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            exclude_directories: DEFAULT_EXCLUDE_DIRECTORIES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            unknown_file_policy: UnknownFilePolicy::Protect,
            max_file_bytes: default_max_file_bytes(),
        }
    }

    pub fn ensure_default_exclusions_if_empty(&mut self) {
        if self.exclude_patterns.is_empty() {
            self.exclude_patterns = DEFAULT_EXCLUDE_PATTERNS
                .iter()
                .map(|s| s.to_string())
                .collect();
        }
        if self.exclude_directories.is_empty() {
            self.exclude_directories = DEFAULT_EXCLUDE_DIRECTORIES
                .iter()
                .map(|s| s.to_string())
                .collect();
        }
    }
}

/// 单个存档文件的管理上限，单位字节。
///
/// 这是「我们愿意管理多大的文件」这条线的唯一定义：超过它的文件既不进版本库
/// （收集侧 `collect` 直接跳过），恢复时也不受保护（`is_protected_file` 判否）。
///
/// 存档识别产出的 scope 曾经用远高于此的候选判定上限来填 `max_file_bytes`，
/// 于是出现「列进了 confirmed_files，实际既不备份也不保护」的静默缺口。
/// 现在两侧共用本常量，超限候选会被明确剔除并告知用户。
pub const DEFAULT_MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

fn default_max_file_bytes() -> Option<u64> {
    Some(DEFAULT_MAX_FILE_BYTES)
}

fn default_keep_versions() -> usize {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveProfile {
    pub profile_id: String,
    pub game_uid: String,
    pub executable_hash: String,
    pub scopes: Vec<SaveScope>,
    pub detection_evidence: Vec<String>,
    pub confidence: u8,
    pub enabled: bool,
    #[serde(default = "default_keep_versions")]
    pub keep_versions: usize,
    pub created_at: String,
    pub updated_at: String,
}

impl SaveProfile {
    pub fn new(
        game_uid: String,
        executable_hash: String,
        scopes: Vec<SaveScope>,
        confidence: u8,
        now: String,
    ) -> Self {
        Self {
            profile_id: Uuid::new_v4().to_string(),
            game_uid,
            executable_hash,
            scopes,
            // 溯源信息由调用方按实际采集方式补上（`with_detection_evidence`）。
            // 手动创建的 profile 没有识别过程，留空才是真话。
            detection_evidence: Vec::new(),
            confidence,
            enabled: true,
            keep_versions: 5,
            created_at: now.clone(),
            updated_at: now,
        }
    }

    /// 记录这份 profile 的范围是**怎么得出来的**。
    ///
    /// 这里原本硬编码 `["snapshot_diff", "folder_grouping"]`，走 ETW 时也原样不动 ——
    /// 等于把一份会随云端档案一起带出去的溯源信息写成了假话。
    pub fn with_detection_evidence(mut self, detection_evidence: Vec<String>) -> Self {
        self.detection_evidence = detection_evidence;
        self
    }
}

/// 识别证据的词汇表。
///
/// 刻意收敛在 Rust 单点：前端只负责说清「这次是 ETW 还是快照差异」，具体措辞由这里
/// 决定，免得同一份溯源信息在前后端各写一套字面量、日后各改各的。
pub fn detection_evidence_for(capture_mode: Option<&str>) -> Vec<String> {
    let mut evidence = vec![match capture_mode {
        Some("etw") => "etw_write",
        _ => "snapshot_diff",
    }
    .to_string()];
    evidence.push("folder_grouping".to_string());
    evidence
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_save_scope_contains_default_exclusions() {
        let scope = SaveScope::new_manual("D:\\GameSaves".to_string());
        assert!(scope.exclude_patterns.contains(&"*.tmp".to_string()));
        assert!(scope.exclude_patterns.contains(&"*.log".to_string()));
        assert!(scope.exclude_patterns.contains(&"*.bak".to_string()));
        assert!(scope.exclude_directories.contains(&"logs".to_string()));
        assert!(scope
            .exclude_directories
            .contains(&"crashdumps".to_string()));
        assert!(scope.exclude_directories.contains(&"cache".to_string()));
        assert!(scope
            .exclude_directories
            .contains(&"shader_cache".to_string()));
        assert!(scope.exclude_directories.contains(&"vulkan".to_string()));
    }

    #[test]
    fn ensure_default_exclusions_if_empty_preserves_custom_rules_or_fills_defaults() {
        let mut empty_scope = SaveScope {
            root_type: SaveRootType::Custom,
            root_path: "D:\\GameSaves".to_string(),
            confirmed_files: vec![],
            include_directories: vec![".".to_string()],
            exclude_exact: vec![],
            exclude_patterns: vec![],
            exclude_directories: vec![],
            unknown_file_policy: UnknownFilePolicy::Protect,
            max_file_bytes: Some(10 * 1024 * 1024),
        };
        empty_scope.ensure_default_exclusions_if_empty();
        assert!(!empty_scope.exclude_patterns.is_empty());
        assert!(!empty_scope.exclude_directories.is_empty());

        let mut custom_scope = SaveScope {
            root_type: SaveRootType::Custom,
            root_path: "D:\\GameSaves".to_string(),
            confirmed_files: vec![],
            include_directories: vec![".".to_string()],
            exclude_exact: vec![],
            exclude_patterns: vec!["*.custom".to_string()],
            exclude_directories: vec!["custom_dir".to_string()],
            unknown_file_policy: UnknownFilePolicy::Protect,
            max_file_bytes: Some(10 * 1024 * 1024),
        };
        custom_scope.ensure_default_exclusions_if_empty();
        assert_eq!(custom_scope.exclude_patterns, vec!["*.custom".to_string()]);
        assert_eq!(
            custom_scope.exclude_directories,
            vec!["custom_dir".to_string()]
        );
    }

    #[test]
    fn detection_evidence_reflects_the_actual_capture_mode() {
        assert_eq!(
            detection_evidence_for(Some("etw")),
            vec!["etw_write".to_string(), "folder_grouping".to_string()]
        );
        assert_eq!(
            detection_evidence_for(Some("snapshot")),
            vec!["snapshot_diff".to_string(), "folder_grouping".to_string()]
        );
        // 采集方式缺失或无法识别时按快照差异处理，绝不谎报 ETW。
        assert_eq!(
            detection_evidence_for(None),
            vec!["snapshot_diff".to_string(), "folder_grouping".to_string()]
        );
    }

    /// 新建的 profile 不自带溯源信息：手动创建的根本没有识别过程，
    /// 旧实现无条件写死 `snapshot_diff` 是在说假话。
    #[test]
    fn new_profile_has_no_detection_evidence_until_it_is_set() {
        let profile =
            SaveProfile::new("g".to_string(), "h".to_string(), vec![], 0, "0".to_string());
        assert!(profile.detection_evidence.is_empty());
        let profile = profile.with_detection_evidence(vec!["etw_write".to_string()]);
        assert_eq!(profile.detection_evidence, vec!["etw_write".to_string()]);
    }
}
