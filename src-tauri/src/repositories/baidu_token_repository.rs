use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

use super::{
    baidu_config_repository::{protect, unprotect},
    store_file::{atomic_replace, load_with_recovery},
};

const TOKEN_FILE: &str = "baidu-netdisk-token.json";
const TOKEN_LABEL: &str = "百度网盘授权信息";
static TOKEN_LOCK: Mutex<()> = Mutex::new(());
pub(crate) const AUTHORIZATION_CHANGED: &str = "百度网盘授权已变更，当前任务已停止，请重新发起操作";

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BaiduToken {
    #[serde(alias = "access_token")]
    pub access_token: String,
    #[serde(default, alias = "expires_at")]
    pub expires_at: Option<u64>,
    #[serde(
        default,
        alias = "refresh_token",
        skip_serializing_if = "Option::is_none"
    )]
    pub refresh_token: Option<String>,
}

impl std::fmt::Debug for BaiduToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BaiduToken")
            .field("access_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BaiduAuthorization {
    pub revision: String,
    pub configuration_id: Option<String>,
    pub token: Option<BaiduToken>,
}

impl BaiduAuthorization {
    pub(crate) fn new(token: Option<BaiduToken>, configuration_id: Option<String>) -> Self {
        Self {
            revision: uuid::Uuid::new_v4().to_string(),
            configuration_id,
            token,
        }
    }
}

#[derive(Deserialize, Serialize)]
struct ProtectedAuthorization {
    version: u32,
    protected: String,
}

pub(crate) struct BaiduTokenRepository;

impl BaiduTokenRepository {
    pub(crate) fn path(app_data_dir: &Path) -> PathBuf {
        app_data_dir.join(TOKEN_FILE)
    }

    pub(crate) fn load(
        app_data_dir: &Path,
        configuration_id: Option<&str>,
    ) -> Result<Option<BaiduAuthorization>, String> {
        let _guard = TOKEN_LOCK
            .lock()
            .map_err(|_| "锁定百度授权信息失败".to_string())?;
        Self::load_locked(app_data_dir, configuration_id)
    }

    pub(crate) fn update(
        app_data_dir: &Path,
        update: impl FnOnce(Option<BaiduAuthorization>) -> Result<BaiduAuthorization, String>,
    ) -> Result<BaiduAuthorization, String> {
        let _guard = TOKEN_LOCK
            .lock()
            .map_err(|_| "锁定百度授权信息失败".to_string())?;
        let next = update(Self::load_locked(app_data_dir, None)?)?;
        validate(&next)?;
        Self::save_locked(app_data_dir, &next)?;
        Ok(next)
    }

    pub(crate) fn invalidate(app_data_dir: &Path) -> Result<(), String> {
        Self::update(app_data_dir, |_| Ok(BaiduAuthorization::new(None, None))).map(|_| ())
    }

    fn load_locked(
        app_data_dir: &Path,
        configuration_id: Option<&str>,
    ) -> Result<Option<BaiduAuthorization>, String> {
        let path = Self::path(app_data_dir);
        let Some(bytes) = load_with_recovery(&path, TOKEN_LABEL, |raw| {
            decode(raw, configuration_id).is_ok()
        })?
        else {
            return Ok(None);
        };
        let (authorization, plaintext) = decode(&bytes, configuration_id)?;
        if plaintext {
            Self::save_locked(app_data_dir, &authorization)?;
        }
        remove_plaintext_residue(&path);
        Ok(Some(authorization))
    }

    fn save_locked(app_data_dir: &Path, authorization: &BaiduAuthorization) -> Result<(), String> {
        let plain =
            serde_json::to_vec(authorization).map_err(|_| "序列化百度授权信息失败".to_string())?;
        let stored = ProtectedAuthorization {
            version: 1,
            protected: hex::encode(protect(&plain)?),
        };
        let bytes =
            serde_json::to_vec_pretty(&stored).map_err(|_| "序列化百度授权密文失败".to_string())?;
        let path = Self::path(app_data_dir);
        atomic_replace(&path, &bytes, TOKEN_LABEL)?;
        remove_plaintext_residue(&path);
        Ok(())
    }
}

fn decode(
    bytes: &[u8],
    configuration_id: Option<&str>,
) -> Result<(BaiduAuthorization, bool), String> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| "百度授权信息损坏，无法解析".to_string())?;
    let plaintext = value.get("protected").is_none() && value.get("version").is_none();
    let authorization = if plaintext {
        let token = serde_json::from_value::<BaiduToken>(value)
            .map_err(|_| "百度授权信息格式无效".to_string())?;
        BaiduAuthorization::new(Some(token), configuration_id.map(str::to_owned))
    } else {
        let stored: ProtectedAuthorization =
            serde_json::from_value(value).map_err(|_| "百度授权密文格式无效".to_string())?;
        if stored.version != 1 {
            return Err("百度授权信息版本不受支持".into());
        }
        let encrypted =
            hex::decode(stored.protected).map_err(|_| "百度授权密文无效".to_string())?;
        let plain = unprotect(&encrypted)?;
        serde_json::from_slice::<BaiduAuthorization>(&plain)
            .map_err(|_| "百度授权解密结果无效".to_string())?
    };
    validate(&authorization)?;
    Ok((authorization, plaintext))
}

