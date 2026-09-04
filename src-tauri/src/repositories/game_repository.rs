use crate::domain::{store::CURRENT_SCHEMA_VERSION, AppStore};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};
use tauri::{AppHandle, Manager};
use uuid::Uuid;

static LAST_PERSISTED_HASH: Mutex<Option<[u8; 32]>> = Mutex::new(None);

pub struct GameRepository;

impl GameRepository {
    pub fn store_path(app: &AppHandle) -> Result<PathBuf, String> {
        Ok(app
            .path()
            .app_data_dir()
            .map_err(|err| format!("resolve GameSaver data directory failed: {err}"))?
            .join("store.json"))
    }

    pub fn load(app: &AppHandle) -> Result<AppStore, String> {
        let path = Self::store_path(app)?;
        if !path.exists() {
            return Ok(AppStore::default());
        }
        let bytes = fs::read(&path).map_err(|err| format!("read GameSaver store failed: {err}"))?;
        let mut store = serde_json::from_slice::<AppStore>(&bytes)
            .map_err(|err| format!("parse GameSaver store failed: {err}"))?;
        if store.schema_version > CURRENT_SCHEMA_VERSION {
            return Err(format!(
                "unsupported GameSaver store schema: {} (expected {})",
                store.schema_version, CURRENT_SCHEMA_VERSION
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
        let parent = path.parent().ok_or_else(|| "store path has no parent".to_string())?;
        fs::create_dir_all(parent).map_err(|err| format!("create GameSaver data directory failed: {err}"))?;
        let mut candidate = store.clone();
        candidate.normalize();
        let bytes = serde_json::to_vec_pretty(&candidate)
            .map_err(|err| format!("serialize GameSaver store failed: {err}"))?;

        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let hash: [u8; 32] = hasher.finalize().into();

        if let Ok(mut guard) = LAST_PERSISTED_HASH.lock() {
            if guard.as_ref() == Some(&hash) && path.exists() {
                return Ok(());
            }
            atomic_replace(&path, &bytes)?;
            *guard = Some(hash);
            return Ok(());
        }

        atomic_replace(&path, &bytes)
    }
}

fn atomic_replace(target: &Path, bytes: &[u8]) -> Result<(), String> {
    let temp = target.with_file_name(format!(".{}.tmp-{}", target.file_name().unwrap_or_default().to_string_lossy(), Uuid::new_v4().simple()));
    let backup = target.with_file_name(format!(".{}.bak-{}", target.file_name().unwrap_or_default().to_string_lossy(), Uuid::new_v4().simple()));
    let result = (|| -> Result<(), String> {
        let mut file = fs::File::create(&temp).map_err(|err| format!("create store temporary file failed: {err}"))?;
        file.write_all(bytes).map_err(|err| format!("write store temporary file failed: {err}"))?;
        file.sync_all().map_err(|err| format!("flush store temporary file failed: {err}"))?;
        drop(file);
        let had_target = target.exists();
        if had_target {
            fs::rename(target, &backup).map_err(|err| format!("stage existing store failed: {err}"))?;
        }
        if let Err(err) = fs::rename(&temp, target) {
            if had_target {
                let _ = fs::rename(&backup, target);
            }
            return Err(format!("commit GameSaver store failed: {err}"));
        }
        if had_target {
            let _ = fs::remove_file(&backup);
        }
        Ok(())
    })();
    let _ = fs::remove_file(&temp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_replace_creates_and_overwrites() {
        let dir = std::env::temp_dir().join(format!("gamesaver-test-{}", Uuid::new_v4().simple()));
        let _ = fs::create_dir_all(&dir);
        let file = dir.join("test_store.json");

        atomic_replace(&file, b"{\"test\": 1}").expect("initial create");
        assert_eq!(fs::read_to_string(&file).expect("read 1"), "{\"test\": 1}");

        atomic_replace(&file, b"{\"test\": 2}").expect("overwrite");
        assert_eq!(fs::read_to_string(&file).expect("read 2"), "{\"test\": 2}");

        let _ = fs::remove_dir_all(&dir);
    }
}
