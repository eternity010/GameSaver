use super::run_blocking;
use crate::{
    app_state::AppState,
    domain::{TaskCategory, TaskStatus},
    repositories::GameRepository,
    services::{
        disk_space,
        library_service::{LibraryMigration, LibraryStorageOverview, MigrationOutcome},
        LibraryService, TaskService,
    },
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
    pub covers_bytes: u64,
    pub total_bytes: u64,
    pub file_count: usize,
    pub free_bytes: u64,
    pub pending_migration_source: Option<String>,
    pub migration_warning: Option<String>,
}

#[tauri::command]
pub fn get_library_settings(
    app: AppHandle,
    state: State<AppState>,
) -> Result<LibrarySettingsView, String> {
    let root = state.library_root_path()?;
    let usage = LibraryService::usage(&root)?;
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("解析数据目录失败：{error}"))?;
    let pending = LibraryMigration::pending(&data_dir)?;
    Ok(LibrarySettingsView {
        library_root: root.to_string_lossy().to_string(),
        games_path: root.join("games").to_string_lossy().to_string(),
        body_packages_path: root.join("body-packages").to_string_lossy().to_string(),
        saves_path: root.join("saves").to_string_lossy().to_string(),
        total_bytes: usage.total_bytes(),
        games_bytes: usage.games_bytes,
        body_packages_bytes: usage.body_packages_bytes,
        saves_bytes: usage.saves_bytes,
        covers_bytes: usage.covers_bytes,
        file_count: usage.file_count,
        // 探不到盘时报告 0（前端按"未知"处理），这是本视图原有的约定。
        free_bytes: disk_space::available_space(&root).unwrap_or(0),
        pending_migration_source: pending
            .as_ref()
            .map(|record| record.source().to_string_lossy().to_string()),
        migration_warning: pending.map(|record| {
            if record.cleanup_only() {
                format!(
                    "新游戏库已启用，旧目录尚未清理完成：{}",
                    record.source().display()
                )
            } else {
                "游戏库迁移尚未完成，请继续恢复后再操作游戏。".to_string()
            }
        }),
    })
}

#[tauri::command]
pub async fn get_library_storage_overview(app: AppHandle) -> Result<LibraryStorageOverview, String> {
    run_blocking(move || library_storage_overview_blocking(app)).await
}

fn library_storage_overview_blocking(app: AppHandle) -> Result<LibraryStorageOverview, String> {
    let state = app.state::<AppState>();
    let root = state.library_root_path()?;
    let store = state
        .store
        .lock()
        .map_err(|_| "读取游戏库记录失败".to_string())?
        .clone();
    let overview = LibraryService::storage_overview(&root, &store)?;
    if root != state.library_root_path()? {
        return Err("游戏库位置在统计期间发生变化，请刷新重试".to_string());
    }
    Ok(overview)
}

#[tauri::command]
pub fn start_set_library_root_task(
    app: AppHandle,
    state: State<AppState>,
    target_root: String,
) -> Result<String, String> {
    let target = PathBuf::from(target_root.trim());
    start_migration_task(app, &state, Some(target))
}

#[tauri::command]
pub fn resume_library_migration_task(
    app: AppHandle,
    state: State<AppState>,
) -> Result<String, String> {
    start_migration_task(app, &state, None)
}

