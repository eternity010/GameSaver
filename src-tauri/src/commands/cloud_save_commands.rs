use crate::{
    app_state::AppState,
    domain::{Game, SaveProfile, SaveVersion, TaskStatus},
    repositories::BaiduConfigRepository,
    services::{
        BaiduNetdiskClient, CloudSaveManifestVersion, CloudSaveService, CloudSaveSyncStatusView,
        TaskService,
    },
};
use tauri::{AppHandle, Manager, State};

#[tauri::command]
pub fn get_cloud_save_status(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
) -> Result<CloudSaveSyncStatusView, String> {
    let store = state.store.lock().map_err(|_| "锁定本地存储失败".to_string())?;
    let game = store
        .games
        .iter()
        .find(|g| g.game_uid == game_uid)
        .ok_or_else(|| "未找到指定游戏".to_string())?
        .clone();
    drop(store);

    CloudSaveService::get_sync_status(&app, &game)
}

#[tauri::command]
pub fn list_cloud_save_versions(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
) -> Result<Vec<CloudSaveManifestVersion>, String> {
    let store = state.store.lock().map_err(|_| "锁定本地存储失败".to_string())?;
    let game = store
        .games
        .iter()
        .find(|g| g.game_uid == game_uid)
        .ok_or_else(|| "未找到指定游戏".to_string())?
        .clone();
    drop(store);

    let client = load_baidu_client(&app)?;
    let manifest = CloudSaveService::fetch_manifest(&client, &game.game_key, &game.game_uid)?;
    Ok(manifest.map(|m| m.versions).unwrap_or_default())
}

#[tauri::command]
pub fn start_upload_save_version_task(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
    version_id: String,
) -> Result<String, String> {
    let store = state.store.lock().map_err(|_| "锁定本地存储失败".to_string())?;
    let game = store
        .games
        .iter()
        .find(|g| g.game_uid == game_uid)
        .ok_or_else(|| "未找到指定游戏".to_string())?
        .clone();
    let profile = game
        .save_profile_id
        .as_ref()
        .and_then(|pid| store.save_profiles.iter().find(|p| &p.profile_id == pid))
        .ok_or_else(|| "未找到该游戏的存档保护规则".to_string())?
        .clone();
    let version = store
        .save_versions
        .iter()
        .find(|v| v.game_uid == game_uid && v.version_id == version_id)
        .ok_or_else(|| "未找到指定的本地存档版本".to_string())?
        .clone();
    drop(store);

    let app_data_dir = app.path().app_data_dir().map_err(|err| format!("解析应用数据目录失败：{err}"))?;
    let keep_limit = BaiduConfigRepository::load(&app_data_dir)?
        .map(|c| c.cloud_save_keep_limit)
        .unwrap_or(10);

    let task_id = begin_sync(&state, &format!("上传【{}】游戏存档至百度网盘", game.display_name), &game.game_uid)?;
    let task_id_for_thread = task_id.clone();
    let app_for_thread = app.clone();

    std::thread::spawn(move || {
        let result = upload_save_worker(&app_for_thread, &task_id_for_thread, &game, &profile, &version, keep_limit);
        finish_sync(
            &app_for_thread,
            &task_id_for_thread,
            result,
            &format!("【{}】游戏存档已成功同步至百度网盘", game.display_name),
            &format!("【{}】游戏存档云端同步失败", game.display_name),
        );
    });

    Ok(task_id)
}

