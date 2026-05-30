use crate::shared::{ExecutionConfig, GameSaveRule, PersistedStore};
use serde_json;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};
use tauri::{AppHandle, Manager};
use uuid::Uuid;

const MAX_BACKUP_KEEP_VERSIONS: usize = 10;

pub(crate) trait StoreRepository {
    fn load(&self, app: &AppHandle) -> Result<PersistedStore, String>;
    fn persist(&self, app: &AppHandle, store: &PersistedStore) -> Result<(), String>;
    fn normalize(&self, store: &mut PersistedStore);
}

pub(crate) struct JsonStoreRepository;

impl JsonStoreRepository {
    pub(crate) fn new() -> Self {
        Self
    }

    fn store_file_path(&self, app: &AppHandle) -> Result<PathBuf, String> {
        let base = app
            .path()
            .app_data_dir()
            .map_err(|err| format!("resolve app_data_dir failed: {err}"))?;
        fs::create_dir_all(&base).map_err(|err| format!("create app_data_dir failed: {err}"))?;
        Ok(base.join("store.json"))
    }

    fn store_backup_file_path(&self, app: &AppHandle) -> Result<PathBuf, String> {
        Ok(self.store_file_path(app)?.with_extension("json.bak"))
    }
}

impl StoreRepository for JsonStoreRepository {
    fn load(&self, app: &AppHandle) -> Result<PersistedStore, String> {
        let path = self.store_file_path(app)?;
        if !path.exists() {
            return Ok(PersistedStore::default());
        }

        let raw = fs::read(&path).map_err(|err| format!("read store failed: {err}"))?;
        let content = decode_text_bytes(&raw);

        match serde_json::from_str::<PersistedStore>(&content) {
            Ok(mut store) => {
                self.normalize(&mut store);
                Ok(store)
            }
            Err(primary_err) => {
                let backup = self.store_backup_file_path(app)?;
                if backup.exists() {
                    let backup_raw =
                        fs::read(&backup).map_err(|err| format!("read store backup failed: {err}"))?;
                    let backup_content = decode_text_bytes(&backup_raw);
                    if let Ok(mut store) = serde_json::from_str::<PersistedStore>(&backup_content) {
                        self.normalize(&mut store);
                        return Ok(store);
                    }
                }
                Err(format!("parse store failed: {primary_err}"))
            }
        }
    }

    fn persist(&self, app: &AppHandle, store: &PersistedStore) -> Result<(), String> {
        let content =
            serde_json::to_string_pretty(store).map_err(|err| format!("serialize store failed: {err}"))?;
        let path = self.store_file_path(app)?;
        let backup = self.store_backup_file_path(app)?;

        if path.exists() {
            let _ = fs::copy(&path, &backup);
        }

        let temp_path = path.with_extension("json.tmp");
        fs::write(&temp_path, content).map_err(|err| format!("write temp store failed: {err}"))?;
        if path.exists() {
            fs::remove_file(&path).map_err(|err| format!("remove old store failed: {err}"))?;
        }
        fs::rename(&temp_path, &path).map_err(|err| format!("replace store failed: {err}"))
    }

