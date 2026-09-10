use crate::domain::{store::CURRENT_SCHEMA_VERSION, AppStore};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, sync::Mutex};
use tauri::{AppHandle, Manager};

use super::store_file::{atomic_replace, load_with_recovery};

const STORE_LABEL: &str = "游戏库数据";

/// 上一次落盘内容的哈希，用于在内容未变化时跳过整块重写。
static LAST_PERSISTED_HASH: Mutex<Option<[u8; 32]>> = Mutex::new(None);

pub struct GameRepository;

impl GameRepository {
    pub fn store_path(app: &AppHandle) -> Result<PathBuf, String> {
        Ok(app
            .path()
            .app_data_dir()
            .map_err(|err| format!("解析 GameSaver 数据目录失败：{err}"))?
            .join("store.json"))
    }

    /// 读取游戏库。
    ///
    /// 主文件缺失并不等于「首次运行」：上一次落盘可能崩在「原文件已改名、新文件
    /// 尚未到位」之间。此时把「读不到」当成「没有」，下一次写入就会用空库覆盖现场。
    /// 这里先交给 [`load_with_recovery`] 尝试从崩溃残留中恢复，只有确认确实没有
    /// 任何残留时才返回空库。
    pub fn load(app: &AppHandle) -> Result<AppStore, String> {
        let path = Self::store_path(app)?;
        let Some(bytes) = load_with_recovery(&path, STORE_LABEL, is_parseable_store)? else {
            return Ok(AppStore::default());
        };
        let mut store = serde_json::from_slice::<AppStore>(&bytes)
            .map_err(|err| format!("解析{STORE_LABEL}失败：{err}"))?;
        if store.schema_version > CURRENT_SCHEMA_VERSION {
            return Err(format!(
                "{STORE_LABEL}版本不受支持：{}（当前支持到 {CURRENT_SCHEMA_VERSION}）",
                store.schema_version
            ));
        }
        let needs_migration = store.schema_version < CURRENT_SCHEMA_VERSION;
        store.normalize();
        if needs_migration {
            Self::persist(app, &store)?;
        } else {
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            if let Ok(mut guard) = LAST_PERSISTED_HASH.lock() {
                *guard = Some(hasher.finalize().into());
            }
        }
        Ok(store)
    }

    pub fn persist(app: &AppHandle, store: &AppStore) -> Result<(), String> {
        let path = Self::store_path(app)?;
        let mut candidate = store.clone();
        candidate.normalize();
        let bytes = serde_json::to_vec_pretty(&candidate)
            .map_err(|err| format!("序列化{STORE_LABEL}失败：{err}"))?;

        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let hash: [u8; 32] = hasher.finalize().into();

        if let Ok(mut guard) = LAST_PERSISTED_HASH.lock() {
            if guard.as_ref() == Some(&hash) && path.exists() {
                return Ok(());
            }
            atomic_replace(&path, &bytes, STORE_LABEL)?;
            *guard = Some(hash);
            return Ok(());
        }

        atomic_replace(&path, &bytes, STORE_LABEL)
    }
}

/// 恢复候选的准入条件：能反序列化成 `AppStore` 即可。
///
/// 刻意不在校验里重复 `CURRENT_SCHEMA_VERSION` 判断——版本检查留在读取主流程，
/// 免得「残留内容版本偏新」被静默换成「残留内容版本偏旧」。
fn is_parseable_store(bytes: &[u8]) -> bool {
    serde_json::from_slice::<AppStore>(bytes).is_ok()
}
