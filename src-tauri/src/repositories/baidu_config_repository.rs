use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::store_file::{atomic_replace, load_with_recovery};

const CONFIG_FILE: &str = "baidu-netdisk-config.json";
const CONFIG_VERSION: u32 = 1;
const CONFIG_LABEL: &str = "百度网盘配置";
static CONFIG_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BaiduConfig {
    pub app_key: String,
    pub secret_key: String,
    #[serde(default)]
    pub auto_upload_body: bool,
    #[serde(default = "default_true")]
    pub auto_sync_save: bool,
    #[serde(default = "default_true")]
    pub check_cloud_save_on_launch: bool,
    #[serde(default = "default_save_keep_limit")]
    pub cloud_save_keep_limit: usize,
}

fn default_true() -> bool {
    true
}

fn default_save_keep_limit() -> usize {
    10
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredBaiduConfig {
    version: u32,
    protected: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BaiduConfigView {
    pub configured: bool,
    pub app_key: Option<String>,
    pub secret_key_configured: bool,
    pub auto_upload_body: bool,
    pub auto_sync_save: bool,
    pub check_cloud_save_on_launch: bool,
    pub cloud_save_keep_limit: usize,
}

pub struct BaiduConfigRepository;

impl BaiduConfigRepository {
    pub(crate) fn with_current<T>(
        app_data_dir: &Path,
        read: impl FnOnce(Option<BaiduConfig>) -> Result<T, String>,
    ) -> Result<T, String> {
        let _guard = CONFIG_LOCK
            .lock()
            .map_err(|_| "锁定百度配置失败".to_string())?;
        read(Self::load_locked(app_data_dir)?)
    }

    pub fn path(app_data_dir: &Path) -> PathBuf {
        app_data_dir.join(CONFIG_FILE)
    }

    pub fn load(app_data_dir: &Path) -> Result<Option<BaiduConfig>, String> {
        let _guard = CONFIG_LOCK
            .lock()
            .map_err(|_| "锁定百度配置失败".to_string())?;
        Self::load_locked(app_data_dir)
    }

    fn load_locked(app_data_dir: &Path) -> Result<Option<BaiduConfig>, String> {
        let path = Self::path(app_data_dir);
        let Some(raw) =
            load_with_recovery(&path, CONFIG_LABEL, |bytes: &[u8]| decode(bytes).is_ok())?
        else {
            return Ok(None);
        };
        decode(&raw).map(Some)
    }

    pub fn view(app_data_dir: &Path) -> Result<BaiduConfigView, String> {
        let config = Self::load(app_data_dir)?;
        Ok(BaiduConfigView {
            configured: config.is_some(),
            app_key: config.as_ref().map(|value| value.app_key.clone()),
            secret_key_configured: config
                .as_ref()
                .is_some_and(|value| !value.secret_key.is_empty()),
            auto_upload_body: config.as_ref().is_some_and(|value| value.auto_upload_body),
            auto_sync_save: config
                .as_ref()
                .map(|value| value.auto_sync_save)
                .unwrap_or(true),
            check_cloud_save_on_launch: config
                .as_ref()
                .map(|value| value.check_cloud_save_on_launch)
                .unwrap_or(true),
            cloud_save_keep_limit: config
                .as_ref()
                .map(|value| value.cloud_save_keep_limit)
                .unwrap_or(10),
        })
    }

    #[cfg(test)]
    pub fn save(app_data_dir: &Path, config: BaiduConfig) -> Result<(), String> {
        let _guard = CONFIG_LOCK
            .lock()
            .map_err(|_| "锁定百度配置失败".to_string())?;
        Self::save_locked(app_data_dir, config)
    }

    pub fn update(
        app_data_dir: &Path,
        update: impl FnOnce(Option<BaiduConfig>) -> Result<BaiduConfig, String>,
    ) -> Result<(), String> {
        let _guard = CONFIG_LOCK
            .lock()
            .map_err(|_| "锁定百度配置失败".to_string())?;
        let next = update(Self::load_locked(app_data_dir)?)?;
        Self::save_locked(app_data_dir, next)
    }

    pub fn update_with_commit<T>(
        app_data_dir: &Path,
        update: impl FnOnce(&mut BaiduConfig),
        commit: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let _guard = CONFIG_LOCK
            .lock()
            .map_err(|_| "锁定百度配置失败".to_string())?;
        let previous = Self::load_locked(app_data_dir)?;
        let changed = if let Some(config) = previous.as_ref() {
            let mut next = config.clone();
            update(&mut next);
            if &next != config {
                Self::save_locked(app_data_dir, next)?;
                true
            } else {
                false
            }
        } else {
            false
        };
        match commit() {
            Ok(value) => Ok(value),
            Err(error) => {
                if changed {
                    if let Some(config) = previous {
                        if let Err(rollback) = Self::save_locked(app_data_dir, config) {
                            return Err(format!("{error}；百度配置回滚失败：{rollback}"));
                        }
                    }
                }
                Err(error)
            }
        }
    }

    fn save_locked(app_data_dir: &Path, config: BaiduConfig) -> Result<(), String> {
        validate(&config)?;
        let plain = serde_json::to_vec(&config)
            .map_err(|error| format!("序列化{CONFIG_LABEL}凭证失败：{error}"))?;
        let stored = StoredBaiduConfig {
            version: CONFIG_VERSION,
            protected: hex::encode(protect(&plain)?),
        };
        let path = Self::path(app_data_dir);
        let bytes = serde_json::to_vec_pretty(&stored)
            .map_err(|error| format!("序列化{CONFIG_LABEL}失败：{error}"))?;
        atomic_replace(&path, &bytes, CONFIG_LABEL)
    }
}

/// 把落盘密文还原成可用配置，同时作为崩溃恢复候选的准入条件。
fn decode(raw: &[u8]) -> Result<BaiduConfig, String> {
    let stored = serde_json::from_slice::<StoredBaiduConfig>(raw)
        .map_err(|error| format!("解析{CONFIG_LABEL}失败：{error}"))?;
    if stored.version != CONFIG_VERSION {
        return Err(format!("{CONFIG_LABEL}版本不受支持：{}", stored.version));
    }
    let protected = hex::decode(stored.protected)
        .map_err(|error| format!("读取{CONFIG_LABEL}密文失败：{error}"))?;
    let plain = unprotect(&protected)?;
    let config = serde_json::from_slice::<BaiduConfig>(&plain)
        .map_err(|error| format!("解析{CONFIG_LABEL}凭证失败：{error}"))?;
    validate(&config)?;
    Ok(config)
}

fn validate(config: &BaiduConfig) -> Result<(), String> {
    if config.app_key.trim().is_empty() {
        return Err("百度 AppKey 不能为空".to_string());
    }
    if config.secret_key.trim().is_empty() {
        return Err("百度 SecretKey 不能为空".to_string());
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn protect(input: &[u8]) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB},
    };
    let mut input_blob = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_ptr() as *mut u8,
    };
    let mut output_blob = CRYPT_INTEGER_BLOB::default();
    let success = unsafe {
        CryptProtectData(
            &mut input_blob,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output_blob,
        )
    };
    if success == 0 {
        return Err(format!(
            "Windows 凭证加密失败：{}",
            std::io::Error::last_os_error()
        ));
    }
    let result = unsafe {
        std::slice::from_raw_parts(output_blob.pbData, output_blob.cbData as usize).to_vec()
    };
    unsafe { LocalFree(output_blob.pbData as _) };
    Ok(result)
}

