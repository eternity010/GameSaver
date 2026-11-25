use std::path::Path;

use reqwest::blocking::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::repositories::{
    baidu_token_repository::{
        BaiduAuthorization, BaiduToken, BaiduTokenRepository, AUTHORIZATION_CHANGED,
    },
    BaiduConfig, BaiduConfigRepository,
};

use super::baidu_netdisk_service::safe_network_error;

const OAUTH_BASE: &str = "https://openapi.baidu.com/oauth/2.0";
const REDIRECT_URI: &str = "oob";

pub(crate) struct AuthorizationRequest {
    config: BaiduConfig,
    revision: Option<String>,
    configuration_id: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

pub(crate) struct BaiduAuthService;

impl BaiduAuthService {
    pub(crate) fn save_config(
        app_data_dir: &Path,
        app_key: &str,
        secret_key: &str,
    ) -> Result<(), String> {
        let app_key = app_key.trim();
        let secret_key = secret_key.trim();
        if app_key.is_empty() || secret_key.is_empty() {
            return Err("百度 AppKey 和 SecretKey 不能为空".into());
        }
        BaiduConfigRepository::update(app_data_dir, |existing| {
            let changed = existing
                .as_ref()
                .is_none_or(|config| config.app_key != app_key || config.secret_key != secret_key);
            let mut config = existing.unwrap_or(BaiduConfig {
                app_key: String::new(),
                secret_key: String::new(),
                auto_upload_body: false,
                auto_sync_save: true,
                check_cloud_save_on_launch: true,
                cloud_save_keep_limit: 10,
            });
            if changed {
                BaiduTokenRepository::invalidate(app_data_dir)?;
            }
            config.app_key = app_key.into();
            config.secret_key = secret_key.into();
            Ok(config)
        })
    }

    pub(crate) fn authorize_url(app_data_dir: &Path) -> Result<String, String> {
        let config = BaiduConfigRepository::load(app_data_dir)?
            .ok_or_else(|| "请先保存百度 AppKey 和 SecretKey".to_string())?;
        let mut url = reqwest::Url::parse(&format!("{OAUTH_BASE}/authorize"))
            .map_err(|_| "生成百度授权地址失败".to_string())?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &config.app_key)
            .append_pair("redirect_uri", REDIRECT_URI)
            .append_pair("scope", "basic,netdisk");
        Ok(url.into())
    }

    pub(crate) fn open_authorization_page(app_data_dir: &Path) -> Result<String, String> {
        Self::open_authorization_with(app_data_dir, open_in_default_browser)
    }

    pub(crate) fn open_developer_page(page: &str) -> Result<(), String> {
        let url = match page {
            "console" => "https://pan.baidu.com/union/console/applist",
            "docs" => "https://pan.baidu.com/union/doc/%E4%BD%BF%E7%94%A8%E5%85%A5%E9%97%A8/%E5%88%9B%E5%BB%BA%E5%BA%94%E7%94%A8/",
            _ => return Err("不支持的百度开放平台页面".into()),
        };
        open_in_default_browser(url)
    }

    fn open_authorization_with(
        app_data_dir: &Path,
        open: impl FnOnce(&str) -> Result<(), String>,
    ) -> Result<String, String> {
        let url = Self::authorize_url(app_data_dir)?;
        open(&url)?;
        Ok(url)
    }

    pub(crate) fn begin_authorization(app_data_dir: &Path) -> Result<AuthorizationRequest, String> {
        BaiduConfigRepository::with_current(app_data_dir, |config| {
            let config = config.ok_or_else(|| "请先保存百度 AppKey 和 SecretKey".to_string())?;
            let configuration_id = configuration_id(&config.app_key, &config.secret_key);
            let current = BaiduTokenRepository::load(app_data_dir, Some(&configuration_id))?;
            Ok(AuthorizationRequest {
                config,
                revision: current.map(|state| state.revision),
                configuration_id,
            })
        })
    }

    pub(crate) fn commit_authorization(
        app_data_dir: &Path,
        request: &AuthorizationRequest,
        token: BaiduToken,
    ) -> Result<(), String> {
        BaiduConfigRepository::with_current(app_data_dir, |config| {
            let config = config.ok_or_else(|| AUTHORIZATION_CHANGED.to_string())?;
            if configuration_id(&config.app_key, &config.secret_key) != request.configuration_id {
                return Err(AUTHORIZATION_CHANGED.into());
            }
            BaiduTokenRepository::update(app_data_dir, |current| {
                if current.as_ref().map(|state| state.revision.as_str())
                    != request.revision.as_deref()
                {
                    return Err(AUTHORIZATION_CHANGED.into());
                }
                Ok(BaiduAuthorization::new(
                    Some(token),
                    Some(request.configuration_id.clone()),
                ))
            })
            .map(|_| ())
        })
    }

