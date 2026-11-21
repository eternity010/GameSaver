use crate::{
    commands::run_blocking,
    repositories::{BaiduConfigRepository, BaiduConfigView},
    services::baidu_auth_service::BaiduAuthService,
};
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

#[tauri::command]
pub fn get_baidu_config(app: AppHandle) -> Result<BaiduConfigView, String> {
    BaiduConfigRepository::view(&app_data_dir(&app)?)
}

#[tauri::command]
pub fn save_baidu_config(
    app: AppHandle,
    app_key: String,
    secret_key: String,
) -> Result<BaiduConfigView, String> {
    let app_data_dir = app_data_dir(&app)?;
    BaiduAuthService::save_config(&app_data_dir, &app_key, &secret_key)?;
    BaiduConfigRepository::view(&app_data_dir)
}

#[tauri::command]
pub fn set_baidu_auto_upload(app: AppHandle, enabled: bool) -> Result<BaiduConfigView, String> {
    let app_data_dir = app_data_dir(&app)?;
    BaiduConfigRepository::update(&app_data_dir, |existing| {
        let mut config =
            existing.ok_or_else(|| "请先在平台设置中保存百度 AppKey 和 SecretKey".to_string())?;
        config.auto_upload_body = enabled;
        Ok(config)
    })?;
    BaiduConfigRepository::view(&app_data_dir)
}

#[tauri::command]
pub fn update_baidu_save_sync_settings(
    app: AppHandle,
    auto_sync_save: bool,
    check_cloud_save_on_launch: bool,
    cloud_save_keep_limit: usize,
) -> Result<BaiduConfigView, String> {
    let app_data_dir = app_data_dir(&app)?;
    BaiduConfigRepository::update(&app_data_dir, |existing| {
        let mut config =
            existing.ok_or_else(|| "请先在平台设置中保存百度 AppKey 和 SecretKey".to_string())?;
        config.auto_sync_save = auto_sync_save;
        config.check_cloud_save_on_launch = check_cloud_save_on_launch;
        config.cloud_save_keep_limit = cloud_save_keep_limit.clamp(1, 100);
        Ok(config)
    })?;
    BaiduConfigRepository::view(&app_data_dir)
}

#[tauri::command]
pub fn build_baidu_authorize_url(app: AppHandle) -> Result<String, String> {
    BaiduAuthService::authorize_url(&app_data_dir(&app)?)
}

#[tauri::command]
pub async fn open_baidu_authorize_page(app: AppHandle) -> Result<String, String> {
    run_blocking(move || BaiduAuthService::open_authorization_page(&app_data_dir(&app)?)).await
}

/// 用授权码换取 token。
///
/// 体内 `Client::builder() … .send()` 是一次 OAuth 往返，必须在阻塞线程池上执行。
#[tauri::command]
pub async fn exchange_baidu_code(app: AppHandle, code: String) -> Result<(), String> {
    run_blocking(move || BaiduAuthService::exchange_code(&app_data_dir(&app)?, &code)).await
}

fn app_data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|error| format!("解析 GameSaver 数据目录失败：{error}"))
}