#[cfg(not(windows))]
pub(crate) fn protect(_: &[u8]) -> Result<Vec<u8>, String> {
    Err("百度网盘应用凭证加密仅支持 Windows".to_string())
}

#[cfg(windows)]
pub(crate) fn unprotect(input: &[u8]) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        },
    };
    let mut input_blob = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_ptr() as *mut u8,
    };
    let mut output_blob = CRYPT_INTEGER_BLOB::default();
    let success = unsafe {
        CryptUnprotectData(
            &mut input_blob,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output_blob,
        )
    };
    if success == 0 {
        return Err(format!(
            "Windows 凭证解密失败：{}",
            std::io::Error::last_os_error()
        ));
    }
    let result = unsafe {
        std::slice::from_raw_parts(output_blob.pbData, output_blob.cbData as usize).to_vec()
    };
    unsafe { LocalFree(output_blob.pbData as _) };
    Ok(result)
}

#[cfg(not(windows))]
pub(crate) fn unprotect(_: &[u8]) -> Result<Vec<u8>, String> {
    Err("百度网盘应用凭证解密仅支持 Windows".to_string())
}

#[cfg(test)]
mod tests {
    use super::{BaiduConfig, BaiduConfigRepository};

    fn fixture() -> BaiduConfig {
        BaiduConfig {
            app_key: "review-app".into(),
            secret_key: "review-secret".into(),
            auto_upload_body: false,
            auto_sync_save: true,
            check_cloud_save_on_launch: true,
            cloud_save_keep_limit: 10,
        }
    }

