use crate::{repositories::{BaiduConfig, BaiduConfigRepository, BaiduConfigView}, services::{BaiduNetdiskClient, BaiduToken}};
use reqwest::blocking::Client;
use serde::Deserialize;
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

const OAUTH_BASE: &str = "https://openapi.baidu.com/oauth/2.0";
const REDIRECT_URI: &str = "oob";

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

#[tauri::command]
pub fn get_baidu_config(app: AppHandle) -> Result<BaiduConfigView, String> {
    BaiduConfigRepository::view(&app_data_dir(&app)?)
}

#[tauri::command]
pub fn save_baidu_config(app: AppHandle, app_key: String, secret_key: String) -> Result<BaiduConfigView, String> {
    let app_data_dir = app_data_dir(&app)?;
    let auto_upload_body = BaiduConfigRepository::load(&app_data_dir)?.is_some_and(|config| config.auto_upload_body);
    let config = BaiduConfig { app_key: app_key.trim().to_string(), secret_key: secret_key.trim().to_string(), auto_upload_body };
    BaiduConfigRepository::save(&app_data_dir, config)?;
    BaiduConfigRepository::view(&app_data_dir)
}

#[tauri::command]
pub fn set_baidu_auto_upload(app: AppHandle, enabled: bool) -> Result<BaiduConfigView, String> {
    let app_data_dir = app_data_dir(&app)?;
    let mut config = BaiduConfigRepository::load(&app_data_dir)?
        .ok_or_else(|| "请先在平台设置中保存百度 AppKey 和 SecretKey".to_string())?;
    config.auto_upload_body = enabled;
    BaiduConfigRepository::save(&app_data_dir, config)?;
    BaiduConfigRepository::view(&app_data_dir)
}

#[tauri::command]
pub fn build_baidu_authorize_url(app: AppHandle) -> Result<String, String> {
    let config = BaiduConfigRepository::load(&app_data_dir(&app)?)?
        .ok_or_else(|| "请先在平台设置中保存百度 AppKey 和 SecretKey".to_string())?;
    let mut url = reqwest::Url::parse(&format!("{OAUTH_BASE}/authorize"))
        .map_err(|error| format!("生成百度授权地址失败：{error}"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &config.app_key)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", "basic,netdisk");
    Ok(url.to_string())
}

#[tauri::command]
pub fn exchange_baidu_code(app: AppHandle, code: String) -> Result<(), String> {
    let code = code.trim();
    if code.is_empty() {
        return Err("授权 Code 不能为空".to_string());
    }
    let config = BaiduConfigRepository::load(&app_data_dir(&app)?)?
        .ok_or_else(|| "请先在平台设置中保存百度 AppKey 和 SecretKey".to_string())?;
    let client = Client::builder()
        .no_proxy()
        .connect_timeout(std::time::Duration::from_secs(20))
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("pan.baidu.com")
        .build()
        .map_err(|error| format!("创建百度授权客户端失败：{error}"))?;
    let response = client
        .get(format!("{OAUTH_BASE}/token"))
        .query(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", config.app_key.as_str()),
            ("client_secret", config.secret_key.as_str()),
            ("redirect_uri", REDIRECT_URI),
        ])
        .send()
        .map_err(|error| format!("请求百度授权 Token 失败：{error}"))?;
    let status = response.status();
    let body = response.text().map_err(|error| format!("读取百度授权响应失败：{error}"))?;
    let value = serde_json::from_str::<serde_json::Value>(&body)
        .map_err(|error| format!("百度授权返回非 JSON：HTTP {status}，{error}"))?;
    if let Some(error_code) = value.get("error").and_then(serde_json::Value::as_str) {
        let description = value.get("error_description").and_then(serde_json::Value::as_str).unwrap_or("未知授权错误");
        return Err(format!("百度授权失败：{description} ({error_code})"));
    }
    let token: TokenResponse = serde_json::from_value(value)
        .map_err(|error| format!("百度授权响应格式无效：{error}"))?;
    let access_token = token.access_token.filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "百度授权响应缺少 access_token".to_string())?;
    let expires_at = token.expires_in.map(|seconds| now_millis().saturating_add(seconds.saturating_mul(1000)));
    BaiduNetdiskClient::save_token(&app_data_dir(&app)?, BaiduToken { access_token, expires_at, refresh_token: token.refresh_token })
}

fn app_data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path().app_data_dir().map_err(|error| format!("解析 GameSaver 数据目录失败：{error}"))
}

fn now_millis() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|duration| duration.as_millis() as u64).unwrap_or(0)
}