    pub(crate) fn exchange_code(app_data_dir: &Path, code: &str) -> Result<(), String> {
        let code = code.trim();
        if code.is_empty() {
            return Err("授权 Code 不能为空".into());
        }
        let request = Self::begin_authorization(app_data_dir)?;
        let client = Client::builder()
            .no_proxy()
            .connect_timeout(std::time::Duration::from_secs(20))
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("pan.baidu.com")
            .build()
            .map_err(|error| format!("创建百度授权客户端失败：{}", safe_network_error(error)))?;
        let response = client
            .get(format!("{OAUTH_BASE}/token"))
            .query(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("client_id", request.config.app_key.as_str()),
                ("client_secret", request.config.secret_key.as_str()),
                ("redirect_uri", REDIRECT_URI),
            ])
            .send()
            .map_err(|error| format!("请求百度授权 Token 失败：{}", safe_network_error(error)))?;
        let status = response.status();
        let value: serde_json::Value = response
            .json()
            .map_err(|_| format!("百度授权响应无法解析：HTTP {status}"))?;
        if let Some(error) = value.get("error").and_then(serde_json::Value::as_str) {
            return Err(format!(
                "百度授权失败：{} ({error})",
                value
                    .get("error_description")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("请检查凭证并重新授权")
            ));
        }
        if !status.is_success() {
            return Err(format!("百度授权失败：HTTP {status}"));
        }
        let parsed: TokenResponse =
            serde_json::from_value(value).map_err(|_| "百度授权响应格式无效".to_string())?;
        let token = BaiduToken {
            access_token: parsed
                .access_token
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| "百度授权响应缺少 access_token".to_string())?,
            refresh_token: parsed
                .refresh_token
                .filter(|value| !value.trim().is_empty()),
            expires_at: parsed
                .expires_in
                .map(|seconds| now_millis().saturating_add(seconds.saturating_mul(1000))),
        };
        Self::commit_authorization(app_data_dir, &request, token)
    }
}

pub(crate) fn configuration_id(app_key: &str, secret_key: &str) -> String {
    let mut hash = Sha256::new();
    hash.update((app_key.len() as u64).to_le_bytes());
    hash.update(app_key.as_bytes());
    hash.update(secret_key.as_bytes());
    hex::encode(hash.finalize())
}

