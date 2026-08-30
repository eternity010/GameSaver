use crate::domain::{Game, GameBodyVersion};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fs, path::{Path, PathBuf}};
use uuid::Uuid;
use walkdir::WalkDir;

pub struct GameBodyUpdateService;

pub struct UpdatePlan {
    pub source: PathBuf,
    pub executable_relative_path: String,
    pub file_count: usize,
    pub total_bytes: u64,
}

pub struct BodySwap {
    pub archive_path: PathBuf,
    pub failed_path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct UpdateJournal {
    managed_path: String,
    staging_path: String,
    archive_path: String,
}

impl GameBodyUpdateService {
    pub fn journal_path(games_root: &Path, game_uid: &str) -> PathBuf {
        games_root.join(format!(".{game_uid}.update.json"))
    }

    pub fn write_journal(
        games_root: &Path,
        game_uid: &str,
        managed_path: &Path,
        staging_path: &Path,
        archive_path: &Path,
    ) -> Result<PathBuf, String> {
        let path = Self::journal_path(games_root, game_uid);
        let journal = UpdateJournal {
            managed_path: managed_path.to_string_lossy().to_string(),
            staging_path: staging_path.to_string_lossy().to_string(),
            archive_path: archive_path.to_string_lossy().to_string(),
        };
        let bytes = serde_json::to_vec(&journal).map_err(|err| format!("序列化游戏更新日志失败：{err}"))?;
        atomic_write(&path, &bytes)?;
        Ok(path)
    }