fn validate(authorization: &BaiduAuthorization) -> Result<(), String> {
    if uuid::Uuid::parse_str(&authorization.revision).is_err() {
        return Err("百度授权修订信息无效".into());
    }
    if authorization
        .token
        .as_ref()
        .is_some_and(|token| token.access_token.trim().is_empty())
    {
        return Err("百度授权缺少 access token，请重新授权".into());
    }
    Ok(())
}

fn remove_plaintext_residue(path: &Path) {
    let Some(parent) = path.parent() else { return };
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return;
    };
    let prefixes = [format!(".{name}.tmp-"), format!(".{name}.bak-")];
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let filename = entry.file_name();
        let filename = filename.to_string_lossy();
        if !prefixes.iter().any(|prefix| filename.starts_with(prefix)) {
            continue;
        }
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        if fs::read(entry.path())
            .ok()
            .is_some_and(|raw| decode(&raw, None).is_ok_and(|(_, plain)| plain))
        {
            if let Err(error) = fs::remove_file(entry.path()) {
                crate::logging::warn(format!("清理百度授权旧明文残留失败：{error}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> BaiduToken {
        BaiduToken {
            access_token: "fake-sensitive-access".into(),
            expires_at: Some(u64::MAX),
            refresh_token: Some("fake-sensitive-refresh".into()),
        }
    }

    #[test]
    fn token_is_encrypted_and_debug_does_not_reveal_secrets() {
        let root = crate::test_support::TempWorkspace::new("token-encrypted");
        let saved = BaiduTokenRepository::update(&root, |_| {
            Ok(BaiduAuthorization::new(
                Some(token()),
                Some("config-a".into()),
            ))
        })
        .unwrap();
        let bytes = fs::read_to_string(BaiduTokenRepository::path(&root)).unwrap();
        assert!(!bytes.contains("fake-sensitive"));
        assert!(!format!("{:?}", token()).contains("fake-sensitive"));
        let loaded = BaiduTokenRepository::load(&root, None).unwrap().unwrap();
        assert_eq!(loaded.revision, saved.revision);
        assert_eq!(loaded.token.unwrap().access_token, token().access_token);
    }

    #[test]
    fn plaintext_current_token_is_migrated_once_and_residue_removed() {
        let root = crate::test_support::TempWorkspace::new("token-migrate");
        let path = BaiduTokenRepository::path(&root);
        let raw = serde_json::to_vec(&token()).unwrap();
        fs::write(&path, &raw).unwrap();
        let residue = root.join(format!(".{TOKEN_FILE}.bak-old"));
        fs::write(&residue, &raw).unwrap();
        let first = BaiduTokenRepository::load(&root, Some("config-a"))
            .unwrap()
            .unwrap();
        let second = BaiduTokenRepository::load(&root, Some("config-a"))
            .unwrap()
            .unwrap();
        assert_eq!(first.revision, second.revision);
        assert_eq!(second.configuration_id.as_deref(), Some("config-a"));
        assert!(!residue.exists());
        assert!(!fs::read_to_string(&path)
            .unwrap()
            .contains("fake-sensitive"));
    }

    #[test]
    fn missing_primary_recovers_encrypted_backup_without_legacy_lookup() {
        let root = crate::test_support::TempWorkspace::new("token-recover");
        let app = root.join("app-data");
        let saved = BaiduTokenRepository::update(&app, |_| {
            Ok(BaiduAuthorization::new(Some(token()), None))
        })
        .unwrap();
        let path = BaiduTokenRepository::path(&app);
        let backup = app.join(format!(".{TOKEN_FILE}.bak-crash"));
        fs::rename(&path, &backup).unwrap();
        let legacy = root.join("com.gamesaver.desktop");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join(TOKEN_FILE), b"not-current-account").unwrap();
        let loaded = BaiduTokenRepository::load(&app, None).unwrap().unwrap();
        assert_eq!(loaded.revision, saved.revision);
        assert!(path.is_file());
        assert_eq!(
            fs::read(legacy.join(TOKEN_FILE)).unwrap(),
            b"not-current-account"
        );
    }

    #[test]
    fn no_current_token_never_uses_another_projects_token() {
        let root = crate::test_support::TempWorkspace::new("token-no-legacy");
        let legacy = root.join("com.gamesaver.desktop");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(
            legacy.join(TOKEN_FILE),
            serde_json::to_vec(&token()).unwrap(),
        )
        .unwrap();
        assert!(BaiduTokenRepository::load(&root.join("app-data"), None)
            .unwrap()
            .is_none());
    }

    #[test]
    fn invalidation_persists_and_changes_the_session_revision() {
        let root = crate::test_support::TempWorkspace::new("token-invalidate");
        let before = BaiduTokenRepository::update(&root, |_| {
            Ok(BaiduAuthorization::new(Some(token()), None))
        })
        .unwrap();
        BaiduTokenRepository::invalidate(&root).unwrap();
        let after = BaiduTokenRepository::load(&root, None).unwrap().unwrap();
        assert!(after.token.is_none());
        assert_ne!(before.revision, after.revision);
    }

    #[test]
    fn invalid_residue_is_preserved_and_reported() {
        let root = crate::test_support::TempWorkspace::new("token-bad-residue");
        let residue = root.join(format!(".{TOKEN_FILE}.bak-broken"));
        fs::write(&residue, b"broken").unwrap();
        assert!(BaiduTokenRepository::load(&root, None).is_err());
        assert!(residue.is_file());
    }
}