#[cfg(target_os = "windows")]
fn open_in_default_browser(url: &str) -> Result<(), String> {
    use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};

    let verb = "open".encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let target = url.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    } as isize;
    if result <= 32 {
        return Err(format!(
            "打开默认浏览器失败（系统错误码 {result}），请检查默认浏览器设置，或复制页面地址手动打开"
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn open_in_default_browser(_: &str) -> Result<(), String> {
    Err("当前平台暂不支持自动打开浏览器，请复制页面地址手动打开".into())
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(value: &str) -> BaiduToken {
        BaiduToken {
            access_token: value.into(),
            expires_at: Some(u64::MAX),
            refresh_token: Some("fake-refresh".into()),
        }
    }

    #[test]
    fn changed_credentials_invalidate_authorization_but_keep_preferences() {
        let root = crate::test_support::TempWorkspace::new("auth-config-change");
        BaiduAuthService::save_config(&root, "app-a", "secret-a").unwrap();
        let request = BaiduAuthService::begin_authorization(&root).unwrap();
        BaiduAuthService::commit_authorization(&root, &request, token("account-a")).unwrap();
        BaiduConfigRepository::update(&root, |existing| {
            let mut config = existing.unwrap();
            config.auto_upload_body = true;
            Ok(config)
        })
        .unwrap();
        BaiduAuthService::save_config(&root, "app-b", "secret-b").unwrap();
        assert!(BaiduTokenRepository::load(&root, None)
            .unwrap()
            .unwrap()
            .token
            .is_none());
        assert!(
            BaiduConfigRepository::load(&root)
                .unwrap()
                .unwrap()
                .auto_upload_body
        );
    }

    #[test]
    fn unchanged_credentials_keep_the_existing_session() {
        let root = crate::test_support::TempWorkspace::new("auth-same-config");
        BaiduAuthService::save_config(&root, "app", "secret").unwrap();
        let request = BaiduAuthService::begin_authorization(&root).unwrap();
        BaiduAuthService::commit_authorization(&root, &request, token("account")).unwrap();
        let before = BaiduTokenRepository::load(&root, None).unwrap().unwrap();
        BaiduAuthService::save_config(&root, " app ", " secret ").unwrap();
        assert_eq!(
            BaiduTokenRepository::load(&root, None)
                .unwrap()
                .unwrap()
                .revision,
            before.revision
        );
    }

    #[cfg(windows)]
    #[test]
    fn failed_configuration_commit_does_not_keep_the_old_authorization_active() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

        let root = crate::test_support::TempWorkspace::new("auth-config-locked");
        BaiduAuthService::save_config(&root, "app-a", "secret-a").unwrap();
        let request = BaiduAuthService::begin_authorization(&root).unwrap();
        BaiduAuthService::commit_authorization(&root, &request, token("account-a")).unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(BaiduConfigRepository::path(&root))
            .unwrap();

        let result = BaiduAuthService::save_config(&root, "app-b", "secret-b");
        drop(file);

        assert!(result.is_err());
        assert_eq!(
            BaiduConfigRepository::load(&root).unwrap().unwrap().app_key,
            "app-a"
        );
        assert!(BaiduTokenRepository::load(&root, None)
            .unwrap()
            .unwrap()
            .token
            .is_none());
        assert!(BaiduAuthService::commit_authorization(&root, &request, token("late")).is_err());
    }

    #[test]
    fn a_late_code_exchange_cannot_commit_after_configuration_changes() {
        let root = crate::test_support::TempWorkspace::new("auth-late-code");
        BaiduAuthService::save_config(&root, "app-a", "secret").unwrap();
        let request = BaiduAuthService::begin_authorization(&root).unwrap();
        BaiduAuthService::save_config(&root, "app-b", "secret").unwrap();
        BaiduAuthService::save_config(&root, "app-a", "secret").unwrap();
        assert!(BaiduAuthService::commit_authorization(&root, &request, token("late")).is_err());
        assert!(BaiduTokenRepository::load(&root, None)
            .unwrap()
            .unwrap()
            .token
            .is_none());
    }

    #[test]
    fn only_one_concurrent_exchange_can_commit() {
        let root = crate::test_support::TempWorkspace::new("auth-code-cas");
        BaiduAuthService::save_config(&root, "app", "secret").unwrap();
        let first = BaiduAuthService::begin_authorization(&root).unwrap();
        let second = BaiduAuthService::begin_authorization(&root).unwrap();
        BaiduAuthService::commit_authorization(&root, &first, token("first")).unwrap();
        assert!(BaiduAuthService::commit_authorization(&root, &second, token("late")).is_err());
        assert_eq!(
            BaiduTokenRepository::load(&root, None)
                .unwrap()
                .unwrap()
                .token
                .unwrap()
                .access_token,
            "first"
        );
    }

    #[test]
    fn authorization_url_uses_encoded_values_and_oob() {
        let root = crate::test_support::TempWorkspace::new("auth-url");
        BaiduAuthService::save_config(&root, "app&redirect_uri=other", "secret").unwrap();
        let url = reqwest::Url::parse(&BaiduAuthService::authorize_url(&root).unwrap()).unwrap();
        let pairs = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(pairs.get("client_id").unwrap(), "app&redirect_uri=other");
        assert_eq!(pairs.get("redirect_uri").unwrap(), "oob");
    }

    #[test]
    fn browser_open_uses_the_current_saved_configuration() {
        let root = crate::test_support::TempWorkspace::new("auth-browser-current");
        BaiduAuthService::save_config(&root, "old-app", "secret").unwrap();
        let old = BaiduAuthService::authorize_url(&root).unwrap();
        BaiduAuthService::save_config(&root, "new-app&value=other", "secret").unwrap();
        let mut opened = String::new();
        let result = BaiduAuthService::open_authorization_with(&root, |url| {
            opened = url.into();
            Ok(())
        })
        .unwrap();
        assert_eq!(opened, result);
        assert_ne!(opened, old);
        let url = reqwest::Url::parse(&opened).unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("openapi.baidu.com"));
        assert_eq!(url.path(), "/oauth/2.0/authorize");
        assert!(url
            .query_pairs()
            .any(|(key, value)| key == "client_id" && value == "new-app&value=other"));
        assert!(!opened.contains("secret"));
    }

    #[test]
    fn missing_configuration_does_not_open_a_browser() {
        let root = crate::test_support::TempWorkspace::new("auth-browser-unconfigured");
        let mut called = false;
        let result = BaiduAuthService::open_authorization_with(&root, |_| {
            called = true;
            Ok(())
        });
        assert!(result.is_err());
        assert!(!called);
    }

    #[test]
    fn browser_failure_is_returned_without_changing_authorization() {
        let root = crate::test_support::TempWorkspace::new("auth-browser-error");
        BaiduAuthService::save_config(&root, "app", "secret").unwrap();
        let request = BaiduAuthService::begin_authorization(&root).unwrap();
        BaiduAuthService::commit_authorization(&root, &request, token("account")).unwrap();
        let before = BaiduTokenRepository::load(&root, None).unwrap().unwrap();
        let error = BaiduAuthService::open_authorization_with(&root, |_| {
            Err("打开默认浏览器失败（系统错误码 31）".into())
        })
        .unwrap_err();
        assert!(error.contains("系统错误码 31"));
        let after = BaiduTokenRepository::load(&root, None).unwrap().unwrap();
        assert_eq!(before.revision, after.revision);
        assert_eq!(after.token.unwrap().access_token, "account");
    }
}
