use crate::{
    app_state::AppState,
    domain::TaskStatus,
    repositories::{GameRepository, LibraryConfig, LibraryConfigRepository},
    services::{LibraryService, TaskService},
};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager, State};

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibrarySettingsView {
    pub library_root: String,
    pub games_path: String,
    pub body_packages_path: String,
    pub saves_path: String,
    pub games_bytes: u64,
    pub body_packages_bytes: u64,
    pub saves_bytes: u64,
    pub total_bytes: u64,
    pub file_count: usize,
    pub free_bytes: u64,
}

#[tauri::command]
pub fn get_library_settings(state: State<AppState>) -> Result<LibrarySettingsView, String> {
    let root = state.library_root_path()?;
    let usage = LibraryService::usage(&root)?;
    Ok(LibrarySettingsView {
        library_root: root.to_string_lossy().to_string(),
        games_path: root.join("games").to_string_lossy().to_string(),
        body_packages_path: root.join("body-packages").to_string_lossy().to_string(),
        saves_path: root.join("saves").to_string_lossy().to_string(),
        total_bytes: usage.total_bytes(),
        games_bytes: usage.games_bytes,
        body_packages_bytes: usage.body_packages_bytes,
        saves_bytes: usage.saves_bytes,
        file_count: usage.file_count,
        free_bytes: disk_free_bytes(&root),
    })
}

#[tauri::command]
pub fn start_set_library_root_task(
    app: AppHandle,
    state: State<AppState>,
    target_root: String,
) -> Result<String, String> {
    let target = PathBuf::from(target_root.trim());
    let source = state.library_root_path()?;
    if same_path(&source, &target) {
        return Err("游戏库根目录没有变化".to_string());
    }
    if state
        .running_games
        .lock()
        .map_err(|_| "读取运行中游戏状态失败".to_string())?
        .values()
        .next()
        .is_some()
    {
        return Err("游戏运行中，暂时不能迁移游戏库".to_string());
    }
    if !state
        .save_operations
        .lock()
        .map_err(|_| "读取存档操作状态失败".to_string())?
        .is_empty()
    {
        return Err("存档操作进行中，暂时不能迁移游戏库".to_string());
    }
    LibraryService::validate_target(&source, &target)?;
    let store = state
        .store
        .lock()
        .map_err(|_| "读取游戏库记录失败".to_string())?
        .clone();
    let usage = LibraryService::usage(&source)?;
    let free_bytes = disk_free_bytes(&target);
    if free_bytes > 0 && usage.total_bytes() > free_bytes {
        return Err(format!(
            "新游戏库所在磁盘空间不足：需要 {}，可用 {}",
            format_bytes(usage.total_bytes()),
            format_bytes(free_bytes)
        ));
    }
    {
        let mut migrating = state
            .library_migration
            .lock()
            .map_err(|_| "读取游戏库迁移状态失败".to_string())?;
        if *migrating {
            return Err("已有游戏库迁移任务正在进行".to_string());
        }
        *migrating = true;
    }
    let task_id = match TaskService::create(&state, "set_library_root", None, "准备迁移游戏库") {
        Ok(task_id) => task_id,
        Err(error) => {
            if let Ok(mut migrating) = state.library_migration.lock() {
                *migrating = false;
            }
            return Err(error);
        }
    };
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        let result = migrate_library(&app, &task_id_for_thread, &source, &target, &store);
        if let Ok(mut migrating) = app.state::<AppState>().library_migration.lock() {
            *migrating = false;
        }
        match result {
            Ok(()) => TaskService::finish(&app.state(), &task_id_for_thread, TaskStatus::Success, 100, "游戏库迁移完成", None, None),
            Err(error) if error == "任务已取消" => TaskService::finish(&app.state(), &task_id_for_thread, TaskStatus::Cancelled, 100, "已取消游戏库迁移", None, None),
            Err(error) => TaskService::finish(&app.state(), &task_id_for_thread, TaskStatus::Failed, 100, "游戏库迁移失败", None, Some(error)),
        }
    });
    Ok(task_id)
}

fn migrate_library(
    app: &AppHandle,
    task_id: &str,
    source: &Path,
    target: &Path,
    store: &crate::domain::AppStore,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    TaskService::update(&state, task_id, TaskStatus::Running, 1, "正在迁移游戏库文件", None);
    let candidate = LibraryService::migrate(source, target, store, |progress, message| {
        TaskService::update(&state, task_id, TaskStatus::Running, progress, message, None);
    })?;
    if TaskService::is_cancelled(&state, task_id) {
        let _ = std::fs::remove_dir_all(target);
        return Err("任务已取消".to_string());
    }
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("解析 GameSaver 数据目录失败：{error}"))?;
    if let Err(error) = GameRepository::persist(app, &candidate) {
        let _ = std::fs::remove_dir_all(target);
        return Err(format!("保存迁移后的游戏记录失败：{error}"));
    }
    if let Err(error) = LibraryConfigRepository::save(
        &data_dir,
        &LibraryConfig {
            library_root: Some(target.to_string_lossy().to_string()),
        },
    ) {
        let _ = GameRepository::persist(app, store);
        let _ = std::fs::remove_dir_all(target);
        return Err(format!("保存游戏库配置失败：{error}"));
    }
    *state
        .library_root
        .lock()
        .map_err(|_| "更新游戏库根目录失败".to_string())? = target.to_path_buf();
    if let Err(error) = LibraryService::cleanup_source(source) {
        crate::logging::error(format!("旧游戏库大文件清理失败：{error}"));
        TaskService::update(&state, task_id, TaskStatus::Running, 98, "游戏库已切换，正在等待旧目录清理", None);
    }
    Ok(())
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.2} GB", bytes as f64 / 1024.0 / 1024.0 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .eq_ignore_ascii_case(right.to_string_lossy().replace('/', "\\").trim_end_matches('\\'))
}

fn disk_free_bytes(path: &Path) -> u64 {
    #[cfg(windows)]
    {
        use std::{ffi::OsStr, mem::MaybeUninit, os::windows::ffi::OsStrExt};
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let mut root = path.to_path_buf();
        while !root.exists() {
            if !root.pop() {
                return 0;
            }
        }
        let mut wide = OsStr::new(&root).encode_wide().collect::<Vec<_>>();
        wide.push(0);
        let mut available = MaybeUninit::uninit();
        let result = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                available.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if result != 0 {
            return unsafe { available.assume_init() };
        }
    }
    0
}
