use serde::{Deserialize, Serialize};
use std::{fs, io::Write, path::{Path, PathBuf}};
use uuid::Uuid;

const CONFIG_FILE: &str = "library-config.json";

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
        if !path.is_file() {
            return Ok(LibraryConfig::default());
        }
        let raw = fs::read(&path).map_err(|error| format!("读取游戏库配置失败：{error}"))?;
        serde_json::from_slice(&raw).map_err(|error| format!("解析游戏库配置失败：{error}"))
    }

    pub fn save(app_data_dir: &Path, config: &LibraryConfig) -> Result<(), String> {
        let path = Self::path(app_data_dir);
        let parent = path.parent().ok_or_else(|| "游戏库配置路径缺少父目录".to_string())?;
        fs::create_dir_all(parent).map_err(|error| format!("创建游戏库配置目录失败：{error}"))?;
        let bytes = serde_json::to_vec_pretty(config).map_err(|error| format!("序列化游戏库配置失败：{error}"))?;
        let temporary = parent.join(format!(".{CONFIG_FILE}.tmp-{}", Uuid::new_v4().simple()));
        let result = (|| -> Result<(), String> {
            let mut file = fs::File::create(&temporary).map_err(|error| format!("创建游戏库配置临时文件失败：{error}"))?;
            file.write_all(&bytes).map_err(|error| format!("写入游戏库配置临时文件失败：{error}"))?;
            file.sync_all().map_err(|error| format!("刷新游戏库配置临时文件失败：{error}"))?;
            replace_file(&temporary, &path)
        })();
        let _ = fs::remove_file(&temporary);
        result
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

fn replace_file(source: &Path, target: &Path) -> Result<(), String> {
    if target.exists() {
        let backup = target.with_file_name(format!(".{}.bak-{}", target.file_name().unwrap_or_default().to_string_lossy(), Uuid::new_v4().simple()));
        fs::rename(target, &backup).map_err(|error| format!("暂存旧游戏库配置失败：{error}"))?;
        if let Err(error) = fs::rename(source, target) {
            let _ = fs::rename(&backup, target);
            return Err(format!("提交游戏库配置失败：{error}"));
        }
        let _ = fs::remove_file(backup);
        Ok(())
    } else {
        fs::rename(source, target).map_err(|error| format!("提交游戏库配置失败：{error}"))
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
        let root = std::env::temp_dir().join(format!("gamesaver-library-config-{}", uuid::Uuid::new_v4()));
        LibraryConfigRepository::save(&root, &LibraryConfig { library_root: Some(r"E:\GameSaverLibrary".to_string()) }).unwrap();
        assert_eq!(LibraryConfigRepository::resolve_root(&root).unwrap(), Path::new(r"E:\GameSaverLibrary"));
        let _ = std::fs::remove_dir_all(root);
    }
}
