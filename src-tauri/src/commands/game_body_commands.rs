use crate::{
    app_state::AppState,
    domain::{GameBodyVersion, GameLifecycle, SaveProfile, SaveVersion, TaskStatus},
    repositories::{GameRepository, SaveRepository},
    services::{GameBodyUpdateService, GameLibraryService, TaskService},
};
use std::{path::{Path, PathBuf}};
use tauri::{AppHandle, Manager, State};
use uuid::Uuid;

#[tauri::command]
pub fn list_game_body_versions(state: State<AppState>, game_uid: String) -> Result<Vec<GameBodyVersion>, String> {
    let game_uid = game_uid.trim();
    let store = state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())?;
    if !store.games.iter().any(|game| game.game_uid == game_uid) { return Err("游戏不存在".to_string()); }
    let mut versions = store.body_versions.iter().filter(|version| version.game_uid == game_uid).cloned().collect::<Vec<_>>();
    versions.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    Ok(versions)
}

#[tauri::command]
pub fn update_game_body(app: AppHandle, state: State<AppState>, game_uid: String, source_path: String) -> Result<String, String> {
    let game_uid = game_uid.trim().to_string();
    let source_path = PathBuf::from(source_path.trim());
    let (game, profile, latest) = load_update_context(&state, &game_uid)?;
    let games_root = app.path().app_data_dir().map_err(|err| format!("解析 GameSaver 数据目录失败：{err}"))?.join("games");
    if source_path.exists() && paths_overlap(&source_path, &games_root) {
        return Err("新版游戏目录不能位于 GameSaver 受管游戏库内".to_string());
    }
    let plan = GameBodyUpdateService::validate_source(&source_path, &game)?;
    reserve_update(&state, &game_uid)?;
    let task_id = match TaskService::create(&state, "update_game_body", Some(game_uid.clone()), "准备更新游戏本体") {
        Ok(task_id) => task_id,
        Err(error) => {
            release_update(&state, &game_uid);
            return Err(error);
        }
    };
    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        let result = update_game_body_task(&app_handle, &task_id_for_thread, &game, &profile, latest.as_ref(), plan);
        release_update(&app_handle.state(), &game_uid);
        match result {
            Ok(summary) => TaskService::finish(&app_handle.state(), &task_id_for_thread, TaskStatus::Success, 100, "游戏本体更新完成", Some(summary), None),
            Err(error) if error == "任务已取消" => TaskService::finish(&app_handle.state(), &task_id_for_thread, TaskStatus::Cancelled, 100, "已取消游戏本体更新", None, None),
            Err(error) => TaskService::finish(&app_handle.state(), &task_id_for_thread, TaskStatus::Failed, 100, "游戏本体更新失败", None, Some(error)),
        }
    });
    Ok(task_id)
}

