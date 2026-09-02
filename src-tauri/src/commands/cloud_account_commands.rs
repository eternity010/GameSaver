use crate::{
    app_state::AppState,
    domain::{game::CloudStatus, Game, GameHealth, GameLifecycle, SaveProfile, SaveScope, TaskStatus},
    repositories::{BaiduConfigRepository, GameRepository},
    services::{BaiduNetdiskClient, CloudAccountProfile, CloudAccountService, TaskService},
};
use std::{collections::HashMap, path::PathBuf};
use tauri::{AppHandle, Manager, State};
use uuid::Uuid;

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudAccountStatusView {
    pub profile_available: bool,
    pub remote_size: Option<u64>,
    pub remote_updated_at: Option<u64>,
}

#[tauri::command]
pub fn get_cloud_account_status(app: AppHandle) -> Result<CloudAccountStatusView, String> {
    let client = load_baidu_client(&app)?;
    let files = list_account_files(&client)?;
    let profile = files
        .iter()
        .find(|file| file.path == CloudAccountService::remote_profile_path() && !file.is_dir);
    Ok(CloudAccountStatusView {
        profile_available: profile.is_some(),
        remote_size: profile.map(|file| file.size),
        remote_updated_at: profile.and_then(|file| file.server_mtime),
    })
}

#[tauri::command]
pub fn start_upload_cloud_account_task(
    app: AppHandle,
    state: State<AppState>,
) -> Result<String, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "读取本地游戏库失败".to_string())?
        .clone();
    let auto_upload_body = load_auto_upload_setting(&app);
    let profile = CloudAccountService::build(&store, auto_upload_body);
    let task_id = begin_sync(&state, "上传 GameSaver 云端档案")?;
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        let result = upload_account(&app, &task_id_for_thread, &profile);
        finish_sync(&app, &task_id_for_thread, result, "GameSaver 云端档案上传完成", "GameSaver 云端档案上传失败");
    });
    Ok(task_id)
}

#[tauri::command]
pub fn start_download_cloud_account_task(
    app: AppHandle,
    state: State<AppState>,
) -> Result<String, String> {
    let task_id = begin_sync(&state, "准备恢复云端 GameSaver 档案")?;
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        let result = download_account(&app, &task_id_for_thread);
        finish_sync(&app, &task_id_for_thread, result, "GameSaver 云端档案恢复完成", "GameSaver 云端档案恢复失败");
    });
    Ok(task_id)
}

fn upload_account(
    app: &AppHandle,
    task_id: &str,
    profile: &CloudAccountProfile,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    TaskService::update(&state, task_id, TaskStatus::Running, 5, "正在连接百度网盘", None);
    let client = load_baidu_client(app)?;
    let data_dir = app_data_dir(app)?;
    CloudAccountService::write(&client, profile, &data_dir.join("cloud-manifest-temp"))?;
    TaskService::update(&state, task_id, TaskStatus::Running, 100, "云端档案已更新", None);
    Ok(())
}

fn download_account(app: &AppHandle, task_id: &str) -> Result<(), String> {
    let state = app.state::<AppState>();
    TaskService::update(&state, task_id, TaskStatus::Running, 5, "正在读取云端 GameSaver 档案", None);
    let client = load_baidu_client(app)?;
    let files = list_account_files(&client)?;
    let data_dir = app_data_dir(app)?;
    let profile = CloudAccountService::read(&client, &files, &data_dir.join("cloud-manifest-temp"))?
        .ok_or_else(|| "百度网盘中还没有 GameSaver 云端档案".to_string())?;
    TaskService::update(&state, task_id, TaskStatus::Running, 55, "正在合并本地游戏库", None);
    let cloud_auto_upload = profile.settings.auto_upload_body;
    let (candidate, imported_games) = merge_profile(app, &state, profile)?;
    let old_config = BaiduConfigRepository::load(&data_dir)?;
    if let Some(mut config) = old_config.clone() {
        config.auto_upload_body = cloud_auto_upload;
        BaiduConfigRepository::save(&data_dir, config)?;
    }
    if let Err(error) = GameRepository::persist(app, &candidate) {
        if let Some(config) = old_config {
            let _ = BaiduConfigRepository::save(&data_dir, config);
        }
        return Err(format!("保存云端游戏库失败：{error}"));
    }
    *state
        .store
        .lock()
        .map_err(|_| "更新本地游戏库失败".to_string())? = candidate;
    TaskService::update(&state, task_id, TaskStatus::Running, 100, format!("已恢复 {imported_games} 个云端游戏"), None);
    Ok(())
}

