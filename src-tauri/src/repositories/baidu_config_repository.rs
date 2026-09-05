use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use uuid::Uuid;

const CONFIG_FILE: &str = "baidu-netdisk-config.json";
const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize, Serialize)]
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
    pub fn path(app_data_dir: &Path) -> PathBuf {
        app_data_dir.join(CONFIG_FILE)
    }

    pub fn load(app_data_dir: &Path) -> Result<Option<BaiduConfig>, String> {
        let path = Self::path(app_data_dir);
        if !path.is_file() {
            return Ok(None);
        }
        let raw = fs::read(&path).map_err(|error| format!("读取百度网盘应用配置失败：{error}"))?;
        let stored = serde_json::from_slice::<StoredBaiduConfig>(&raw)
            .map_err(|error| format!("解析百度网盘应用配置失败：{error}"))?;
        if stored.version != CONFIG_VERSION {
            return Err("百度网盘应用配置版本不受支持".to_string());
        }
        let protected = hex::decode(stored.protected)
            .map_err(|error| format!("读取百度网盘应用配置密文失败：{error}"))?;
        let plain = unprotect(&protected)?;
        let config = serde_json::from_slice::<BaiduConfig>(&plain)
            .map_err(|error| format!("解析百度网盘应用凭证失败：{error}"))?;
        validate(&config)?;
        Ok(Some(config))
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

    pub fn save(app_data_dir: &Path, config: BaiduConfig) -> Result<(), String> {
        validate(&config)?;
        fs::create_dir_all(app_data_dir)
            .map_err(|error| format!("创建百度网盘配置目录失败：{error}"))?;
        let plain = serde_json::to_vec(&config)
            .map_err(|error| format!("序列化百度网盘应用凭证失败：{error}"))?;
        let stored = StoredBaiduConfig {
            version: CONFIG_VERSION,
            protected: hex::encode(protect(&plain)?),
        };
        let path = Self::path(app_data_dir);
        let bytes = serde_json::to_vec_pretty(&stored)
            .map_err(|error| format!("序列化百度网盘配置失败：{error}"))?;
        atomic_replace(&path, &bytes)
    }
}

fn atomic_replace(target: &Path, bytes: &[u8]) -> Result<(), String> {
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let temporary = target.with_file_name(format!(".{name}.tmp-{}", Uuid::new_v4().simple()));
    let backup = target.with_file_name(format!(".{name}.bak-{}", Uuid::new_v4().simple()));
    let result = (|| -> Result<(), String> {
        let mut file = fs::File::create(&temporary)
            .map_err(|error| format!("创建百度网盘配置临时文件失败：{error}"))?;
        file.write_all(bytes)
            .map_err(|error| format!("写入百度网盘配置临时文件失败：{error}"))?;
        file.sync_all()
            .map_err(|error| format!("刷新百度网盘配置临时文件失败：{error}"))?;
        let had_target = target.exists();
        if had_target {
            fs::rename(target, &backup)
                .map_err(|error| format!("暂存百度网盘配置失败：{error}"))?;
        }
        if let Err(error) = fs::rename(&temporary, target) {
            if had_target {
                let _ = fs::rename(&backup, target);
            }
            return Err(format!("提交百度网盘配置失败：{error}"));
        }
        if had_target {
            let _ = fs::remove_file(&backup);
        }
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result
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
fn protect(input: &[u8]) -> Result<Vec<u8>, String> {
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
fn protect(_: &[u8]) -> Result<Vec<u8>, String> {
    Err("百度网盘应用凭证加密仅支持 Windows".to_string())
}

#[cfg(windows)]
fn unprotect(input: &[u8]) -> Result<Vec<u8>, String> {
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
fn unprotect(_: &[u8]) -> Result<Vec<u8>, String> {
    Err("百度网盘应用凭证解密仅支持 Windows".to_string())
}

#[cfg(test)]
mod tests {
    use super::{BaiduConfig, BaiduConfigRepository};

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
