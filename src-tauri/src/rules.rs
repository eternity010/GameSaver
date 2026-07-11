use crate::{
    app_state::AppState,
    path_utils::normalize_paths,
    runtime::now_iso_string,
    shared::{ExportRulesResult, GameSaveRule, ImportRuleInput, ImportRulesResult, PreviewRulesResult, RuleConflictItem},
    storage::{
        decode_text_bytes, new_game_uid, normalize_exe_hash, normalize_game_key, normalize_game_uid,
        JsonStoreRepository, StoreRepository,
    },
};
use std::{collections::HashMap, fs, path::Path};
use tauri::{AppHandle, State};
use uuid::Uuid;
use sha2::{Digest, Sha256};

fn persist_rules(app: &AppHandle, store: &crate::shared::PersistedStore) -> Result<(), String> {
    JsonStoreRepository::new().persist(app, store)
}

const MAX_RULE_IMPORT_BYTES: u64 = 50 * 1024 * 1024;

fn read_rule_inputs(file_path: &str) -> Result<(Vec<ImportRuleInput>, String), String> {
    let metadata = fs::metadata(file_path).map_err(|err| format!("read rules metadata failed: {err}"))?;
    if metadata.len() > MAX_RULE_IMPORT_BYTES {
        return Err("rules file is too large".to_string());
    }
    let raw = fs::read(file_path).map_err(|err| format!("read rules failed: {err}"))?;
    let file_sha256 = hex::encode(Sha256::digest(&raw));
    let content = decode_text_bytes(&raw);
    let inputs = serde_json::from_str::<Vec<ImportRuleInput>>(&content)
        .map_err(|err| format!("parse rules failed: {err}"))?;
    Ok((inputs, file_sha256))
}

fn build_import_rule(input: ImportRuleInput) -> Option<GameSaveRule> {
    let game_id = input.game_id.trim().to_string();
    let exe_hash = normalize_exe_hash(&input.exe_hash);
    let confirmed_paths = normalize_paths(input.confirmed_paths, None);
    if game_id.is_empty() || exe_hash.is_empty() || confirmed_paths.is_empty() {
        return None;
    }
    let now = now_iso_string();
    Some(GameSaveRule {
        rule_id: input
            .rule_id
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| Uuid::new_v4().to_string()),
        game_id,
        game_uid: input
            .game_uid
            .map(|value| normalize_game_uid(&value))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(new_game_uid),
        exe_hash,
        confirmed_paths,
        created_at: input.created_at.unwrap_or_else(|| now.clone()),
        confidence: input.confidence.unwrap_or(45),
        enabled: input.enabled.unwrap_or(true),
        updated_at: input.updated_at.unwrap_or(now),
    })
}

fn find_import_match(rules: &[GameSaveRule], incoming: &GameSaveRule) -> Option<usize> {
    rules
        .iter()
        .position(|rule| rule.rule_id == incoming.rule_id)
        .or_else(|| {
            let game_key = normalize_game_key(&incoming.game_id);
            rules.iter().position(|rule| {
                normalize_game_key(&rule.game_id) == game_key
                    && normalize_exe_hash(&rule.exe_hash) == incoming.exe_hash
            })
        })
}

fn apply_rule_inputs(
    store: &mut crate::shared::PersistedStore,
    inputs: Vec<ImportRuleInput>,
) -> ImportRulesResult {
    let mut imported = 0usize;
    let mut overwritten = 0usize;
    let mut skipped = 0usize;
    for input in inputs {
        let Some(mut candidate) = build_import_rule(input) else {
            skipped += 1;
            continue;
        };
        if let Some(index) = find_import_match(&store.rules, &candidate) {
            candidate.rule_id = store.rules[index].rule_id.clone();
            candidate.game_uid = store.rules[index].game_uid.clone();
            store.rules[index] = candidate;
            overwritten += 1;
        } else {
            store.rules.push(candidate);
            imported += 1;
        }
    }
    ImportRulesResult { imported, overwritten, skipped }
}

fn replace_export_file(temp_path: &Path, target_path: &Path) -> Result<(), String> {
    let backup_path = target_path.with_extension(format!("bak-{}", Uuid::new_v4().simple()));
    let had_target = target_path.exists();
    if had_target {
        fs::rename(target_path, &backup_path).map_err(|err| format!("backup existing export failed: {err}"))?;
    }
    if let Err(err) = fs::rename(temp_path, target_path) {
        if had_target {
            let _ = fs::rename(&backup_path, target_path);
        }
        return Err(format!("replace rules export failed: {err}"));
    }
    if had_target {
        let _ = fs::remove_file(backup_path);
    }
    Ok(())
}