    pub fn clear_journal(path: &Path) -> Result<(), String> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("清理游戏更新日志失败：{error}")),
        }
    }

    pub fn recover_pending_updates(games_root: &Path, committed_archives: &HashSet<String>) -> Result<(), String> {
        if !games_root.is_dir() {
            return Ok(());
        }
        let mut errors = Vec::new();
        for entry in fs::read_dir(games_root).map_err(|err| format!("读取游戏更新日志失败：{err}"))? {
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(error) => {
                    errors.push(format!("读取游戏更新日志失败：{error}"));
                    continue;
                }
            };
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else { continue };
            if !name.starts_with('.') || !name.ends_with(".update.json") {
                continue;
            }
            if let Err(error) = recover_journal(games_root, &path, committed_archives) {
                errors.push(error);
            }
        }
        if let Err(error) = cleanup_stale_update_artifacts(games_root) {
            errors.push(error);
        }
        if errors.is_empty() { Ok(()) } else { Err(errors.join("；")) }
    }

    pub fn cleanup_removed_restore_archives(games_root: &Path) -> Result<Vec<PathBuf>, String> {
        let versions_root = games_root.join(".versions");
        if !versions_root.is_dir() {
            return Ok(Vec::new());
        }
        let mut removed = Vec::new();
        for game_entry in fs::read_dir(&versions_root).map_err(|err| format!("读取历史本体目录失败：{err}"))? {
            let game_path = game_entry.map_err(|err| format!("读取历史本体目录失败：{err}"))?.path();
            if !game_path.is_dir() {
                continue;
            }
            for archive_entry in fs::read_dir(&game_path).map_err(|err| format!("读取历史本体版本失败：{err}"))? {
                let archive_path = archive_entry.map_err(|err| format!("读取历史本体版本失败：{err}"))?.path();
                let Some(name) = archive_path.file_name().and_then(|value| value.to_str()) else { continue };
                if name.starts_with("restore-") && archive_path.is_dir() {
                    fs::remove_dir_all(&archive_path).map_err(|err| format!("清理历史本体恢复副本失败：{err}"))?;
                    removed.push(archive_path);
                }
            }
            if fs::read_dir(&game_path).map_err(|err| format!("检查历史本体目录失败：{err}"))?.next().is_none() {
                fs::remove_dir(&game_path).map_err(|err| format!("清理空历史本体目录失败：{err}"))?;
            }
        }
        Ok(removed)
    }

    pub fn cleanup_archived_body_versions(
        games_root: &Path,
        versions: &mut Vec<GameBodyVersion>,
    ) -> Result<usize, String> {
        let mut removed = 0;
        for version in versions.iter_mut() {
            if version.archive_path.trim().is_empty() {
                continue;
            }
            let archive = PathBuf::from(&version.archive_path);
            if !path_is_within(&archive, games_root) || archive == games_root {
                return Err(format!("历史本体目录超出游戏库：{}", archive.display()));
            }
            if archive.is_dir() {
                fs::remove_dir_all(&archive)
                    .map_err(|err| format!("清理历史游戏本体失败：{err}"))?;
                removed += 1;
            } else if archive.is_file() {
                fs::remove_file(&archive)
                    .map_err(|err| format!("清理历史游戏本体文件失败：{err}"))?;
                removed += 1;
            }
            version.archive_path.clear();
        }
        versions.retain(|version| {
            version.package_path.is_some() || !version.archive_path.trim().is_empty()
        });
        Ok(removed)
    }

    pub fn validate_source(source: &Path, game: &Game) -> Result<UpdatePlan, String> {
        let source = source.canonicalize().map_err(|err| format!("解析新版游戏目录失败：{err}"))?;
        let managed = Path::new(&game.managed_path);
        if paths_overlap(&source, &managed) {
            return Err("新版游戏目录不能是当前受管游戏目录，或位于其内部".to_string());
        }
        if !source.is_dir() {
            return Err("新版游戏目录不存在或不可访问".to_string());
        }
        let relative = validate_relative(&game.launch.executable_relative_path)?;
        if !source.join(&relative).is_file() {
            return Err(format!("新版游戏目录中找不到启动程序：{}", relative));
        }
        let mut file_count = 0usize;
        let mut total_bytes = 0u64;
        for entry in WalkDir::new(&source).follow_links(false) {
            let entry = entry.map_err(|err| format!("扫描新版游戏目录失败：{err}"))?;
            if entry.file_type().is_symlink() {
                return Err(format!("新版游戏目录包含不支持的符号链接：{}", entry.path().display()));
            }
            if entry.file_type().is_file() {
                let metadata = entry.metadata().map_err(|err| format!("读取新版游戏文件信息失败：{err}"))?;
                file_count += 1;
                total_bytes = total_bytes.saturating_add(metadata.len());
            }
        }
        if file_count == 0 {
            return Err("新版游戏目录中没有可复制的文件".to_string());
        }
        ensure_available_space(managed, total_bytes)?;
        Ok(UpdatePlan { source, executable_relative_path: relative, file_count, total_bytes })
    }

    pub fn copy_to_staging(
        plan: &UpdatePlan,
        games_root: &Path,
        game_uid: &str,
        on_progress: impl Fn(u8, &str),
        is_cancelled: impl Fn() -> bool,
    ) -> Result<PathBuf, String> {
        let staging = games_root.join(format!(".{game_uid}.updating"));
        if staging.exists() {
            fs::remove_dir_all(&staging).map_err(|err| format!("清理上次未完成更新失败：{err}"))?;
        }
        fs::create_dir_all(&staging).map_err(|err| format!("创建游戏更新暂存目录失败：{err}"))?;
        let result = (|| -> Result<PathBuf, String> {
            let mut file_index = 0usize;
            for entry in WalkDir::new(&plan.source).follow_links(false) {
                if is_cancelled() { return Err("任务已取消".to_string()); }
                let entry = entry.map_err(|err| format!("扫描新版游戏目录失败：{err}"))?;
                if !entry.file_type().is_file() { continue; }
                file_index += 1;
                let relative = entry.path().strip_prefix(&plan.source).map_err(|err| format!("计算新版游戏相对路径失败：{err}"))?;
                let target = staging.join(relative);
                if let Some(parent) = target.parent() { fs::create_dir_all(parent).map_err(|err| format!("创建游戏更新目录失败：{err}"))?; }
                fs::copy(entry.path(), &target).map_err(|err| format!("复制新版游戏文件失败（{}）：{err}", relative.display()))?;
                on_progress(10 + ((file_index * 80) / plan.file_count.max(1)) as u8, &format!("正在复制新版游戏文件 {}/{}", file_index, plan.file_count));
            }
            if is_cancelled() { return Err("任务已取消".to_string()); }
            if !staging.join(&plan.executable_relative_path).is_file() { return Err("新版游戏启动程序复制后不存在".to_string()); }
            Ok(staging.clone())
        })();
        if result.is_err() { let _ = fs::remove_dir_all(&staging); }
        result
    }

    pub fn swap(managed_path: &Path, staging: &Path, archive_path: &Path) -> Result<BodySwap, String> {
        let failed_path = managed_path.with_file_name(format!(".{}.failed-{}", managed_path.file_name().unwrap_or_default().to_string_lossy(), Uuid::new_v4().simple()));
        if let Some(parent) = archive_path.parent() { fs::create_dir_all(parent).map_err(|err| format!("创建旧游戏版本目录失败：{err}"))?; }
        fs::rename(managed_path, archive_path).map_err(|err| format!("暂存当前游戏本体失败：{err}"))?;
        if let Err(error) = fs::rename(staging, managed_path) {
            let rollback = fs::rename(archive_path, managed_path);
            return if let Err(rollback_error) = rollback { Err(format!("提交新版游戏失败：{error}；恢复旧游戏本体失败：{rollback_error}")) } else { Err(format!("提交新版游戏失败：{error}")) };
        }
        Ok(BodySwap { archive_path: archive_path.to_path_buf(), failed_path })
    }

    pub fn rollback(managed_path: &Path, swap: &BodySwap) -> Result<(), String> {
        if !swap.archive_path.is_dir() { return Err(format!("旧游戏版本不存在：{}", swap.archive_path.display())); }
        if managed_path.exists() {
            fs::rename(managed_path, &swap.failed_path).map_err(|err| format!("暂存失败的新游戏本体失败：{err}"))?;
        }
        if let Err(error) = fs::rename(&swap.archive_path, managed_path) {
            if swap.failed_path.exists() { let _ = fs::rename(&swap.failed_path, managed_path); }
            return Err(format!("恢复旧游戏本体失败：{error}"));
        }
        fs::remove_dir_all(&swap.failed_path).map_err(|err| format!("清理失败的新游戏本体失败：{err}"))?;
        Ok(())
    }
}

