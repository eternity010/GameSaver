use crate::{
    app_state::AppState,
    path_utils::normalize_paths,
    runtime::now_iso_string,
    shared::{ExportRulesResult, GameSaveRule, ImportRuleInput, ImportRulesResult, RuleConflictItem},
    storage::{
        new_game_uid, normalize_exe_hash, normalize_game_key, normalize_game_uid,
        JsonStoreRepository, StoreRepository,
    },
};
use std::{collections::HashMap, fs};
use tauri::{AppHandle, State};
use uuid::Uuid;

fn persist_rules(app: &AppHandle, store: &crate::shared::PersistedStore) -> Result<(), String> {
    JsonStoreRepository::new().persist(app, store)
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
    let store = state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    let content = serde_json::to_string_pretty(&store.rules)
        .map_err(|err| format!("serialize rules failed: {err}"))?;
    fs::write(file_path, content).map_err(|err| format!("write rules failed: {err}"))?;
    Ok(ExportRulesResult {
        count: store.rules.len(),
    })
}

#[tauri::command]
pub(crate) fn import_rules(
    app: AppHandle,
    state: State<AppState>,
    file_path: String,
) -> Result<ImportRulesResult, String> {
    if file_path.trim().is_empty() {
        return Err("filePath cannot be empty".to_string());
    }
    let content = fs::read_to_string(&file_path).map_err(|err| format!("read rules failed: {err}"))?;
    let inputs = serde_json::from_str::<Vec<ImportRuleInput>>(&content)
        .map_err(|err| format!("parse rules failed: {err}"))?;

    let mut store = state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    let mut imported = 0;
    let mut overwritten = 0;
    let mut skipped = 0;

    for input in inputs {
        let game_id = input.game_id.trim().to_string();
        let exe_hash = normalize_exe_hash(&input.exe_hash);
        let confirmed_paths = normalize_paths(input.confirmed_paths, None);
        if game_id.is_empty() || exe_hash.is_empty() || confirmed_paths.is_empty() {
            skipped += 1;
            continue;
        }

        let now = now_iso_string();
        let candidate = GameSaveRule {
            rule_id: input.rule_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
            game_id,
            game_uid: input.game_uid.unwrap_or_else(new_game_uid),
            exe_hash,
            confirmed_paths,
            created_at: input.created_at.unwrap_or_else(|| now.clone()),
            confidence: input.confidence.unwrap_or(45),
            enabled: input.enabled.unwrap_or(true),
            updated_at: input.updated_at.unwrap_or_else(|| now.clone()),
        };

        if let Some(existing) = store.rules.iter_mut().find(|rule| rule.rule_id == candidate.rule_id) {
            *existing = candidate;
            overwritten += 1;
        } else {
            store.rules.push(candidate);
            imported += 1;
        }
    }

    persist_rules(&app, &store)?;
    Ok(ImportRulesResult {
        imported,
        overwritten,
        skipped,
    })
}
