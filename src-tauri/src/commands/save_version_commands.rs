use crate::{
    app_state::AppState,
    domain::{GameLifecycle, SaveProfile, SaveVersion, TaskStatus},
    repositories::{GameRepository, SaveRepository},
    services::{GameLibraryService, TaskService},
};
use tauri::{AppHandle, Manager, State};

#[tauri::command]
pub fn restore_save_version(app: AppHandle, state: State<AppState>, game_uid: String, version_id: String) -> Result<String, String> {
    let game_uid = game_uid.trim().to_string();
    let version_id = version_id.trim().to_string();
    let (game, profile, version, latest) = load_version_context(&state, &game_uid, &version_id)?;
    reserve_maintenance(&state, &game_uid)?;
    let task_id = match TaskService::create(&state, "restore_save_version", Some(game_uid.clone()), "准备恢复保存版本") {
        Ok(task_id) => task_id,
        Err(error) => {
            release_maintenance(&state, &game_uid);
            return Err(error);
        }
    };
    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        let result = restore_version_task(&app_handle, &task_id_for_thread, &game, &profile, &version, latest.as_ref());
        let state = app_handle.state::<AppState>();
        release_maintenance(&state, &game_uid);
        match result {
            Ok(summary) => TaskService::finish(&state, &task_id_for_thread, TaskStatus::Success, 100, "保存版本恢复完成", Some(summary), None),
            Err(error) => TaskService::finish(&state, &task_id_for_thread, TaskStatus::Failed, 100, "保存版本恢复失败", None, Some(error)),
        }
    });
    Ok(task_id)
}

#[tauri::command]
pub fn delete_save_version(app: AppHandle, state: State<AppState>, game_uid: String, version_id: String) -> Result<String, String> {
    let game_uid = game_uid.trim().to_string();
    let version_id = version_id.trim().to_string();
    ensure_version_exists(&state, &game_uid, &version_id)?;
    reserve_maintenance(&state, &game_uid)?;
    let task_id = match TaskService::create(&state, "delete_save_version", Some(game_uid.clone()), "准备删除保存版本") {
        Ok(task_id) => task_id,
        Err(error) => {
            release_maintenance(&state, &game_uid);
            return Err(error);
        }
    };
    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        let result = delete_versions_task(&app_handle, &task_id_for_thread, &game_uid, std::slice::from_ref(&version_id));
        let state = app_handle.state::<AppState>();
        release_maintenance(&state, &game_uid);
        match result {
            Ok(summary) => {
                let message = if summary.get("garbageCollectionError").is_some() { "保存版本已删除，但对象回收未完成" } else { "保存版本已删除" };
                TaskService::finish(&state, &task_id_for_thread, TaskStatus::Success, 100, message, Some(summary), None)
            }
            Err(error) => TaskService::finish(&state, &task_id_for_thread, TaskStatus::Failed, 100, "删除保存版本失败", None, Some(error)),
        }
    });
    Ok(task_id)
}

#[tauri::command]
pub fn prune_save_versions(app: AppHandle, state: State<AppState>, game_uid: String, keep_versions: usize) -> Result<String, String> {
    let game_uid = game_uid.trim().to_string();
    if keep_versions == 0 {
        return Err("至少保留一个保存版本".to_string());
    }
    let version_ids = versions_to_remove(&state, &game_uid, keep_versions)?;
    if version_ids.is_empty() {
        return Err("没有需要清理的保存版本".to_string());
    }
    reserve_maintenance(&state, &game_uid)?;
    let task_id = match TaskService::create(&state, "prune_save_versions", Some(game_uid.clone()), "准备清理旧保存版本") {
        Ok(task_id) => task_id,
        Err(error) => {
            release_maintenance(&state, &game_uid);
            return Err(error);
        }
    };
    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        let result = delete_versions_task(&app_handle, &task_id_for_thread, &game_uid, &version_ids);
        let state = app_handle.state::<AppState>();
        release_maintenance(&state, &game_uid);
        match result {
            Ok(summary) => {
                let message = if summary.get("garbageCollectionError").is_some() { "旧保存版本已清理，但对象回收未完成" } else { "旧保存版本已清理" };
                TaskService::finish(&state, &task_id_for_thread, TaskStatus::Success, 100, message, Some(summary), None)
            }
            Err(error) => TaskService::finish(&state, &task_id_for_thread, TaskStatus::Failed, 100, "清理旧保存版本失败", None, Some(error)),
        }
    });
    Ok(task_id)
}