    #[test]
    fn concurrent_field_updates_preserve_other_settings_and_credentials() {
        let dir = crate::test_support::TempWorkspace::new("account-config-update");
        BaiduConfigRepository::save(&dir, fixture()).unwrap();
        let start = std::sync::Arc::new(std::sync::Barrier::new(3));
        std::thread::scope(|scope| {
            let barrier = start.clone();
            let path = &*dir;
            scope.spawn(move || {
                barrier.wait();
                BaiduConfigRepository::update(path, |existing| {
                    let mut config = existing.unwrap();
                    config.app_key = "updated-app".into();
                    config.secret_key = "updated-secret".into();
                    config.auto_sync_save = false;
                    Ok(config)
                })
                .unwrap();
            });
            let barrier = start.clone();
            scope.spawn(move || {
                barrier.wait();
                BaiduConfigRepository::update_with_commit(
                    path,
                    |config| config.auto_upload_body = true,
                    || Ok(()),
                )
                .unwrap();
            });
            start.wait();
        });
        let config = BaiduConfigRepository::load(&dir).unwrap().unwrap();
        assert_eq!(config.app_key, "updated-app");
        assert_eq!(config.secret_key, "updated-secret");
        assert!(!config.auto_sync_save);
        assert!(config.auto_upload_body);
    }

    #[test]
    fn failed_store_commit_rolls_back_configuration() {
        let dir = crate::test_support::TempWorkspace::new("account-config-rollback");
        BaiduConfigRepository::save(&dir, fixture()).unwrap();
        let result = BaiduConfigRepository::update_with_commit(
            &dir,
            |config| config.auto_upload_body = true,
            || Err::<(), _>("injected store failure".into()),
        );
        assert_eq!(result.unwrap_err(), "injected store failure");
        assert_eq!(BaiduConfigRepository::load(&dir).unwrap(), Some(fixture()));
    }

    #[cfg(windows)]
    #[test]
    fn rollback_failure_is_included_in_the_result() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = crate::test_support::TempWorkspace::new("account-config-rollback-failure");
        BaiduConfigRepository::save(&dir, fixture()).unwrap();
        let mut locked = None;
        let result = BaiduConfigRepository::update_with_commit(
            &dir,
            |config| config.auto_upload_body = true,
            || {
                locked = Some(
                    std::fs::OpenOptions::new()
                        .read(true)
                        .share_mode(0)
                        .open(BaiduConfigRepository::path(&dir))
                        .unwrap(),
                );
                Err::<(), _>("injected store failure".into())
            },
        );
        let error = result.unwrap_err();
        assert!(error.contains("injected store failure"));
        assert!(error.contains("配置回滚失败"));
        drop(locked);
    }

    #[test]
    fn missing_local_credentials_are_not_created_by_account_restore() {
        let dir = crate::test_support::TempWorkspace::new("account-no-config");
        let value = BaiduConfigRepository::update_with_commit(
            &dir,
            |_| panic!("must not synthesize credentials"),
            || Ok(7),
        )
        .unwrap();
        assert_eq!(value, 7);
        assert!(BaiduConfigRepository::load(&dir).unwrap().is_none());
    }

    #[test]
    fn rejects_empty_credentials() {
        let result = BaiduConfigRepository::save(
            std::path::Path::new("target/test-config"),
            BaiduConfig {
                app_key: String::new(),
                secret_key: "secret".to_string(),
                auto_upload_body: false,
                auto_sync_save: true,
                check_cloud_save_on_launch: true,
                cloud_save_keep_limit: 10,
            },
        );
        assert!(result.is_err());
    }
}