    fn normalize(&self, store: &mut PersistedStore) {
        let current = store.execution_config.managed_save_root.trim().to_string();
        if current.is_empty() || current.eq_ignore_ascii_case(&legacy_managed_save_root()) {
            store.execution_config.managed_save_root = default_managed_save_root();
        }
        if store.execution_config.backup_root.trim().is_empty() {
            store.execution_config.backup_root = default_backup_root();
        }

        let mut normalized_keep_versions = HashMap::new();
        for (uid_key, keep_versions) in store.execution_config.backup_keep_versions_by_uid.clone() {
            let normalized_uid = normalize_game_uid(&uid_key);
            if normalized_uid.is_empty() || keep_versions == 0 {
                continue;
            }
            normalized_keep_versions.insert(normalized_uid, normalize_backup_keep_versions(keep_versions));
        }
        store.execution_config.backup_keep_versions_by_uid = normalized_keep_versions;

        let mut valid_uids_by_game: HashMap<String, HashSet<String>> = HashMap::new();
        for rule in &mut store.rules {
            let normalized_uid = normalize_game_uid(&rule.game_uid);
            if normalized_uid.is_empty() {
                rule.game_uid = new_game_uid();
            } else {
                rule.game_uid = normalized_uid;
            }
            if rule.updated_at.trim().is_empty() {
                rule.updated_at = rule.created_at.clone();
            }
            let game_key = normalize_game_key(&rule.game_id);
            if !game_key.is_empty() {
                valid_uids_by_game
                    .entry(game_key)
                    .or_default()
                    .insert(rule.game_uid.clone());
            }
        }

        let mut normalized_preferred_exe_by_uid = HashMap::new();
        for (uid_key, exe_path) in store.execution_config.preferred_exe_by_uid.clone() {
            let normalized_uid = normalize_game_uid(&uid_key);
            let trimmed_exe = exe_path.trim();
            if normalized_uid.is_empty() || trimmed_exe.is_empty() {
                continue;
            }
            normalized_preferred_exe_by_uid.insert(normalized_uid, trimmed_exe.to_string());
        }
        store.execution_config.preferred_exe_by_uid = normalized_preferred_exe_by_uid;
        store.execution_config.preferred_exe_by_game_legacy = HashMap::new();

        store.execution_config.preferred_rule_uid_by_game.retain(|game_key, uid| {
            let normalized_game_key = normalize_game_key(game_key);
            let normalized_uid = normalize_game_uid(uid);
            !normalized_game_key.is_empty()
                && !normalized_uid.is_empty()
                && valid_uids_by_game
                    .get(&normalized_game_key)
                    .is_some_and(|uids| uids.contains(&normalized_uid))
        });

        store.execution_config.preferred_rule_id_by_exe_hash.retain(|exe_hash, rule_id| {
            !normalize_exe_hash(exe_hash).is_empty() && !rule_id.trim().is_empty()
        });

        store.rules.retain(|rule| {
            !rule.game_id.trim().is_empty()
                && !rule.game_uid.trim().is_empty()
                && !rule.exe_hash.trim().is_empty()
                && !rule.confirmed_paths.is_empty()
        });
    }
}

pub(crate) fn default_managed_save_root() -> String {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|v| v.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    exe_dir.join("GameSaverSaves").to_string_lossy().to_string()
}

pub(crate) fn legacy_managed_save_root() -> String {
    let profile = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".to_string());
    Path::new(&profile)
        .join("Saved Games")
        .join("GameSaver")
        .to_string_lossy()
        .to_string()
}

pub(crate) fn default_backup_root() -> String {
    PathBuf::from(r"D:\GameSaverData\Backups")
        .to_string_lossy()
        .to_string()
}

pub(crate) fn normalize_game_key(game_id: &str) -> String {
    game_id.trim().to_ascii_lowercase()
}

pub(crate) fn normalize_game_uid(game_uid: &str) -> String {
    game_uid.trim().to_ascii_lowercase()
}

pub(crate) fn normalize_exe_hash(exe_hash: &str) -> String {
    exe_hash.trim().to_ascii_lowercase()
}

pub(crate) fn normalize_backup_keep_versions(keep: usize) -> usize {
    keep.clamp(1, MAX_BACKUP_KEEP_VERSIONS)
}

pub(crate) fn new_game_uid() -> String {
    Uuid::new_v4().to_string()
}

pub(crate) fn rule_updated_ts(rule: &GameSaveRule) -> u64 {
    let updated = rule.updated_at.trim();
    if !updated.is_empty() {
        return updated.parse::<u64>().ok().unwrap_or(0);
    }
    rule.created_at.trim().parse::<u64>().ok().unwrap_or(0)
}

pub(crate) fn select_rule_for_game(store: &PersistedStore, game_id: &str) -> Option<GameSaveRule> {
    let game_key = normalize_game_key(game_id);
    if game_key.is_empty() {
        return None;
    }

    if let Some(target_uid) = store
        .execution_config
        .preferred_rule_uid_by_game
        .get(&game_key)
        .map(|uid| normalize_game_uid(uid))
        .filter(|uid| !uid.is_empty())
    {
        let preferred = store
            .rules
            .iter()
            .filter(|rule| {
                normalize_game_key(&rule.game_id) == game_key
                    && normalize_game_uid(&rule.game_uid) == target_uid
            })
            .max_by_key(|rule| (rule.enabled as u8, rule_updated_ts(rule)))
            .cloned();
        if preferred.is_some() {
            return preferred;
        }
    }

    store
        .rules
        .iter()
        .filter(|rule| normalize_game_key(&rule.game_id) == game_key)
        .max_by_key(|rule| (rule.enabled as u8, rule_updated_ts(rule)))
        .cloned()
}

