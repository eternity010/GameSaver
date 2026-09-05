use std::{
    fs,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

pub const LARGE_SOURCE_WARNING_PREFIX: &str = "LARGE_SOURCE_REQUIRED:";
pub const LARGE_SOURCE_WARNING_BYTES: u64 = 3 * 1024 * 1024 * 1024;

pub struct AddGameService;

pub struct CopyResult {
    pub managed_path: PathBuf,
    pub executable_relative_path: String,
    pub file_count: usize,
    pub total_bytes: u64,
}

impl AddGameService {
    pub fn cleanup_copy_artifacts(games_root: &Path, game_uid: &str) -> Result<(), String> {
        let managed_path = games_root.join(game_uid);
        let staging_path = games_root.join(format!(".{game_uid}.copying"));
        for path in [managed_path, staging_path] {
            if path.exists() {
                fs::remove_dir_all(&path)
                    .map_err(|err| format!("清理失败的游戏本体失败：{}：{err}", path.display()))?;
            }
        }
        Ok(())
    }

    pub fn validate_source(source: &Path, executable: &Path) -> Result<String, String> {
        if !source.is_dir() {
            return Err("游戏本体目录不存在或不可访问".to_string());
        }
        if !executable.is_file() {
            return Err("启动程序不存在或不可访问".to_string());
        }
        let source = source
            .canonicalize()
            .map_err(|err| format!("解析游戏目录失败：{err}"))?;
        let executable = executable
            .canonicalize()
            .map_err(|err| format!("解析启动程序失败：{err}"))?;
        let relative = executable
            .strip_prefix(&source)
            .map_err(|_| "启动程序必须位于游戏本体目录内".to_string())?;
        if relative.as_os_str().is_empty() {
            return Err("启动程序路径无效".to_string());
        }
        Ok(relative.to_string_lossy().replace('\\', "/"))
    }

    pub fn copy_to_managed(
        source: &Path,
        games_root: &Path,
        game_uid: &str,
        executable_relative_path: String,
        allow_large_source: bool,
        on_progress: impl Fn(u8, &str),
        is_cancelled: impl Fn() -> bool,
    ) -> Result<CopyResult, String> {
        let source = source
            .canonicalize()
            .map_err(|err| format!("解析游戏目录失败：{err}"))?;
        fs::create_dir_all(games_root).map_err(|err| format!("创建游戏库目录失败：{err}"))?;
        let managed_path = games_root.join(game_uid);
        let staging_path = games_root.join(format!(".{game_uid}.copying"));
        if managed_path.exists() {
            return Err("受管游戏目录已存在".to_string());
        }
        if staging_path.exists() {
            fs::remove_dir_all(&staging_path)
                .map_err(|err| format!("清理上次未完成复制失败：{err}"))?;
        }

        let mut files = Vec::new();
        let mut total_bytes = 0u64;
        for entry in WalkDir::new(&source).follow_links(false) {
            let entry = entry.map_err(|err| format!("扫描游戏目录失败：{err}"))?;
            if entry.file_type().is_symlink() {
                return Err(format!(
                    "游戏目录包含不支持的符号链接：{}",
                    entry.path().display()
                ));
            }
            if entry.file_type().is_file() {
                let metadata = entry
                    .metadata()
                    .map_err(|err| format!("读取游戏文件信息失败：{err}"))?;
                total_bytes = total_bytes.saturating_add(metadata.len());
                files.push((entry.path().to_path_buf(), metadata.len()));
            }
        }
        if files.is_empty() {
            return Err("游戏目录中没有可复制的文件".to_string());
        }
        if needs_large_source_confirmation(total_bytes, allow_large_source) {
            return Err(format!(
                "{LARGE_SOURCE_WARNING_PREFIX}游戏本体大小为 {}，超过 3 GB。请确认游戏大小是否正常；确认后将继续复制。",
                format_size(total_bytes)
            ));
        }
        ensure_available_space(games_root, total_bytes)?;
        on_progress(5, &format!("已扫描 {} 个文件", files.len()));
        fs::create_dir_all(&staging_path).map_err(|err| format!("创建游戏暂存目录失败：{err}"))?;

        let result = (|| -> Result<CopyResult, String> {
            for (index, (source_file, _)) in files.iter().enumerate() {
                if is_cancelled() {
                    return Err("任务已取消".to_string());
                }
                let relative = source_file
                    .strip_prefix(&source)
                    .map_err(|err| format!("计算游戏文件相对路径失败：{err}"))?;
                let target = staging_path.join(relative);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|err| format!("创建游戏文件目录失败：{err}"))?;
                }
                fs::copy(source_file, &target)
                    .map_err(|err| format!("复制游戏文件失败（{}）：{err}", relative.display()))?;
                let progress = 5 + (((index + 1) * 90) / files.len()) as u8;
                on_progress(
                    progress.min(95),
                    &format!("正在复制游戏文件 {}/{}", index + 1, files.len()),
                );
            }
            if is_cancelled() {
                return Err("任务已取消".to_string());
            }
            fs::rename(&staging_path, &managed_path)
                .map_err(|err| format!("提交受管游戏目录失败：{err}"))?;
            on_progress(100, &format!("已复制 {} 个文件", files.len()));
            Ok(CopyResult {
                managed_path,
                executable_relative_path,
                file_count: files.len(),
                total_bytes,
            })
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&staging_path);
        }
        result
    }
}