#[tauri::command]
pub(crate) fn list_rules(state: State<AppState>) -> Result<Vec<GameSaveRule>, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    let mut rules = store.rules.clone();
    rules.sort_by(|a, b| {
        normalize_game_key(&a.game_id)
            .cmp(&normalize_game_key(&b.game_id))
            .then_with(|| a.rule_id.cmp(&b.rule_id))
    });
    Ok(rules)
}

#[tauri::command]
pub(crate) fn list_rule_conflicts(state: State<AppState>) -> Result<Vec<RuleConflictItem>, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    let mut grouped: HashMap<String, Vec<&GameSaveRule>> = HashMap::new();
    for rule in &store.rules {
        let exe_hash = normalize_exe_hash(&rule.exe_hash);
        if exe_hash.is_empty() {
            continue;
        }
        grouped.entry(exe_hash).or_default().push(rule);
    }

    let mut conflicts = grouped
        .into_iter()
        .filter_map(|(exe_hash, rules)| {
            if rules.len() < 2 {
                return None;
            }
            let rule_ids = rules.iter().map(|rule| rule.rule_id.clone()).collect::<Vec<_>>();
            let game_ids = rules.iter().map(|rule| rule.game_id.clone()).collect::<Vec<_>>();
            let primary_rule_id = rules
                .iter()
                .find(|rule| rule.enabled)
                .map(|rule| rule.rule_id.clone())
                .or_else(|| rules.first().map(|rule| rule.rule_id.clone()));
            Some(RuleConflictItem {
                exe_hash,
                conflict_count: rule_ids.len(),
                rule_ids,
                game_ids,
                primary_rule_id,
            })
        })
        .collect::<Vec<_>>();
    conflicts.sort_by(|a, b| a.exe_hash.cmp(&b.exe_hash));
    Ok(conflicts)
}

#[tauri::command]
pub(crate) fn set_primary_rule(
    app: AppHandle,
    state: State<AppState>,
    rule_id: String,
) -> Result<GameSaveRule, String> {
    if rule_id.trim().is_empty() {
        return Err("ruleId cannot be empty".to_string());
    }

    let mut store = state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    let target_rule = store
        .rules
        .iter()
        .find(|rule| rule.rule_id == rule_id)
        .cloned()
        .ok_or_else(|| "ruleId not found".to_string())?;

    let normalized_hash = normalize_exe_hash(&target_rule.exe_hash);
    if normalized_hash.is_empty() {
        return Err("target rule has empty exeHash".to_string());
    }

    store
        .execution_config
        .preferred_rule_id_by_exe_hash
        .insert(normalized_hash, target_rule.rule_id.clone());

    let normalized_game_key = normalize_game_key(&target_rule.game_id);
    let normalized_game_uid = normalize_game_uid(&target_rule.game_uid);
    if !normalized_game_key.is_empty() && !normalized_game_uid.is_empty() {
        store
            .execution_config
            .preferred_rule_uid_by_game
            .insert(normalized_game_key, normalized_game_uid);
    }

    JsonStoreRepository::new().normalize(&mut store);
    persist_rules(&app, &store)?;
    store
        .rules
        .iter()
        .find(|rule| rule.rule_id == rule_id)
        .cloned()
        .ok_or_else(|| "ruleId not found".to_string())
}

#[tauri::command]
pub(crate) fn update_rule(
    app: AppHandle,
    state: State<AppState>,
    rule_id: String,
    game_id: String,
    confirmed_paths: Vec<String>,
    enabled: bool,
) -> Result<GameSaveRule, String> {
    if rule_id.trim().is_empty() {
        return Err("ruleId cannot be empty".to_string());
    }
    if game_id.trim().is_empty() {
        return Err("gameId cannot be empty".to_string());
    }
    let mut store = state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    let rule = store
        .rules
        .iter_mut()
        .find(|item| item.rule_id == rule_id)
        .ok_or_else(|| "ruleId not found".to_string())?;
    let normalized_paths = normalize_paths(confirmed_paths, None);
    if normalized_paths.is_empty() {
        return Err("confirmedPaths cannot be empty".to_string());
    }

    rule.game_id = game_id.trim().to_string();
    rule.confirmed_paths = normalized_paths;
    rule.enabled = enabled;
    if rule.game_uid.trim().is_empty() {
        rule.game_uid = new_game_uid();
    }
    rule.updated_at = now_iso_string();
    let updated = rule.clone();
    persist_rules(&app, &store)?;
    Ok(updated)
}

