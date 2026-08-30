use crate::domain::{store::CURRENT_SCHEMA_VERSION, AppStore};
use std::{fs, io::Write, path::{Path, PathBuf}};
use tauri::{AppHandle, Manager};
use uuid::Uuid;

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
        let raw = fs::read_to_string(&path).map_err(|err| format!("read GameSaver store failed: {err}"))?;
        let mut store = serde_json::from_str::<AppStore>(&raw)
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