fn recover_journal(games_root: &Path, journal_path: &Path, committed_archives: &HashSet<String>) -> Result<(), String> {
    let raw = fs::read(journal_path).map_err(|err| format!("读取游戏更新日志失败：{err}"))?;
    let journal = serde_json::from_slice::<UpdateJournal>(&raw).map_err(|err| format!("解析游戏更新日志失败：{err}"))?;
    let managed = PathBuf::from(journal.managed_path);
    let staging = PathBuf::from(journal.staging_path);
    let archive = PathBuf::from(journal.archive_path);
    for path in [&managed, &staging, &archive] {
        if !path_is_within(path, games_root) {
            return Err(format!("游戏更新日志路径超出游戏库：{}", path.display()));
        }
    }
    let committed = committed_archives.contains(&normalize_path(&archive));
    if committed {
        if managed.is_dir() {
            if staging.exists() {
                fs::remove_dir_all(&staging).map_err(|err| format!("清理未完成游戏更新暂存目录失败：{err}"))?;
            }
        } else if staging.is_dir() {
            fs::rename(&staging, &managed).map_err(|err| format!("恢复已提交的游戏本体失败：{err}"))?;
        } else {
            return Err("已提交的游戏本体更新缺少当前目录和暂存目录".to_string());
        }
    } else if archive.is_dir() {
        if staging.exists() {
            fs::remove_dir_all(&staging).map_err(|err| format!("清理未完成游戏更新暂存目录失败：{err}"))?;
        }
        if managed.is_dir() {
            let failed = managed.with_file_name(format!(".{}.recovery-{}", managed.file_name().unwrap_or_default().to_string_lossy(), Uuid::new_v4().simple()));
            fs::rename(&managed, &failed).map_err(|err| format!("暂存未提交的新游戏本体失败：{err}"))?;
            if let Err(error) = fs::rename(&archive, &managed) {
                let _ = fs::rename(&failed, &managed);
                return Err(format!("恢复中断前的游戏本体失败：{error}"));
            }
            fs::remove_dir_all(&failed).map_err(|err| format!("清理未提交的新游戏本体失败：{err}"))?;
        } else {
            fs::rename(&archive, &managed).map_err(|err| format!("恢复中断前的游戏本体失败：{err}"))?;
        }
    } else if managed.is_dir() {
        if staging.exists() {
            fs::remove_dir_all(&staging).map_err(|err| format!("清理未完成游戏更新暂存目录失败：{err}"))?;
        }
    } else if staging.is_dir() {
        fs::rename(&staging, &managed).map_err(|err| format!("恢复未完成的游戏本体更新失败：{err}"))?;
    } else {
        return Err("游戏更新中断，找不到可恢复的游戏本体或暂存目录".to_string());
    }
    GameBodyUpdateService::clear_journal(journal_path)
}