fn start_migration_task(
    app: AppHandle,
    state: &AppState,
    target: Option<PathBuf>,
) -> Result<String, String> {
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("解析数据目录失败：{error}"))?;
    if target.is_some() && LibraryMigration::pending(&data_dir)?.is_some() {
        return Err("请先继续未完成的迁移或重试旧库清理".to_string());
    }
    if target.is_none() && LibraryMigration::pending(&data_dir)?.is_none() {
        return Err("没有待恢复的游戏库迁移".to_string());
    }
    state.claim_library_migration(target.is_none())?;
    let task_id = match TaskService::create(
        state,
        if target.is_some() {
            "set_library_root"
        } else {
            "resume_library_migration"
        },
        TaskCategory::Maintenance,
        None,
        "准备迁移游戏库",
    ) {
        Ok(task_id) => task_id,
        Err(error) => {
            let blocked = LibraryMigration::pending(&data_dir)
                .map(|pending| pending.is_some_and(|record| !record.cleanup_only()))
                .unwrap_or(true);
            state.finish_library_migration(blocked);
            return Err(error);
        }
    };
    let task_id_for_thread = task_id.clone();
    let worker_app = app.clone();
    let worker_data_dir = data_dir.clone();
    let spawn = std::thread::Builder::new().name("library-migration".to_string()).spawn(move || {
        let data_dir = worker_data_dir;
        let state = worker_app.state::<AppState>();
        let mut result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            migrate_library(&worker_app, &task_id_for_thread, &data_dir, target.as_deref())
        })).unwrap_or_else(|_| Err("迁移线程发生异常，已保留迁移日志和原库，请继续恢复或重启应用".to_string()));
        if result.is_err() {
            if let Ok(Some(mut record)) = LibraryMigration::pending(&data_dir) {
                if record.copying() {
                    if let Err(cleanup_error) = record.abort_copy(&data_dir) {
                        result = Err(format!("{}；暂存清理失败：{cleanup_error}", result.unwrap_err()));
                    }
                }
            }
        }
        let blocked = result.is_err()
            && LibraryMigration::pending(&data_dir).map(|pending| pending.is_some()).unwrap_or(true);
        state.finish_library_migration(blocked);
        match result {
            Ok(outcome) => TaskService::finish(
                &state,
                &task_id_for_thread,
                TaskStatus::Success,
                100,
                if outcome.pending { "游戏库已切换，旧目录清理待重试" } else { "游戏库迁移处理已完成" },
                Some(serde_json::json!({ "libraryRoot": outcome.library_root, "warning": outcome.warning, "pendingCleanup": outcome.pending })),
                None,
            ),
            Err(error) if error == "任务已取消" => TaskService::finish(
                &state,
                &task_id_for_thread,
                TaskStatus::Cancelled,
                100,
                "已取消游戏库迁移",
                None,
                None,
            ),
            Err(error) => TaskService::finish(
                &state,
                &task_id_for_thread,
                TaskStatus::Failed,
                100,
                if blocked { "迁移待恢复，请前往平台设置继续" } else { "游戏库迁移失败，原库保留" },
                None,
                Some(error),
            ),
        }
    });
    if let Err(error) = spawn {
        let blocked = LibraryMigration::pending(&data_dir)
            .map(|pending| pending.is_some_and(|record| !record.cleanup_only()))
            .unwrap_or(true);
        state.finish_library_migration(blocked);
        TaskService::finish(
            state,
            &task_id,
            TaskStatus::Failed,
            0,
            "启动迁移线程失败",
            None,
            Some(error.to_string()),
        );
        return Err(format!("启动迁移线程失败：{error}"));
    }
    Ok(task_id)
}

fn migrate_library(
    app: &AppHandle,
    task_id: &str,
    data_dir: &Path,
    target: Option<&Path>,
) -> Result<MigrationOutcome, String> {
    let state = app.state::<AppState>();
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        1,
        "正在迁移游戏库文件",
        None,
    );
    if let Some(target) = target {
        let source = state.library_root_path()?;
        LibraryService::validate_target(&source, target)?;
        let usage = LibraryService::usage(&source)?;
        let free_bytes = disk_space::available_space(target).unwrap_or(0);
        if free_bytes > 0 && usage.total_bytes() > free_bytes {
            return Err(format!(
                "新游戏库磁盘空间不足：需要 {}，可用 {}",
                format_bytes(usage.total_bytes()),
                format_bytes(free_bytes)
            ));
        }
        let mut record = LibraryMigration::begin(data_dir, &source, target)?;
        record.copy(|progress, message| {
            if TaskService::is_cancelled(&state, task_id) {
                return Err("任务已取消".to_string());
            }
            TaskService::update(
                &state,
                task_id,
                TaskStatus::Running,
                progress,
                message,
                None,
            );
            Ok(())
        })?;
        TaskService::begin_commit(&state, task_id)?;
        record.mark_ready(data_dir)?;
    } else {
        TaskService::begin_commit(&state, task_id)?;
    }
    let outcome = LibraryMigration::recover(data_dir, |source, target| {
        state.with_store_mut(|current| {
            LibraryService::rewrite_paths_in_place(current, source, target)?;
            GameRepository::persist(app, current)
        })
    })?
    .ok_or("迁移记录已不存在")?;
    *state
        .library_root
        .lock()
        .map_err(|_| "更新游戏库根目录失败")? = outcome.library_root.clone();
    if let Some(warning) = &outcome.warning {
        crate::logging::warn(warning);
    }
    Ok(outcome)
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.2} GB", bytes as f64 / 1024.0 / 1024.0 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
    }
}
