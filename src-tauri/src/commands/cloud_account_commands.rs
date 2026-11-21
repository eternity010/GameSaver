use crate::services::cloud_account_service::merge_profile;
use crate::{
    app_state::AppState,
    commands::run_blocking,
    domain::{TaskCategory, TaskStatus},
    repositories::{BaiduConfigRepository, GameRepository},
    services::{BaiduNetdiskClient, CloudAccountProfile, CloudAccountService, TaskService},
};
use std::path::PathBuf;
use tauri::{AppHandle, Manager, State};

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudAccountStatusView {
    pub profile_available: bool,
    pub remote_size: Option<u64>,
    pub remote_updated_at: Option<u64>,
}

/// 查询云端账号状态：`list_account_files` 是一次远端目录列举。
#[tauri::command]
pub async fn get_cloud_account_status(app: AppHandle) -> Result<CloudAccountStatusView, String> {
    run_blocking(move || get_cloud_account_status_blocking(app)).await
}

fn get_cloud_account_status_blocking(app: AppHandle) -> Result<CloudAccountStatusView, String> {
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
    let task_id = begin_sync(&state, "上传 GameSaver 云端档案", false)?;
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        let result = upload_account(&app, &task_id_for_thread, &profile);
        finish_sync(
            &app,
            &task_id_for_thread,
            result,
            "GameSaver 云端档案上传完成",
            "GameSaver 云端档案上传失败",
        );
    });
    Ok(task_id)
}

#[tauri::command]
pub fn start_download_cloud_account_task(
    app: AppHandle,
    state: State<AppState>,
) -> Result<String, String> {
    let task_id = begin_sync(&state, "准备恢复云端 GameSaver 档案", true)?;
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            download_account(&app, &task_id_for_thread)
        }))
        .unwrap_or_else(|_| Err("恢复云端档案时发生内部错误，详情见日志".to_string()));
        app.state::<AppState>()
            .release_operation(AppState::ACCOUNT_RESTORE_KEY);
        finish_sync(
            &app,
            &task_id_for_thread,
            result,
            "GameSaver 云端档案恢复完成",
            "GameSaver 云端档案恢复失败",
        );
    });
    Ok(task_id)
}

fn upload_account(
    app: &AppHandle,
    task_id: &str,
    profile: &CloudAccountProfile,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        5,
        "正在连接百度网盘",
        None,
    );
    let client = load_baidu_client(app)?;
    let data_dir = app_data_dir(app)?;
    CloudAccountService::write(&client, profile, &data_dir.join("cloud-manifest-temp"))?;
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        100,
        "云端档案已更新",
        None,
    );
    Ok(())
}

fn download_account(app: &AppHandle, task_id: &str) -> Result<(), String> {
    let state = app.state::<AppState>();
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        5,
        "正在读取云端 GameSaver 档案",
        None,
    );
    let client = load_baidu_client(app)?;
    let files = list_account_files(&client)?;
    let data_dir = app_data_dir(app)?;
    let profile =
        CloudAccountService::read(&client, &files, &data_dir.join("cloud-manifest-temp"))?
            .ok_or_else(|| "百度网盘中还没有 GameSaver 云端档案".to_string())?;
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        55,
        "正在合并本地游戏库",
        None,
    );
    let cloud_auto_upload = profile.settings.auto_upload_body;
    // games_root 需要锁 library_root，先取出来，避免在 store 锁内嵌套加锁。
    let games_root = state.games_root()?;
    let imported_games = state.with_store_mut(|store| {
        let imported_games = merge_profile(store, &games_root, profile)?;
        BaiduConfigRepository::update_with_commit(
            &data_dir,
            |config| config.auto_upload_body = cloud_auto_upload,
            || {
                GameRepository::persist(app, store)
                    .map_err(|error| format!("保存云端游戏库失败：{error}"))
            },
        )?;
        Ok(imported_games)
    })?;
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        100,
        format!("已恢复 {imported_games} 个云端游戏"),
        None,
    );
    Ok(())
}

fn begin_sync(state: &AppState, message: &str, restoring: bool) -> Result<String, String> {
    let mut syncing = state
        .cloud_account_sync
        .lock()
        .map_err(|_| "读取云端账号同步状态失败".to_string())?;
    if *syncing {
        return Err("已有云端账号同步任务正在进行".to_string());
    }
    if restoring {
        state.claim_account_restore()?;
    }
    *syncing = true;
    match TaskService::create(
        state,
        if restoring {
            "restore_cloud_account"
        } else {
            "cloud_account_sync"
        },
        TaskCategory::Maintenance,
        None,
        message,
    ) {
        Ok(task_id) => Ok(task_id),
        Err(error) => {
            *syncing = false;
            if restoring {
                state.release_operation(AppState::ACCOUNT_RESTORE_KEY);
            }
            Err(error)
        }
    }
}

fn finish_sync(
    app: &AppHandle,
    task_id: &str,
    result: Result<(), String>,
    success: &str,
    failure: &str,
) {
    if let Ok(mut syncing) = app.state::<AppState>().cloud_account_sync.lock() {
        *syncing = false;
    }
    match result {
        Ok(()) => TaskService::finish(
            &app.state(),
            task_id,
            TaskStatus::Success,
            100,
            success,
            None,
            None,
        ),
        Err(error) if error == "任务已取消" => TaskService::finish(
            &app.state(),
            task_id,
            TaskStatus::Cancelled,
            100,
            "已取消云端账号同步",
            None,
            None,
        ),
        Err(error) => TaskService::finish(
            &app.state(),
            task_id,
            TaskStatus::Failed,
            100,
            failure,
            None,
            Some(error),
        ),
    }
}

fn list_account_files(
    client: &BaiduNetdiskClient,
) -> Result<Vec<crate::services::RemoteFile>, String> {
    match client.list(CloudAccountService::remote_directory()) {
        Ok(files) => Ok(files),
        Err(error) if error.contains("(-9)") || error.contains("(-8)") => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

fn load_baidu_client(app: &AppHandle) -> Result<BaiduNetdiskClient, String> {
    let data_dir = app_data_dir(app)?;
    BaiduNetdiskClient::load_configured(&data_dir)
}

fn app_data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|error| format!("解析 GameSaver 数据目录失败：{error}"))
}

fn load_auto_upload_setting(app: &AppHandle) -> bool {
    app_data_dir(app)
        .ok()
        .and_then(|path| BaiduConfigRepository::load(&path).ok().flatten())
        .is_some_and(|config| config.auto_upload_body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_restore_task_creation_releases_both_claims() {
        let dir = crate::test_support::TempWorkspace::new("account-start-failure");
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let state = AppState::new(
            crate::domain::AppStore::default(),
            dir.to_path_buf(),
            std::collections::HashMap::new(),
            blocker.join("tasks.json"),
        );
        assert!(begin_sync(&state, "restore", true).is_err());
        assert!(!*state.cloud_account_sync.lock().unwrap());
        assert!(state.save_operations.lock().unwrap().is_empty());
        assert!(state.claim_operation("game", "busy").is_ok());
    }
}