fn merge_profile(
    app: &AppHandle,
    state: &AppState,
    profile: CloudAccountProfile,
) -> Result<(crate::domain::AppStore, usize), String> {
    let mut candidate = state
        .store
        .lock()
        .map_err(|_| "读取本地游戏库失败".to_string())?
        .clone();
    let mut imported_games = 0;
    let mut cloud_uid_to_local_uid = HashMap::new();
    let mut cloud_uid_to_game_key = HashMap::new();
    for cloud_game in &profile.games {
        let game_key = if cloud_game.game_key.trim().is_empty() {
            Game::derive_game_key(&cloud_game.display_name)
        } else {
            Game::derive_game_key(&cloud_game.game_key)
        };
        let local_index = candidate.games.iter().position(|game| {
            game.game_key == game_key || game.game_uid == cloud_game.game_uid
        });
        let local_uid = if let Some(index) = local_index {
            let local = &mut candidate.games[index];
            local.game_key = game_key.clone();
            local.display_name = cloud_game.display_name.clone();
            local.launch = cloud_game.launch.clone();
            local.last_played_at = cloud_game.last_played_at.clone();
            local.cloud_status = CloudStatus::Synced;
            local.game_uid.clone()
        } else {
            let local_uid = Uuid::new_v4().to_string();
            let mut game = Game::new_pending(
                &cloud_game.display_name,
                state.games_root()?.join(&local_uid).to_string_lossy(),
                &cloud_game.launch.executable_relative_path,
            );
            game.game_uid = local_uid.clone();
            game.game_key = game_key.clone();
            game.launch = cloud_game.launch.clone();
            game.save_profile_id = cloud_game.save_profile_id.clone();
            game.last_played_at = cloud_game.last_played_at.clone();
            game.lifecycle = GameLifecycle::NeedsRepair;
            game.health = GameHealth::Broken;
            game.cloud_status = CloudStatus::Synced;
            candidate.games.push(game);
            imported_games += 1;
            local_uid
        };
        cloud_uid_to_local_uid.insert(cloud_game.game_uid.clone(), local_uid);
        cloud_uid_to_game_key.insert(cloud_game.game_uid.clone(), game_key);
    }
    for cloud_profile in &profile.save_profiles {
        let local_uid = cloud_uid_to_local_uid
            .get(&cloud_profile.game_uid)
            .cloned()
            .or_else(|| {
                let game_key = if cloud_profile.game_key.trim().is_empty() {
                    cloud_uid_to_game_key.get(&cloud_profile.game_uid).cloned()
                } else {
                    Some(Game::derive_game_key(&cloud_profile.game_key))
                }?;
                candidate
                    .games
                    .iter()
                    .find(|game| game.game_key == game_key)
                    .map(|game| game.game_uid.clone())
            });
        let Some(local_uid) = local_uid else {
            continue;
        };
        let game = candidate
            .games
            .iter()
            .find(|game| game.game_uid == local_uid)
            .cloned()
            .ok_or_else(|| "云端存档配置对应的本地游戏不存在".to_string())?;
        let existing = candidate.save_profiles.iter().find(|item| item.profile_id == cloud_profile.profile_id || item.game_uid == local_uid).cloned();
        let scopes = cloud_profile.scopes.iter().enumerate().map(|(index, scope)| {
            let fallback = existing.as_ref().and_then(|item| item.scopes.get(index)).map(|item| item.root_path.as_str());
            resolve_scope(app, &game, scope, fallback)
        }).collect::<Result<Vec<_>, _>>()?;
        let save_profile = SaveProfile {
            profile_id: cloud_profile.profile_id.clone(),
            game_uid: local_uid.clone(),
            executable_hash: cloud_profile.executable_hash.clone(),
            scopes,
            detection_evidence: cloud_profile.detection_evidence.clone(),
            confidence: cloud_profile.confidence,
            enabled: true,
            keep_versions: existing.as_ref().map(|item| item.keep_versions).unwrap_or(5),
            created_at: existing.as_ref().map(|item| item.created_at.clone()).unwrap_or_else(|| cloud_profile.updated_at.clone()),
            updated_at: cloud_profile.updated_at.clone(),
        };
        candidate.save_profiles.retain(|item| item.profile_id != save_profile.profile_id && item.game_uid != save_profile.game_uid);
        candidate.save_profiles.push(save_profile);
        if let Some(local) = candidate.games.iter_mut().find(|item| item.game_uid == local_uid) {
            local.save_profile_id = Some(cloud_profile.profile_id.clone());
        }
    }
    candidate.normalize();
    Ok((candidate, imported_games))
}

