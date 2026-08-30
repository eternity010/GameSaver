use crate::{
    app_state::AppState,
    domain::{Game, TaskStatus},
    repositories::GameRepository,
    services::{AddGameService, GameLibraryService, TaskService},
};
use std::{fs, path::{Path, PathBuf}};
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
    if game_key.is_empty() || game_key.contains('/') || game_key.contains('\\') {
        return Err("游戏标识不能为空，且不能包含路径分隔符".to_string());
    }
    let executable_relative_path = AddGameService::validate_source(&source, &executable)?;
    let source = source.canonicalize().map_err(|err| format!("解析游戏目录失败：{err}"))?;
    let games_root = state.games_root()?;
    if paths_overlap(&source, &games_root) {
        return Err("原始游戏目录不能位于 GameSaver 受管游戏目录内，或包含该目录".to_string());
    }

    let game_uid = uuid::Uuid::new_v4().to_string();
    let managed_path = games_root.join(&game_uid);
    {
        let store = state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())?;
        if store.games.iter().any(|game| game.game_key == game_key) {
            return Err("游戏标识已存在，请使用游戏库中的现有记录".to_string());
        }
        if store
            .games
            .iter()
            .any(|game| game.managed_path == managed_path.to_string_lossy())
        {
            return Err("受管游戏目录已存在".to_string());
        }
    }
    let task_id = TaskService::create(&state, "add_game", Some(game_uid.clone()), "准备添加游戏")?;
    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    let games_root_for_cleanup = games_root.clone();
    let game_uid_for_cleanup = game_uid.clone();
    std::thread::spawn(move || {
        let result = (|| -> Result<(Game, String), String> {
            TaskService::update(&app_handle.state(), &task_id_for_thread, TaskStatus::Running, 1, "正在检查游戏目录", None);
            let copy_result = AddGameService::copy_to_managed(
                &source,
                &games_root,
                &game_uid,
                executable_relative_path,
                allow_large_source,
                |progress, message| TaskService::update(&app_handle.state(), &task_id_for_thread, TaskStatus::Running, progress, message, None),
                || TaskService::is_cancelled(&app_handle.state(), &task_id_for_thread),
            )?;
            if TaskService::is_cancelled(&app_handle.state(), &task_id_for_thread) {
                let _ = fs::remove_dir_all(&copy_result.managed_path);
                return Err("任务已取消".to_string());
            }
            let mut game = Game::new_pending(display_name, copy_result.managed_path.to_string_lossy(), copy_result.executable_relative_path);
            game.game_uid = game_uid.clone();
            game.game_key = game_key.clone();
            let copied_message = format!("游戏已加入，复制 {} 个文件（{} MB），等待存档设置", copy_result.file_count, copy_result.total_bytes / 1024 / 1024);
            let state: State<AppState> = app_handle.state();
            let mut store = state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())?;
            let mut candidate = store.clone();
            GameLibraryService::register_pending(&mut candidate, game.clone())?;
            if let Err(error) = GameRepository::persist(&app_handle, &candidate) {
                let _ = fs::remove_dir_all(&copy_result.managed_path);
                return Err(error);
            }
            *store = candidate;
            Ok((game, copied_message))
        })();
        if result.is_err() {
            if let Err(cleanup_error) = AddGameService::cleanup_copy_artifacts(&games_root_for_cleanup, &game_uid_for_cleanup) {
                crate::logging::error(format!("添加游戏失败后的本体清理失败：{cleanup_error}"));
            }
        }
        match result {
            Ok((_, message)) => TaskService::update(&app_handle.state(), &task_id_for_thread, TaskStatus::Success, 100, message, None),
            Err(error) if error == "任务已取消" => TaskService::update(&app_handle.state(), &task_id_for_thread, TaskStatus::Cancelled, 100, "已取消添加游戏", None),
            Err(error) => TaskService::update(&app_handle.state(), &task_id_for_thread, TaskStatus::Failed, 100, "添加游戏失败", Some(error)),
        }
    });
    Ok(task_id)
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    let left = left.to_string_lossy().replace('/', "\\").to_ascii_lowercase();
    let right = right.to_string_lossy().replace('/', "\\").to_ascii_lowercase();
    left == right || left.starts_with(&(right.clone() + "\\")) || right.starts_with(&(left + "\\"))
}