fn restore_version_task(
    app: &AppHandle,
    task_id: &str,
    game: &crate::domain::Game,
    profile: &SaveProfile,
    version: &SaveVersion,
    latest: Option<&SaveVersion>,
) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    ensure_maintenance_allowed(&state, &game.game_uid)?;
    TaskService::update(&state, task_id, TaskStatus::Running, 5, "正在保护当前存档", None);
    let protected = SaveRepository::commit(app, game, profile, latest, |progress, message| {
        TaskService::update(&state, task_id, TaskStatus::Running, 5 + progress / 4, message, None);
    })?;
    let pending_protected = protected.clone();
    if let Some(protected) = protected {
        let mut candidate = match state.store.lock() {
            Ok(store) => store.clone(),
            Err(_) => {
                if let Some(version) = pending_protected.as_ref() { crate::repositories::release_pending_objects(version); }
                return Err("读取 GameSaver 存储失败".to_string());
            }
        };
        let protected_id = protected.version_id.clone();
        candidate.save_versions.push(protected);

        let keep_versions = profile.keep_versions;
        if keep_versions > 0 {
            let mut game_versions: Vec<_> = candidate
                .save_versions
                .iter()
                .filter(|v| v.game_uid == game.game_uid)
                .cloned()
                .collect();
            game_versions.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.version_id.cmp(&a.version_id)));
            if game_versions.len() > keep_versions {
                let to_remove: std::collections::HashSet<String> = game_versions
                    .into_iter()
                    .skip(keep_versions)
                    .map(|v| v.version_id)
                    .collect();
                candidate.save_versions.retain(|v| !(v.game_uid == game.game_uid && to_remove.contains(&v.version_id)));
                let _ = SaveRepository::collect_garbage(app, &candidate.save_versions);
            }
        }

        let Some(game_record) = candidate.games.iter_mut().find(|item| item.game_uid == game.game_uid) else {
            if let Some(version) = pending_protected.as_ref() { crate::repositories::release_pending_objects(version); }
            return Err("游戏记录不存在".to_string());
        };
        game_record.latest_save_version_id = Some(protected_id);
        if let Err(error) = GameRepository::persist(app, &candidate) {
            if let Some(version) = pending_protected.as_ref() { crate::repositories::release_pending_objects(version); }
            return Err(error);
        }
        *state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())? = candidate;
    }
    if let Some(version) = pending_protected.as_ref() { crate::repositories::release_pending_objects(version); }
    TaskService::update(&state, task_id, TaskStatus::Running, 35, "当前存档已保护，正在校验目标版本", None);
    let receipt = SaveRepository::restore(app, game, profile, version, |progress, message| {
        TaskService::update(&state, task_id, TaskStatus::Running, 35 + progress * 3 / 5, message, None);
    })?;
    let mut candidate = state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())?.clone();
    let Some(game_record) = candidate.games.iter_mut().find(|item| item.game_uid == game.game_uid) else {
        let rollback = SaveRepository::rollback_restore(receipt);
        return Err(match rollback {
            Ok(()) => "游戏记录不存在，已回滚存档恢复".to_string(),
            Err(error) => format!("游戏记录不存在，且存档回滚失败：{error}"),
        });
    };
    game_record.latest_save_version_id = Some(version.version_id.clone());
    if let Err(error) = GameRepository::persist(app, &candidate) {
        return match SaveRepository::rollback_restore(receipt) {
            Ok(()) => Err(format!("保存恢复结果失败，已回滚存档：{error}")),
            Err(rollback_error) => Err(format!("保存恢复结果失败，且存档回滚失败：{error}；{rollback_error}")),
        };
    }
    SaveRepository::finalize_restore(receipt);
    *state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())? = candidate;
    Ok(serde_json::json!({ "versionId": version.version_id, "createdAt": version.created_at }))
}