fn resolve_scope(
    app: &AppHandle,
    game: &Game,
    scope: &crate::services::cloud_account_service::CloudSaveScope,
    fallback: Option<&str>,
) -> Result<SaveScope, String> {
    let root_path = match scope.root_type {
        crate::domain::SaveRootType::ManagedGame => Some(game.managed_path.clone()),
        crate::domain::SaveRootType::AppData => std::env::var("APPDATA").ok(),
        crate::domain::SaveRootType::LocalAppData => std::env::var("LOCALAPPDATA").ok(),
        crate::domain::SaveRootType::LocalLow => std::env::var("USERPROFILE").ok().map(|path| PathBuf::from(path).join("AppData").join("LocalLow").to_string_lossy().to_string()),
        crate::domain::SaveRootType::Documents => std::env::var("USERPROFILE").ok().map(|path| PathBuf::from(path).join("Documents").to_string_lossy().to_string()),
        crate::domain::SaveRootType::SavedGames => std::env::var("USERPROFILE").ok().map(|path| PathBuf::from(path).join("Saved Games").to_string_lossy().to_string()),
        crate::domain::SaveRootType::UserProfile => std::env::var("USERPROFILE").ok(),
        crate::domain::SaveRootType::Custom => scope.custom_root_path.clone(),
    }
    .or_else(|| fallback.map(str::to_string))
    .ok_or_else(|| format!("无法解析云端存档范围：{:?}", scope.root_type))?;
    let _ = app;
    Ok(SaveScope {
        root_type: scope.root_type,
        root_path,
        confirmed_files: scope.confirmed_files.clone(),
        include_directories: scope.include_directories.clone(),
        exclude_exact: scope.exclude_exact.clone(),
        exclude_patterns: scope.exclude_patterns.clone(),
        exclude_directories: scope.exclude_directories.clone(),
        unknown_file_policy: scope.unknown_file_policy,
        max_file_bytes: scope.max_file_bytes,
    })
}

fn begin_sync(state: &AppState, message: &str) -> Result<String, String> {
    let mut syncing = state.cloud_account_sync.lock().map_err(|_| "读取云端账号同步状态失败".to_string())?;
    if *syncing {
        return Err("已有云端账号同步任务正在进行".to_string());
    }
    *syncing = true;
    match TaskService::create(state, "cloud_account_sync", None, message) {
        Ok(task_id) => Ok(task_id),
        Err(error) => {
            *syncing = false;
            Err(error)
        }
    }
}

fn finish_sync(app: &AppHandle, task_id: &str, result: Result<(), String>, success: &str, failure: &str) {
    if let Ok(mut syncing) = app.state::<AppState>().cloud_account_sync.lock() {
        *syncing = false;
    }
    match result {
        Ok(()) => TaskService::finish(&app.state(), task_id, TaskStatus::Success, 100, success, None, None),
        Err(error) if error == "任务已取消" => TaskService::finish(&app.state(), task_id, TaskStatus::Cancelled, 100, "已取消云端账号同步", None, None),
        Err(error) => TaskService::finish(&app.state(), task_id, TaskStatus::Failed, 100, failure, None, Some(error)),
    }
}

fn list_account_files(client: &BaiduNetdiskClient) -> Result<Vec<crate::services::RemoteFile>, String> {
    match client.list(CloudAccountService::remote_directory()) {
        Ok(files) => Ok(files),
        Err(error) if error.contains("(-9)") || error.contains("(-8)") => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

fn load_baidu_client(app: &AppHandle) -> Result<BaiduNetdiskClient, String> {
    let data_dir = app_data_dir(app)?;
    let config = BaiduConfigRepository::load(&data_dir)?;
    match config {
        Some(config) => BaiduNetdiskClient::load_from_app_data_with_credentials(&data_dir, Some(&config.app_key), Some(&config.secret_key)),
        None => BaiduNetdiskClient::load_from_app_data(&data_dir),
    }
}

fn app_data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path().app_data_dir().map_err(|error| format!("解析 GameSaver 数据目录失败：{error}"))
}

fn load_auto_upload_setting(app: &AppHandle) -> bool {
    app_data_dir(app).ok().and_then(|path| BaiduConfigRepository::load(&path).ok().flatten()).is_some_and(|config| config.auto_upload_body)
}