#[tauri::command]
pub(crate) fn delete_rule(app: AppHandle, state: State<AppState>, rule_id: String) -> Result<(), String> {
    if rule_id.trim().is_empty() {
        return Err("ruleId cannot be empty".to_string());
    }
    let mut store = state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    let before = store.rules.len();
    store.rules.retain(|rule| rule.rule_id != rule_id);
    if store.rules.len() == before {
        return Err("ruleId not found".to_string());
    }
    persist_rules(&app, &store)
}

#[tauri::command]
pub(crate) fn export_rules(state: State<AppState>, file_path: String) -> Result<ExportRulesResult, String> {
    if file_path.trim().is_empty() {
        return Err("filePath cannot be empty".to_string());
    }
    let rules = {
        let store = state
            .store
            .lock()
            .map_err(|_| "failed to lock app state".to_string())?;
        store.rules.clone()
    };
    let content = serde_json::to_string_pretty(&rules)
        .map_err(|err| format!("serialize rules failed: {err}"))?;
    let target_path = Path::new(file_path.trim());
    if let Some(parent) = target_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|err| format!("create export directory failed: {err}"))?;
        }
    }
    let temp_path = target_path.with_extension(format!("json.tmp-{}", Uuid::new_v4().simple()));
    if let Err(err) = fs::write(&temp_path, content) {
        let _ = fs::remove_file(&temp_path);
        return Err(format!("write rules failed: {err}"));
    }
    replace_export_file(&temp_path, target_path)?;
    Ok(ExportRulesResult {
        count: rules.len(),
    })
}

#[tauri::command]
pub(crate) fn preview_rules_import(
    state: State<AppState>,
    file_path: String,
) -> Result<PreviewRulesResult, String> {
    if file_path.trim().is_empty() {
        return Err("filePath cannot be empty".to_string());
    }
    let (inputs, file_sha256) = read_rule_inputs(file_path.trim())?;
    let store = state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    let mut preview_store = store.clone();
    let result = apply_rule_inputs(&mut preview_store, inputs);
    Ok(PreviewRulesResult {
        rule_count: result.imported + result.overwritten,
        imported: result.imported,
        overwritten: result.overwritten,
        skipped: result.skipped,
        file_sha256,
    })
}

#[tauri::command]
pub(crate) fn import_rules(
    app: AppHandle,
    state: State<AppState>,
    file_path: String,
    expected_file_sha256: Option<String>,
) -> Result<ImportRulesResult, String> {
    if file_path.trim().is_empty() {
        return Err("filePath cannot be empty".to_string());
    }
    let (inputs, actual_sha256) = read_rule_inputs(file_path.trim())?;
    if let Some(expected) = expected_file_sha256.filter(|value| !value.trim().is_empty()) {
        if !actual_sha256.eq_ignore_ascii_case(expected.trim()) {
            return Err("rules file changed after preview; preview it again before importing".to_string());
        }
    }

    let mut store = state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    let mut candidate_store = store.clone();
    let result = apply_rule_inputs(&mut candidate_store, inputs);
    JsonStoreRepository::new().normalize(&mut candidate_store);
    persist_rules(&app, &candidate_store)?;
    *store = candidate_store;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_rule_import_preserves_local_identity() {
        let mut store = crate::shared::PersistedStore::default();
        store.rules.push(GameSaveRule {
            rule_id: "local-rule".to_string(),
            game_id: "Example Game".to_string(),
            game_uid: "local-uid".to_string(),
            exe_hash: "abc".to_string(),
            confirmed_paths: vec!["old".to_string()],
            created_at: "1".to_string(),
            confidence: 10,
            enabled: true,
            updated_at: "1".to_string(),
        });
        let result = apply_rule_inputs(
            &mut store,
            vec![ImportRuleInput {
                rule_id: Some("foreign-rule".to_string()),
                game_id: "example game".to_string(),
                game_uid: Some("foreign-uid".to_string()),
                exe_hash: "ABC".to_string(),
                confirmed_paths: vec!["new".to_string()],
                created_at: None,
                updated_at: None,
                confidence: None,
                enabled: None,
            }],
        );
        assert_eq!(result.imported, 0);
        assert_eq!(result.overwritten, 1);
        assert_eq!(store.rules[0].rule_id, "local-rule");
        assert_eq!(store.rules[0].game_uid, "local-uid");
    }
}
