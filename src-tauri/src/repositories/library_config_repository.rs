use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::store_file::{atomic_replace, load_with_recovery};

const CONFIG_FILE: &str = "library-config.json";
const CONFIG_LABEL: &str = "游戏库配置";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryConfig {
    #[serde(default)]
    pub library_root: Option<String>,
}

pub struct LibraryConfigRepository;

impl LibraryConfigRepository {
    pub fn path(app_data_dir: &Path) -> PathBuf {
        app_data_dir.join(CONFIG_FILE)
    }

    pub fn load(app_data_dir: &Path) -> Result<LibraryConfig, String> {
        let path = Self::path(app_data_dir);
        let Some(raw) = load_with_recovery(&path, CONFIG_LABEL, |bytes: &[u8]| {
            serde_json::from_slice::<LibraryConfig>(bytes).is_ok()
        })?
        else {
            return Ok(LibraryConfig::default());
        };
        serde_json::from_slice(&raw).map_err(|error| format!("解析{CONFIG_LABEL}失败：{error}"))
    }

    pub fn save(app_data_dir: &Path, config: &LibraryConfig) -> Result<(), String> {
        let path = Self::path(app_data_dir);
        let bytes = serde_json::to_vec_pretty(config)
            .map_err(|error| format!("序列化{CONFIG_LABEL}失败：{error}"))?;
        atomic_replace(&path, &bytes, CONFIG_LABEL)
    }

    pub fn resolve_root(app_data_dir: &Path) -> Result<PathBuf, String> {
        let config = Self::load(app_data_dir)?;
        let root = config
            .library_root
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| app_data_dir.to_path_buf());
        if !root.is_absolute() {
            return Err("游戏库根目录必须是绝对路径".to_string());
        }
        Ok(root)
    }
}

#[cfg(test)]
mod tests {
    use super::{LibraryConfig, LibraryConfigRepository};
    use std::path::Path;

    #[test]
    fn missing_config_uses_app_data_as_legacy_root() {
        let root = Path::new(r"C:\Users\tester\AppData\Roaming\com.gamesaver.next");
        assert_eq!(LibraryConfigRepository::resolve_root(root).unwrap(), root);
    }

    #[test]
    fn configured_root_is_loaded_from_config() {
        let root =
            std::env::temp_dir().join(format!("gamesaver-library-config-{}", uuid::Uuid::new_v4()));
        LibraryConfigRepository::save(
            &root,
            &LibraryConfig {
                library_root: Some(r"E:\GameSaverLibrary".to_string()),
            },
        )
        .unwrap();
        assert_eq!(
            LibraryConfigRepository::resolve_root(&root).unwrap(),
            Path::new(r"E:\GameSaverLibrary")
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