fn update_game_body_task(
    app: &AppHandle,
    task_id: &str,
    game: &crate::domain::Game,
    profile: &SaveProfile,
    latest: Option<&SaveVersion>,
    plan: crate::services::game_body_update_service::UpdatePlan,
) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    let games_root = Path::new(&game.managed_path).parent().ok_or_else(|| "解析游戏库目录失败".to_string())?.to_path_buf();
    let staging = GameBodyUpdateService::copy_to_staging(&plan, &games_root, &game.game_uid, |progress, message| {
        TaskService::update(&state, task_id, TaskStatus::Running, progress, message, None);
    }, || TaskService::is_cancelled(&state, task_id))?;
    if TaskService::is_cancelled(&state, task_id) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err("任务已取消".to_string());
    }
    TaskService::update(&state, task_id, TaskStatus::Running, 92, "正在保护并恢复当前存档", None);
    let protected = match SaveRepository::commit(app, game, profile, latest, |progress, message| {
        TaskService::update(&state, task_id, TaskStatus::Running, 92 + progress / 20, message, None);
    }) {
        Ok(protected) => protected,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    let pending_protected = protected.clone();
    if TaskService::is_cancelled(&state, task_id) {
        if let Some(version) = pending_protected.as_ref() { crate::repositories::release_pending_objects(version); }
        let _ = std::fs::remove_dir_all(&staging);
        return Err("任务已取消".to_string());
    }
    let save_target = protected.as_ref().or(latest);
    let version_id = Uuid::new_v4().to_string();
    let archive_path = games_root.join(".versions").join(&game.game_uid).join(&version_id);
    let journal_path = GameBodyUpdateService::journal_path(&games_root, &game.game_uid);
    if let Err(error) = GameBodyUpdateService::write_journal(&games_root, &game.game_uid, Path::new(&game.managed_path), &staging, &archive_path) {
        if let Some(version) = pending_protected.as_ref() { crate::repositories::release_pending_objects(version); }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    let swap = match GameBodyUpdateService::swap(Path::new(&game.managed_path), &staging, &archive_path) {
        Ok(swap) => swap,
        Err(error) => {
            if Path::new(&game.managed_path).is_dir() { let _ = GameBodyUpdateService::clear_journal(&journal_path); }
            if let Some(version) = pending_protected.as_ref() { crate::repositories::release_pending_objects(version); }
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    let receipt = match save_target {
        Some(target) => match SaveRepository::restore(app, game, profile, target, |progress, message| {
            TaskService::update(&state, task_id, TaskStatus::Running, 95 + progress / 20, message, None);
        }) {
            Ok(receipt) => Some(receipt),
            Err(error) => {
                return rollback_update(game, &swap, &journal_path, None, pending_protected.as_ref(), format!("更新后恢复存档失败：{error}"));
            }
        },
        None => None,
    };
    let body_version = GameBodyVersion {
        version_id: version_id.clone(),
        game_uid: game.game_uid.clone(),
        created_at: now_iso(),
        archive_path: swap.archive_path.to_string_lossy().to_string(),
        file_count: plan.file_count,
        total_bytes: plan.total_bytes,
    };
    let mut candidate = match state.store.lock() {
        Ok(store) => store.clone(),
        Err(_) => return rollback_update(game, &swap, &journal_path, receipt, pending_protected.as_ref(), "读取游戏更新记录失败".to_string()),
    };
    if let Some(protected) = protected {
        candidate.save_versions.push(protected);
    }
    candidate.body_versions.push(body_version);
    let Some(game_record) = candidate.games.iter_mut().find(|item| item.game_uid == game.game_uid) else {
        return rollback_update(game, &swap, &journal_path, receipt, pending_protected.as_ref(), "游戏记录不存在".to_string());
    };
    if let Some(protected) = candidate.save_versions.last().filter(|version| version.game_uid == game.game_uid) {
        game_record.latest_save_version_id = Some(protected.version_id.clone());
    }
    if let Err(error) = GameRepository::persist(app, &candidate) {
        let save_rollback = receipt.map(SaveRepository::rollback_restore);
        let body_rollback = GameBodyUpdateService::rollback(Path::new(&game.managed_path), &swap);
        if let Some(version) = pending_protected.as_ref() { crate::repositories::release_pending_objects(version); }
        if body_rollback.is_ok() { let _ = GameBodyUpdateService::clear_journal(&journal_path); }
        let mut errors = vec![format!("保存本体更新记录失败：{error}")];
        if let Some(Err(save_error)) = save_rollback { errors.push(format!("存档回滚失败：{save_error}")); }
        if let Err(body_error) = body_rollback { errors.push(format!("游戏本体回滚失败：{body_error}")); }
        return Err(errors.join("；"));
    }
    if let Some(receipt) = receipt { SaveRepository::finalize_restore(receipt); }
    *state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())? = candidate;
    if let Some(version) = pending_protected.as_ref() { crate::repositories::release_pending_objects(version); }
    let _ = GameBodyUpdateService::clear_journal(&journal_path);
    Ok(serde_json::json!({ "bodyVersionId": version_id, "fileCount": plan.file_count, "totalBytes": plan.total_bytes }))
}

fn rollback_update(
    game: &crate::domain::Game,
    swap: &crate::services::game_body_update_service::BodySwap,
    journal_path: &Path,
    receipt: Option<crate::repositories::save_repository::RestoreReceipt>,
    pending_version: Option<&crate::domain::SaveVersion>,
    error: String,
) -> Result<serde_json::Value, String> {
    if let Some(version) = pending_version { crate::repositories::release_pending_objects(version); }
    let save_rollback = receipt.map(SaveRepository::rollback_restore);
    let body_rollback = GameBodyUpdateService::rollback(Path::new(&game.managed_path), swap);
    if body_rollback.is_ok() { let _ = GameBodyUpdateService::clear_journal(journal_path); }
    let mut errors = vec![error];
    if let Some(Err(save_error)) = save_rollback { errors.push(format!("存档回滚失败：{save_error}")); }
    if let Err(body_error) = body_rollback { errors.push(format!("游戏本体回滚失败：{body_error}")); }
    Err(errors.join("；"))
}

fn load_update_context(state: &AppState, game_uid: &str) -> Result<(crate::domain::Game, SaveProfile, Option<SaveVersion>), String> {
    let store = state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())?;
    let game = GameLibraryService::find(&store, game_uid).ok_or_else(|| "游戏不存在".to_string())?;
    if !matches!(game.lifecycle, GameLifecycle::Active) { return Err("游戏尚未完成设置".to_string()); }
    let profile = store.save_profiles.iter().find(|profile| game.save_profile_id.as_deref() == Some(profile.profile_id.as_str()) && profile.game_uid == game_uid && profile.enabled).cloned().ok_or_else(|| "存档保护配置不存在".to_string())?;
    let latest = game.latest_save_version_id.as_ref().and_then(|id| store.save_versions.iter().find(|version| &version.version_id == id)).cloned();
    Ok((game, profile, latest))
}

fn reserve_update(state: &AppState, game_uid: &str) -> Result<(), String> {
    let mut operations = state.save_operations.lock().map_err(|_| "lock save operation state failed".to_string())?;
    if operations.contains(game_uid) { return Err("该游戏已有存档操作正在进行".to_string()); }
    if state.running_games.lock().map_err(|_| "lock running game state failed".to_string())?.contains_key(game_uid) { return Err("游戏运行中，暂时不能更新游戏本体".to_string()); }
    operations.insert(game_uid.to_string());
    Ok(())
}

fn release_update(state: &AppState, game_uid: &str) {
    if let Ok(mut operations) = state.save_operations.lock() { operations.remove(game_uid); }
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    let left = left.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_ascii_lowercase();
    let right = right.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_ascii_lowercase();
    left == right || left.starts_with(&(right.clone() + "\\")) || right.starts_with(&(left + "\\"))
}

fn now_iso() -> String {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|value| value.as_secs().to_string()).unwrap_or_else(|_| "0".to_string())
}