#[tauri::command]
pub fn start_restore_cloud_save_task(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
    version_id: String,
) -> Result<String, String> {
    let store = state.store.lock().map_err(|_| "锁定本地存储失败".to_string())?;
    let game = store
        .games
        .iter()
        .find(|g| g.game_uid == game_uid)
        .ok_or_else(|| "未找到指定游戏".to_string())?
        .clone();
    let profile = game
        .save_profile_id
        .as_ref()
        .and_then(|pid| store.save_profiles.iter().find(|p| &p.profile_id == pid))
        .ok_or_else(|| "未找到该游戏的存档保护规则".to_string())?
        .clone();
    drop(store);

    let client = load_baidu_client(&app)?;
    let manifest = CloudSaveService::fetch_manifest(&client, &game.game_key, &game.game_uid)?
        .ok_or_else(|| "未找到云端存档清单".to_string())?;
    let remote_version = manifest
        .versions
        .iter()
        .find(|v| v.version_id == version_id)
        .ok_or_else(|| "未找到指定的云端存档版本".to_string())?
        .clone();

    let task_id = begin_sync(&state, &format!("从云端还原【{}】游戏存档", game.display_name), &game.game_uid)?;
    let task_id_for_thread = task_id.clone();
    let app_for_thread = app.clone();

    std::thread::spawn(move || {
        let result = restore_save_worker(&app_for_thread, &task_id_for_thread, &game, &profile, &remote_version);
        finish_sync(
            &app_for_thread,
            &task_id_for_thread,
            result,
            &format!("【{}】云端存档已成功还原至本地", game.display_name),
            &format!("【{}】云端存档还原失败", game.display_name),
        );
    });

    Ok(task_id)
}

#[tauri::command]
pub fn delete_cloud_save_version(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
    version_id: String,
) -> Result<Vec<CloudSaveManifestVersion>, String> {
    let store = state.store.lock().map_err(|_| "锁定本地存储失败".to_string())?;
    let game = store
        .games
        .iter()
        .find(|g| g.game_uid == game_uid)
        .ok_or_else(|| "未找到指定游戏".to_string())?
        .clone();
    drop(store);

    let client = load_baidu_client(&app)?;
    let manifest = CloudSaveService::delete_cloud_version(&client, &game.game_key, &game.game_uid, &version_id)?;
    Ok(manifest.versions)
}

fn upload_save_worker(
    app: &AppHandle,
    task_id: &str,
    game: &Game,
    profile: &SaveProfile,
    version: &SaveVersion,
    keep_limit: usize,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    TaskService::update(&state, task_id, TaskStatus::Running, 5, "正在准备上传存档", None);
    let client = load_baidu_client(app)?;
    CloudSaveService::upload_save_version(app, &client, game, profile, version, keep_limit, |pct, msg| {
        TaskService::update(&state, task_id, TaskStatus::Running, pct, msg, None);
        true
    })?;
    Ok(())
}

fn restore_save_worker(
    app: &AppHandle,
    task_id: &str,
    game: &Game,
    profile: &SaveProfile,
    remote_version: &CloudSaveManifestVersion,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    TaskService::update(&state, task_id, TaskStatus::Running, 5, "正在连接网盘下载存档", None);
    let client = load_baidu_client(app)?;
    CloudSaveService::download_and_restore_cloud_save(app, &client, game, profile, remote_version, |pct, msg| {
        TaskService::update(&state, task_id, TaskStatus::Running, pct, msg, None);
        true
    })?;
    Ok(())
}

fn begin_sync(state: &AppState, title: &str, game_uid: &str) -> Result<String, String> {
    let task_id = TaskService::create(
        state,
        "sync_cloud_save",
        Some(game_uid.to_string()),
        title,
    )?;
    TaskService::update(state, &task_id, TaskStatus::Running, 0, "任务已创建", None);
    Ok(task_id)
}

fn finish_sync(
    app: &AppHandle,
    task_id: &str,
    result: Result<(), String>,
    success_message: &str,
    failed_prefix: &str,
) {
    let state = app.state::<AppState>();
    match result {
        Ok(_) => {
            TaskService::update(&state, task_id, TaskStatus::Success, 100, success_message, None);
        }
        Err(error) => {
            TaskService::update(
                &state,
                task_id,
                TaskStatus::Failed,
                100,
                &format!("{failed_prefix}：{error}"),
                Some(error),
            );
        }
    }
}

fn load_baidu_client(app: &AppHandle) -> Result<BaiduNetdiskClient, String> {
    let app_data_dir = app.path().app_data_dir().map_err(|err| format!("解析应用数据目录失败：{err}"))?;
    BaiduNetdiskClient::load_from_app_data(&app_data_dir)
}