fn delete_versions_task(app: &AppHandle, task_id: &str, game_uid: &str, version_ids: &[String]) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    ensure_maintenance_allowed(&state, game_uid)?;
    TaskService::update(&state, task_id, TaskStatus::Running, 25, "正在更新保存版本清单", None);
    let mut candidate = state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())?.clone();
    let before = candidate.save_versions.len();
    candidate.save_versions.retain(|version| !(version.game_uid == game_uid && version_ids.iter().any(|id| id == &version.version_id)));
    if candidate.save_versions.len() == before {
        return Err("保存版本不存在".to_string());
    }
    let latest = candidate
        .save_versions
        .iter()
        .filter(|version| version.game_uid == game_uid)
        .max_by(|left, right| left.created_at.cmp(&right.created_at).then(left.version_id.cmp(&right.version_id)))
        .map(|version| version.version_id.clone());
    let game = candidate.games.iter_mut().find(|game| game.game_uid == game_uid).ok_or_else(|| "游戏不存在".to_string())?;
    game.latest_save_version_id = latest;
    GameRepository::persist(app, &candidate)?;
    *state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())? = candidate.clone();
    TaskService::update(&state, task_id, TaskStatus::Running, 70, "正在回收不再引用的存档对象", None);
    let mut summary = serde_json::json!({ "removedVersions": version_ids.len(), "removedObjects": 0 });
    match SaveRepository::collect_garbage(app, &candidate.save_versions) {
        Ok(removed_objects) => summary["removedObjects"] = serde_json::json!(removed_objects),
        Err(error) => summary["garbageCollectionError"] = serde_json::json!(error),
    }
    Ok(summary)
}

fn load_version_context(state: &AppState, game_uid: &str, version_id: &str) -> Result<(crate::domain::Game, SaveProfile, SaveVersion, Option<SaveVersion>), String> {
    let store = state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())?;
    let game = GameLibraryService::find(&store, game_uid).ok_or_else(|| "游戏不存在".to_string())?;
    if !matches!(game.lifecycle, GameLifecycle::Active) { return Err("游戏尚未完成设置".to_string()); }
    let profile = store.save_profiles.iter().find(|profile| game.save_profile_id.as_deref() == Some(profile.profile_id.as_str()) && profile.game_uid == game_uid && profile.enabled).cloned().ok_or_else(|| "存档保护配置不存在".to_string())?;
    let version = store.save_versions.iter().find(|version| version.version_id == version_id && version.game_uid == game_uid).cloned().ok_or_else(|| "保存版本不存在".to_string())?;
    let latest = game.latest_save_version_id.as_ref().and_then(|id| store.save_versions.iter().find(|version| &version.version_id == id)).cloned();
    Ok((game, profile, version, latest))
}

fn ensure_version_exists(state: &AppState, game_uid: &str, version_id: &str) -> Result<(), String> {
    let store = state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())?;
    if !store.games.iter().any(|game| game.game_uid == game_uid) { return Err("游戏不存在".to_string()); }
    if !store.save_versions.iter().any(|version| version.game_uid == game_uid && version.version_id == version_id) { return Err("保存版本不存在".to_string()); }
    Ok(())
}

fn versions_to_remove(state: &AppState, game_uid: &str, keep_versions: usize) -> Result<Vec<String>, String> {
    let store = state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())?;
    if !store.games.iter().any(|game| game.game_uid == game_uid) { return Err("游戏不存在".to_string()); }
    let mut versions = store.save_versions.iter().filter(|version| version.game_uid == game_uid).collect::<Vec<_>>();
    versions.sort_by(|left, right| right.created_at.cmp(&left.created_at).then(right.version_id.cmp(&left.version_id)));
    Ok(versions.into_iter().skip(keep_versions).map(|version| version.version_id.clone()).collect())
}

fn ensure_maintenance_allowed(state: &AppState, game_uid: &str) -> Result<(), String> {
    if state.running_games.lock().map_err(|_| "lock running game state failed".to_string())?.contains_key(game_uid) {
        return Err("游戏运行中，暂时不能操作保存版本".to_string());
    }
    Ok(())
}

fn reserve_maintenance(state: &AppState, game_uid: &str) -> Result<(), String> {
    let mut operations = state.save_operations.lock().map_err(|_| "lock save operation state failed".to_string())?;
    if operations.contains(game_uid) {
        return Err("该游戏已有存档版本操作正在进行".to_string());
    }
    if state.running_games.lock().map_err(|_| "lock running game state failed".to_string())?.contains_key(game_uid) {
        return Err("游戏运行中，暂时不能操作保存版本".to_string());
    }
    operations.insert(game_uid.to_string());
    Ok(())
}

fn release_maintenance(state: &AppState, game_uid: &str) {
    if let Ok(mut operations) = state.save_operations.lock() {
        operations.remove(game_uid);
    }
}
