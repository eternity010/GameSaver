use crate::domain::AppStore;
use std::{
    fs,
    path::Path,
};
use uuid::Uuid;
use walkdir::WalkDir;

#[derive(Debug, Clone, Default)]
pub struct LibraryUsage {
    pub games_bytes: u64,
    pub body_packages_bytes: u64,
    pub saves_bytes: u64,
    pub file_count: usize,
}

impl LibraryUsage {
    pub fn total_bytes(&self) -> u64 {
        self.games_bytes
            .saturating_add(self.body_packages_bytes)
            .saturating_add(self.saves_bytes)
    }
}

pub struct LibraryService;

impl LibraryService {
    pub fn usage(root: &Path) -> Result<LibraryUsage, String> {
        let mut usage = LibraryUsage::default();
        usage.games_bytes = collect_usage(&root.join("games"), &mut usage.file_count)?;
        usage.body_packages_bytes = collect_usage(&root.join("body-packages"), &mut usage.file_count)?;
        usage.saves_bytes = collect_usage(&root.join("saves"), &mut usage.file_count)?;
        Ok(usage)
    }

    pub fn validate_target(source: &Path, target: &Path) -> Result<(), String> {
        if !target.is_absolute() {
            return Err("游戏库根目录必须是绝对路径".to_string());
        }
        if same_path(source, target) || is_descendant(target, source) || is_descendant(source, target) {
            return Err("新游戏库目录不能与当前游戏库重叠".to_string());
        }
        if target.exists() && !target.is_dir() {
            return Err("新游戏库路径不是文件夹".to_string());
        }
        if target.is_dir() {
            let has_unmanaged_entries = fs::read_dir(target)
                .map_err(|error| format!("读取新游戏库目录失败：{error}"))?
                .next()
                .is_some();
            if has_unmanaged_entries {
                return Err("新游戏库目录必须为空，避免覆盖已有文件".to_string());
            }
        }
        Ok(())
    }

    pub fn migrate(
        source: &Path,
        target: &Path,
        store: &AppStore,
        mut on_progress: impl FnMut(u8, &str),
    ) -> Result<AppStore, String> {
        Self::validate_target(source, target)?;
        let usage = Self::usage(source)?;
        let parent = target
            .parent()
            .ok_or_else(|| "新游戏库路径缺少父目录".to_string())?;
        fs::create_dir_all(parent).map_err(|error| format!("创建新游戏库父目录失败：{error}"))?;
        let staging = parent.join(format!(".gamesaver-library-migration-{}", Uuid::new_v4().simple()));
        let result = (|| -> Result<AppStore, String> {
            fs::create_dir_all(&staging).map_err(|error| format!("创建游戏库迁移暂存目录失败：{error}"))?;
            for (index, directory) in ["games", "body-packages", "saves"].iter().enumerate() {
                let source_dir = source.join(directory);
                let target_dir = staging.join(directory);
                copy_tree(&source_dir, &target_dir, |message| {
                    let base = (index as u8) * 30;
                    on_progress(base.saturating_add(10), &message);
                })?;
                on_progress((index as u8 + 1) * 30, &format!("已复制 {directory}"));
            }
            let staged_usage = Self::usage(&staging)?;
            if staged_usage.total_bytes() != usage.total_bytes() || staged_usage.file_count != usage.file_count {
                return Err("游戏库迁移校验失败：文件数量或大小不一致".to_string());
            }
            let mut candidate = store.clone();
            rewrite_store_paths(&mut candidate, source, target)?;
            if target.is_dir() {
                fs::remove_dir(target).map_err(|error| format!("清理新游戏库空目录失败：{error}"))?;
            }
            fs::rename(&staging, target).map_err(|error| format!("提交新游戏库失败：{error}"))?;
            on_progress(90, "新游戏库已校验并提交");
            Ok(candidate)
        })();
        let _ = fs::remove_dir_all(&staging);
        result
    }

    pub fn cleanup_source(source: &Path) -> Result<(), String> {
        for directory in ["games", "body-packages", "saves"] {
            let path = source.join(directory);
            if path.is_dir() {
                fs::remove_dir_all(&path).map_err(|error| format!("清理旧游戏库目录失败：{}：{error}", path.display()))?;
            }
        }
        Ok(())
    }
}

fn collect_usage(root: &Path, file_count: &mut usize) -> Result<u64, String> {
    if !root.is_dir() {
        return Ok(0);
    }
    let mut total = 0u64;
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| format!("扫描游戏库失败：{error}"))?;
        if entry.file_type().is_file() {
            *file_count = file_count.saturating_add(1);
            total = total.saturating_add(
                entry
                    .metadata()
                    .map_err(|error| format!("读取游戏库文件信息失败：{error}"))?
                    .len(),
            );
        }
    }
    Ok(total)
}

fn copy_tree(source: &Path, target: &Path, mut on_file: impl FnMut(String)) -> Result<(), String> {
    if !source.exists() {
        return Ok(());
    }
    for entry in WalkDir::new(source).follow_links(false) {
        let entry = entry.map_err(|error| format!("扫描待迁移数据失败：{error}"))?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|error| format!("计算迁移相对路径失败：{error}"))?;
        let destination = target.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&destination).map_err(|error| format!("创建迁移目录失败：{error}"))?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|error| format!("创建迁移文件目录失败：{error}"))?;
            }
            fs::copy(entry.path(), &destination).map_err(|error| format!("复制游戏库文件失败：{error}"))?;
            on_file(format!("正在迁移 {}", relative.display()));
        }
    }
    Ok(())
}

fn rewrite_store_paths(store: &mut AppStore, source: &Path, target: &Path) -> Result<(), String> {
    for game in &mut store.games {
        game.managed_path = rewrite_path(&game.managed_path, source, target)?;
    }
    for version in &mut store.body_versions {
        version.archive_path = rewrite_path(&version.archive_path, source, target)?;
        if let Some(path) = version.package_path.as_mut() {
            *path = rewrite_path(path, source, target)?;
        }
    }
    for profile in &mut store.save_profiles {
        for scope in &mut profile.scopes {
            scope.root_path = rewrite_path(&scope.root_path, source, target)?;
        }
    }
    for version in &mut store.save_versions {
        for file in &mut version.files {
            if let Some(path) = file.root_path.as_mut() {
                *path = rewrite_path(path, source, target)?;
            }
        }
    }
    Ok(())
}

fn rewrite_path(value: &str, source: &Path, target: &Path) -> Result<String, String> {
    let value = value.replace('/', "\\");
    let source = normalized(source);
    let target = target.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_string();
    if value.to_ascii_lowercase() == source {
        return Ok(target);
    }
    let prefix = format!("{source}\\");
    if !value.to_ascii_lowercase().starts_with(&prefix) {
        return Ok(value.to_string());
    }
    Ok(format!("{}\\{}", target, &value[prefix.len()..]))
}

fn normalized(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_ascii_lowercase()
}

fn same_path(left: &Path, right: &Path) -> bool {
    normalized(left) == normalized(right)
}

fn is_descendant(path: &Path, parent: &Path) -> bool {
    let path = normalized(path);
    let parent = normalized(parent);
    path.starts_with(&(parent + "\\"))
}

#[cfg(test)]
mod tests {
    use super::LibraryService;
    use std::path::Path;

    #[test]
    fn rejects_overlapping_library_roots() {
        assert!(LibraryService::validate_target(Path::new(r"E:\Games"), Path::new(r"E:\Games\new")).is_err());
        assert!(LibraryService::validate_target(Path::new(r"E:\Games"), Path::new(r"E:\Games")).is_err());
    }
}