pub(crate) fn resolve_preferred_rule_id_for_exe_hash(
    execution_config: &ExecutionConfig,
    exe_hash: &str,
) -> Option<String> {
    let normalized_hash = normalize_exe_hash(exe_hash);
    if normalized_hash.is_empty() {
        return None;
    }
    execution_config
        .preferred_rule_id_by_exe_hash
        .get(&normalized_hash)
        .cloned()
        .filter(|rule_id| !rule_id.trim().is_empty())
}

pub(crate) fn match_enabled_rule_for_exe_hash(
    rules: &[GameSaveRule],
    execution_config: &ExecutionConfig,
    exe_hash: &str,
    expected_game_key: Option<&str>,
) -> (Option<GameSaveRule>, bool) {
    let normalized_hash = normalize_exe_hash(exe_hash);
    if normalized_hash.is_empty() {
        return (None, false);
    }

    let preferred_rule_id_for_hash =
        resolve_preferred_rule_id_for_exe_hash(execution_config, &normalized_hash);
    let mut hash_matched_any = false;
    let mut candidates = Vec::new();
    for rule in rules {
        if !rule.enabled || normalize_exe_hash(&rule.exe_hash) != normalized_hash {
            continue;
        }
        hash_matched_any = true;
        let game_matches = if let Some(game_key) = expected_game_key {
            normalize_game_key(&rule.game_id) == game_key
        } else {
            true
        };
        if game_matches {
            candidates.push(rule.clone());
        }
    }

    if let Some(preferred_rule_id) = preferred_rule_id_for_hash {
        if let Some(preferred) = candidates
            .iter()
            .find(|rule| rule.rule_id == preferred_rule_id)
            .cloned()
        {
            return (Some(preferred), hash_matched_any);
        }
    }

    (
        candidates
            .into_iter()
            .max_by_key(|rule| (rule.enabled as u8, rule_updated_ts(rule))),
        hash_matched_any,
    )
}

pub(crate) fn has_unresolved_primary_rule_conflict_for_exe_hash(
    rules: &[GameSaveRule],
    execution_config: &ExecutionConfig,
    exe_hash: &str,
    expected_game_key: Option<&str>,
) -> bool {
    let normalized_hash = normalize_exe_hash(exe_hash);
    if normalized_hash.is_empty() {
        return false;
    }

    let mut candidate_rule_ids = Vec::new();
    for rule in rules {
        if !rule.enabled || normalize_exe_hash(&rule.exe_hash) != normalized_hash {
            continue;
        }
        let game_matches = if let Some(game_key) = expected_game_key {
            normalize_game_key(&rule.game_id) == game_key
        } else {
            true
        };
        if game_matches {
            candidate_rule_ids.push(rule.rule_id.clone());
        }
    }

    if candidate_rule_ids.len() <= 1 {
        return false;
    }

    let Some(preferred_rule_id) =
        resolve_preferred_rule_id_for_exe_hash(execution_config, &normalized_hash)
    else {
        return true;
    };

    !candidate_rule_ids.iter().any(|rule_id| rule_id == &preferred_rule_id)
}

pub(crate) fn decode_text_bytes(raw: &[u8]) -> String {
    if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(&raw[3..]).to_string();
    }
    if raw.starts_with(&[0xFF, 0xFE]) {
        let mut units = Vec::new();
        let mut i = 2;
        while i + 1 < raw.len() {
            units.push(u16::from_le_bytes([raw[i], raw[i + 1]]));
            i += 2;
        }
        return String::from_utf16_lossy(&units);
    }
    if raw.starts_with(&[0xFE, 0xFF]) {
        let mut units = Vec::new();
        let mut i = 2;
        while i + 1 < raw.len() {
            units.push(u16::from_be_bytes([raw[i], raw[i + 1]]));
            i += 2;
        }
        return String::from_utf16_lossy(&units);
    }

    if let Ok(text) = String::from_utf8(raw.to_vec()) {
        return text;
    }

    if raw.len() >= 2 {
        let mut units = Vec::new();
        let mut i = 0;
        while i + 1 < raw.len() {
            units.push(u16::from_le_bytes([raw[i], raw[i + 1]]));
            i += 2;
        }
        let utf16 = String::from_utf16_lossy(&units);
        if utf16.chars().any(|ch| ch == ',' || ch == '\n') {
            return utf16;
        }
    }

    String::from_utf8_lossy(raw).to_string()
}
