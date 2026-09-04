use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
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
    "*.tmp",
    "*.temp",
    "*.log",
    "*.dmp",
    "*.bak",
    "*.etl",
    "*.csv",
    "*.cache",
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
            exclude_patterns: DEFAULT_EXCLUDE_PATTERNS.iter().map(|s| s.to_string()).collect(),
            exclude_directories: DEFAULT_EXCLUDE_DIRECTORIES.iter().map(|s| s.to_string()).collect(),
            unknown_file_policy: UnknownFilePolicy::Protect,
            max_file_bytes: default_max_file_bytes(),
        }
    }

    pub fn ensure_default_exclusions_if_empty(&mut self) {
        if self.exclude_patterns.is_empty() {
            self.exclude_patterns = DEFAULT_EXCLUDE_PATTERNS.iter().map(|s| s.to_string()).collect();
        }
        if self.exclude_directories.is_empty() {
            self.exclude_directories = DEFAULT_EXCLUDE_DIRECTORIES.iter().map(|s| s.to_string()).collect();
        }
    }
}

fn default_max_file_bytes() -> Option<u64> {
    Some(10 * 1024 * 1024)
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
    pub fn new(game_uid: String, executable_hash: String, scopes: Vec<SaveScope>, confidence: u8, now: String) -> Self {
        Self {
            profile_id: Uuid::new_v4().to_string(),
            game_uid,
            executable_hash,
            scopes,
            detection_evidence: vec!["snapshot_diff".to_string(), "folder_grouping".to_string()],
            confidence,
            enabled: true,
            keep_versions: 5,
            created_at: now.clone(),
            updated_at: now,
        }
    }
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
        assert!(scope.exclude_directories.contains(&"crashdumps".to_string()));
        assert!(scope.exclude_directories.contains(&"cache".to_string()));
        assert!(scope.exclude_directories.contains(&"shader_cache".to_string()));
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
        assert_eq!(custom_scope.exclude_directories, vec!["custom_dir".to_string()]);
    }
}