fn ensure_available_space(target_root: &Path, required_bytes: u64) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let mut probe = target_root.to_path_buf();
        while !probe.exists() {
            if !probe.pop() {
                break;
            }
        }
        let wide = probe
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut available = 0u64;
        let result = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut available,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if result == 0 {
            return Err("无法确认 GameSaver 游戏库所在磁盘的可用空间".to_string());
        }
        let required_with_headroom = required_bytes.saturating_add(128 * 1024 * 1024);
        if available < required_with_headroom {
            return Err(format!(
                "游戏库磁盘空间不足，需要至少 {} MB，可用 {} MB",
                required_with_headroom / 1024 / 1024,
                available / 1024 / 1024
            ));
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = (target_root, required_bytes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{needs_large_source_confirmation, AddGameService};
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn copies_game_and_preserves_executable_relative_path() {
        let root = std::env::temp_dir().join(format!("gamesaver-next-test-{}", Uuid::new_v4()));
        let source = root.join("source");
        let games_root = root.join("games");
        fs::create_dir_all(source.join("bin")).expect("create source");
        fs::write(source.join("bin/game.exe"), b"exe").expect("write exe");
        fs::write(source.join("save.dat"), b"save").expect("write save");

        let relative = AddGameService::validate_source(&source, &source.join("bin/game.exe"))
            .expect("validate source");
        let result = AddGameService::copy_to_managed(
            &source,
            &games_root,
            "game-1",
            relative,
            false,
            |_, _| {},
            || false,
        )
        .expect("copy game");
        assert_eq!(result.executable_relative_path, "bin/game.exe");
        assert_eq!(
            fs::read(result.managed_path.join("save.dat")).expect("read copied file"),
            b"save"
        );
        fs::remove_dir_all(root).expect("cleanup test directory");
    }

    #[test]
    fn cancellation_removes_staging_directory() {
        let root = std::env::temp_dir().join(format!("gamesaver-next-cancel-{}", Uuid::new_v4()));
        let source = root.join("source");
        let games_root = root.join("games");
        fs::create_dir_all(&source).expect("create source");
        fs::write(source.join("game.exe"), b"exe").expect("write exe");
        let result = AddGameService::copy_to_managed(
            &source,
            &games_root,
            "game-2",
            "game.exe".to_string(),
            false,
            |_, _| {},
            || true,
        );
        assert!(result.is_err());
        assert!(!games_root.join("game-2").exists());
        assert!(!games_root.join(".game-2.copying").exists());
        fs::remove_dir_all(root).expect("cleanup test directory");
    }

    #[test]
    fn cleanup_removes_managed_and_staging_directories() {
        let root = std::env::temp_dir().join(format!("gamesaver-next-cleanup-{}", Uuid::new_v4()));
        let games_root = root.join("games");
        fs::create_dir_all(games_root.join("game-3/bin")).expect("create managed directory");
        fs::create_dir_all(games_root.join(".game-3.copying/bin"))
            .expect("create staging directory");
        fs::write(games_root.join("game-3/bin/game.exe"), b"exe").expect("write managed file");
        fs::write(games_root.join(".game-3.copying/bin/game.exe"), b"exe")
            .expect("write staging file");

        AddGameService::cleanup_copy_artifacts(&games_root, "game-3").expect("cleanup artifacts");

        assert!(!games_root.join("game-3").exists());
        assert!(!games_root.join(".game-3.copying").exists());
        fs::remove_dir_all(root).expect("cleanup test directory");
    }

    #[test]
    fn large_source_warning_uses_binary_three_gigabyte_boundary() {
        let boundary = 3 * 1024 * 1024 * 1024;
        assert!(!needs_large_source_confirmation(boundary, false));
        assert!(needs_large_source_confirmation(boundary + 1, false));
        assert!(!needs_large_source_confirmation(boundary + 1, true));
    }
}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn needs_large_source_confirmation(total_bytes: u64, allow_large_source: bool) -> bool {
    total_bytes > LARGE_SOURCE_WARNING_BYTES && !allow_large_source
}