fn cleanup_stale_update_artifacts(games_root: &Path) -> Result<(), String> {
    for entry in fs::read_dir(games_root).map_err(|err| format!("读取游戏更新暂存目录失败：{err}"))? {
        let path = entry.map_err(|err| format!("读取游戏更新暂存目录失败：{err}"))?.path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else { continue };
        if !name.starts_with('.') || !path.is_dir() {
            continue;
        }
        if let Some(uid) = name.strip_prefix('.').and_then(|value| value.strip_suffix(".updating")) {
            if games_root.join(uid).is_dir() {
                fs::remove_dir_all(&path).map_err(|err| format!("清理残留游戏更新暂存目录失败：{err}"))?;
            } else {
                fs::remove_dir_all(&path).map_err(|err| format!("清理孤立游戏更新暂存目录失败：{err}"))?;
            }
        } else if let Some((base, _)) = name.strip_prefix('.').and_then(|value| value.split_once(".failed-")) {
            if games_root.join(base).is_dir() {
                fs::remove_dir_all(&path).map_err(|err| format!("清理残留游戏更新目录失败：{err}"))?;
            }
        } else if name.starts_with('.') && name.contains(".uninstalling-") {
            fs::remove_dir_all(&path).map_err(|err| format!("清理残留游戏卸载目录失败：{err}"))?;
        }
    }
    Ok(())
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    let path = normalize_path(path);
    let root = normalize_path(root);
    path == root || path.starts_with(&(root + "\\"))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temporary = path.with_file_name(format!(".{}.tmp-{}", path.file_name().unwrap_or_default().to_string_lossy(), Uuid::new_v4().simple()));
    let result = (|| -> Result<(), String> {
        let mut file = fs::File::create(&temporary).map_err(|err| format!("创建游戏更新日志临时文件失败：{err}"))?;
        std::io::Write::write_all(&mut file, bytes).map_err(|err| format!("写入游戏更新日志失败：{err}"))?;
        file.sync_all().map_err(|err| format!("刷新游戏更新日志失败：{err}"))?;
        fs::rename(&temporary, path).map_err(|err| format!("提交游戏更新日志失败：{err}"))?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result
}

fn validate_relative(value: &str) -> Result<String, String> {
    let path = Path::new(value);
    if path.is_absolute() || path.components().any(|component| matches!(component, std::path::Component::ParentDir)) { return Err("启动程序相对路径无效".to_string()); }
    Ok(value.replace('\\', "/").trim_matches('/').to_string())
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    let left = normalize_path(left);
    let right = normalize_path(right);
    left == right || left.starts_with(&(right.clone() + "\\")) || right.starts_with(&(left + "\\"))
}

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_ascii_lowercase()
}

fn ensure_available_space(target: &Path, required_bytes: u64) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let mut probe = target.to_path_buf();
        while !probe.exists() {
            if !probe.pop() { break; }
        }
        let wide = probe.as_os_str().encode_wide().chain(std::iter::once(0)).collect::<Vec<_>>();
        let mut available = 0u64;
        if unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, std::ptr::null_mut(), std::ptr::null_mut()) } == 0 { return Err("无法确认游戏库磁盘可用空间".to_string()); }
        let required = required_bytes.saturating_add(128 * 1024 * 1024);
        if available < required { return Err(format!("游戏库磁盘空间不足，需要至少 {} MB，可用 {} MB", required / 1024 / 1024, available / 1024 / 1024)); }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = (target, required_bytes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::GameBodyUpdateService;
    use crate::domain::{Game, GameBodyVersion};
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn update_source_must_contain_existing_executable() {
        let root = std::env::temp_dir().join(format!("gamesaver-update-{}", Uuid::new_v4()));
        let source = root.join("source");
        fs::create_dir_all(&source).expect("create source");
        let game = Game::new_pending("Game", root.join("managed").to_string_lossy(), "game.exe");
        assert!(GameBodyUpdateService::validate_source(&source, &game).is_err());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn swap_and_rollback_restore_the_original_directory() {
        let root = std::env::temp_dir().join(format!("gamesaver-swap-{}", Uuid::new_v4()));
        let managed = root.join("managed");
        let staging = root.join("staging");
        let archive = root.join("versions").join("old");
        fs::create_dir_all(&managed).expect("create managed");
        fs::create_dir_all(&staging).expect("create staging");
        fs::write(managed.join("state.txt"), b"old").expect("write old");
        fs::write(staging.join("state.txt"), b"new").expect("write new");
        let swap = GameBodyUpdateService::swap(&managed, &staging, &archive).expect("swap");
        assert_eq!(fs::read(managed.join("state.txt")).expect("read new"), b"new");
        GameBodyUpdateService::rollback(&managed, &swap).expect("rollback");
        assert_eq!(fs::read(managed.join("state.txt")).expect("read old"), b"old");
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn recovery_restores_original_directory_after_interrupted_swap() {
        let root = std::env::temp_dir().join(format!("gamesaver-recovery-{}", Uuid::new_v4()));
        let games_root = root.join("games");
        let managed = games_root.join("game-1");
        let staging = games_root.join(".game-1.updating");
        let archive = games_root.join(".versions").join("game-1").join("version-1");
        fs::create_dir_all(&archive).expect("create archive");
        fs::create_dir_all(&staging).expect("create staging");
        fs::write(archive.join("state.txt"), b"old").expect("write old");
        fs::write(staging.join("state.txt"), b"new").expect("write new");
        let journal = GameBodyUpdateService::write_journal(&games_root, "game-1", &managed, &staging, &archive).expect("write journal");

        GameBodyUpdateService::recover_pending_updates(&games_root, &std::collections::HashSet::new()).expect("recover update");

        assert_eq!(fs::read(managed.join("state.txt")).expect("read restored old"), b"old");
        assert!(!staging.exists());
        assert!(!archive.exists());
        assert!(!journal.exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn recovery_keeps_new_directory_after_committed_swap() {
        let root = std::env::temp_dir().join(format!("gamesaver-committed-recovery-{}", Uuid::new_v4()));
        let games_root = root.join("games");
        let managed = games_root.join("game-1");
        let staging = games_root.join(".game-1.updating");
        let archive = games_root.join(".versions").join("game-1").join("version-1");
        fs::create_dir_all(&managed).expect("create managed");
        fs::create_dir_all(&archive).expect("create archive");
        fs::write(managed.join("state.txt"), b"new").expect("write new");
        fs::write(archive.join("state.txt"), b"old").expect("write old");
        let journal = GameBodyUpdateService::write_journal(&games_root, "game-1", &managed, &staging, &archive).expect("write journal");
        let committed = std::collections::HashSet::from([archive.to_string_lossy().replace('/', "\\").to_ascii_lowercase()]);

        GameBodyUpdateService::recover_pending_updates(&games_root, &committed).expect("recover update");

        assert_eq!(fs::read(managed.join("state.txt")).expect("read new"), b"new");
        assert!(!journal.exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn cleanup_archived_body_versions_removes_full_old_copy() {
        let root = std::env::temp_dir().join(format!("gamesaver-cleanup-{}", Uuid::new_v4()));
        let games_root = root.join("games");
        let archive = games_root.join(".versions").join("game-1").join("version-1");
        fs::create_dir_all(&archive).expect("create archive");
        fs::write(archive.join("game.exe"), b"old body").expect("write archive");
        let mut versions = vec![GameBodyVersion {
            version_id: "version-1".to_string(),
            game_uid: "game-1".to_string(),
            created_at: "1".to_string(),
            archive_path: archive.to_string_lossy().to_string(),
            file_count: 1,
            total_bytes: 8,
            package_path: None,
            sha256: None,
            excluded_items: Vec::new(),
            upload_status: None,
            remote_path: None,
            remote_fs_id: None,
            remote_size: None,
        }];

        let removed = GameBodyUpdateService::cleanup_archived_body_versions(&games_root, &mut versions)
            .expect("cleanup archived body version");

        assert_eq!(removed, 1);
        assert!(!archive.exists());
        assert!(versions.is_empty());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn recovery_removes_orphan_update_staging_directory() {
        let root = std::env::temp_dir().join(format!("gamesaver-orphan-update-{}", Uuid::new_v4()));
        let games_root = root.join("games");
        let staging = games_root.join(".game-1.updating");
        fs::create_dir_all(&staging).expect("create staging");
        fs::write(staging.join("partial.bin"), b"partial").expect("write partial");

        GameBodyUpdateService::recover_pending_updates(&games_root, &std::collections::HashSet::new()).expect("recover update");

        assert!(!staging.exists());
        fs::remove_dir_all(root).expect("cleanup");
    }
}
