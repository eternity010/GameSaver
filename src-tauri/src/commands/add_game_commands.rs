use crate::{
    app_state::AppState,
    domain::{is_safe_path_segment, path_utils::normalize_path, Game, TaskCategory, TaskStatus},
    repositories::GameRepository,
    services::{AddGameService, GameLibraryService, TaskService},
};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager, State};

#[tauri::command]
pub fn start_add_game_task(
    app: AppHandle,
    state: State<AppState>,
    source_path: String,
    executable_path: String,
    display_name: String,
    game_key: String,
    allow_large_source: bool,
) -> Result<String, String> {
    let source = PathBuf::from(source_path.trim());
    let executable = PathBuf::from(executable_path.trim());
    let display_name = display_name.trim().to_string();
    if display_name.is_empty() {
        return Err("游戏名称不能为空".to_string());
    }
    let game_key = Game::derive_game_key(&game_key);
    // 用领域层的 `is_safe_path_segment` 而不是手写分隔符检查：手写那版漏了 `..` 与 `.`，
    // 而 game_key 会被拼进远程路径（云端存档目录、本体目录）并直接作用于远端文件。
    if !is_safe_path_segment(&game_key) {
        return Err("游戏标识不能为空，且不能包含路径分隔符".to_string());
    }
    let executable_relative_path = AddGameService::validate_source(&source, &executable)?;
    let source = source
        .canonicalize()
        .map_err(|err| format!("解析游戏目录失败：{err}"))?;
    let games_root = state.games_root()?;
    if paths_overlap(&source, &games_root) {
        return Err("原始游戏目录不能位于 GameSaver 受管游戏目录内，或包含该目录".to_string());
    }

    let operation_key = state.claim_game_add(&game_key)?;
    let mut reservation = AddOperationGuard {
        state: &state,
        key: Some(operation_key.clone()),
    };

    let game_uid = uuid::Uuid::new_v4().to_string();
    let managed_path = games_root.join(&game_uid);
    {
        let store = state
            .store
            .lock()
            .map_err(|_| "lock GameSaver store failed".to_string())?;
        if store.games.iter().any(|game| game.game_key == game_key) {
            return Err("游戏标识已存在，请在游戏库继续设置，或删除现有记录后重试".to_string());
        }
        if store
            .games
            .iter()
            .any(|game| game.managed_path == managed_path.to_string_lossy())
        {
            return Err("受管游戏目录已存在".to_string());
        }
    }
    let task_id = TaskService::create(
        &state,
        "add_game",
        TaskCategory::Maintenance,
        Some(game_uid.clone()),
        "准备添加游戏",
    )?;
    let games_root = match (|| {
        let root = state.games_root()?;
        if paths_overlap(&source, &root) {
            return Err("原始游戏目录不能与当前受管游戏库重叠".to_string());
        }
        Ok(root)
    })() {
        Ok(root) => root,
        Err(error) => {
            TaskService::finish(
                &state,
                &task_id,
                TaskStatus::Failed,
                0,
                "读取当前游戏库位置失败",
                None,
                Some(error.clone()),
            );
            return Err(error);
        }
    };
    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    let games_root_for_cleanup = games_root.clone();
    let game_uid_for_cleanup = game_uid.clone();
    let spawn = std::thread::Builder::new()
        .name("add-game".into())
        .spawn(move || {
            let worker_state = app_handle.state::<AppState>();
            let _reservation = AddOperationGuard {
                state: &worker_state,
                key: Some(operation_key),
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || -> Result<(Game, String), String> {
                    TaskService::update(
                        &app_handle.state(),
                        &task_id_for_thread,
                        TaskStatus::Running,
                        1,
                        "正在检查游戏目录",
                        None,
                    );
                    let copy_result = AddGameService::copy_to_managed_with_commit(
                        &source,
                        &games_root,
                        &game_uid,
                        executable_relative_path,
                        allow_large_source,
                        |progress, message| {
                            TaskService::update(
                                &app_handle.state(),
                                &task_id_for_thread,
                                TaskStatus::Running,
                                progress,
                                message,
                                None,
                            )
                        },
                        || TaskService::is_cancelled(&app_handle.state(), &task_id_for_thread),
                        || TaskService::begin_commit(&app_handle.state(), &task_id_for_thread),
                    )?;
                    let mut game = Game::new_pending(
                        display_name,
                        copy_result.managed_path.to_string_lossy(),
                        copy_result.executable_relative_path,
                    );
                    game.game_uid = game_uid.clone();
                    game.game_key = game_key.clone();
                    game.mark_body_upload_pending();
                    let copied_message = format!(
                        "游戏已加入，复制 {} 个文件（{} MB），等待存档设置",
                        copy_result.file_count,
                        copy_result.total_bytes / 1024 / 1024
                    );
                    let state: State<AppState> = app_handle.state();
                    let mut store = state
                        .store
                        .lock()
                        .map_err(|_| "lock GameSaver store failed".to_string())?;
                    let mut candidate = store.clone();
                    GameLibraryService::register_pending(&mut candidate, game.clone())?;
                    GameRepository::persist(&app_handle, &candidate)?;
                    *store = candidate;
                    drop(store);
                    if let Err(error) = AddGameService::complete_copy(&games_root, &game_uid) {
                        crate::logging::error(format!(
                            "游戏已登记，添加事务将在启动时收尾：{error}"
                        ));
                    }
                    Ok((game, copied_message))
                },
            ))
            .unwrap_or_else(|_| Err("添加游戏线程异常中止，重启后将恢复现场".into()));
            if result.is_err() {
                let unregistered = worker_state
                    .store
                    .lock()
                    .map(|store| {
                        !store
                            .games
                            .iter()
                            .any(|game| game.game_uid == game_uid_for_cleanup)
                    })
                    .unwrap_or(false);
                if unregistered {
                    if let Err(cleanup_error) = AddGameService::cleanup_copy_artifacts(
                        &games_root_for_cleanup,
                        &game_uid_for_cleanup,
                    ) {
                        crate::logging::error(format!(
                            "添加游戏失败后的本体清理失败：{cleanup_error}"
                        ));
                    }
                }
            }
            match result {
                Ok((game, message)) => TaskService::finish(
                    &app_handle.state(),
                    &task_id_for_thread,
                    TaskStatus::Success,
                    100,
                    message,
                    serde_json::to_value(game).ok(),
                    None,
                ),
                Err(error) if error == "任务已取消" => TaskService::finish(
                    &app_handle.state(),
                    &task_id_for_thread,
                    TaskStatus::Cancelled,
                    100,
                    "已取消添加游戏",
                    None,
                    None,
                ),
                Err(error) => TaskService::finish(
                    &app_handle.state(),
                    &task_id_for_thread,
                    TaskStatus::Failed,
                    100,
                    "添加游戏失败",
                    None,
                    Some(error),
                ),
            }
        });
    if let Err(error) = spawn {
        TaskService::finish(
            &state,
            &task_id,
            TaskStatus::Failed,
            0,
            "启动添加游戏任务失败",
            None,
            Some(error.to_string()),
        );
        return Err(format!("启动添加游戏线程失败：{error}"));
    }
    reservation.key = None;
    Ok(task_id)
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    let left = normalize_path(left);
    let right = normalize_path(right);
    left == right || left.starts_with(&(right.clone() + "\\")) || right.starts_with(&(left + "\\"))
}

struct AddOperationGuard<'a> {
    state: &'a AppState,
    key: Option<String>,
}

impl Drop for AddOperationGuard<'_> {
    fn drop(&mut self) {
        if let Some(key) = self.key.as_deref() {
            self.state.release_operation(key);
        }
    }
}
