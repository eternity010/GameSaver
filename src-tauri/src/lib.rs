mod app_state;
mod commands;
mod domain;
mod repositories;
mod services;

use app_state::AppState;
use repositories::{GameRepository, TaskRepository};
use services::{learning::cleanup_stale_captures, BodyPackageService, GameBodyUpdateService};
use std::{collections::{HashMap, HashSet}, path::PathBuf};
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            cleanup_stale_captures(app.handle());
            let data_dir = app.path().app_data_dir()?;
            let games_root = data_dir.join("games");
            let body_package_root = data_dir.join("body-packages");
            if let Err(error) = BodyPackageService::cleanup_temporary_packages(&body_package_root) {
                eprintln!("GameSaver 本体包临时文件清理失败：{error}");
            }
            let store = GameRepository::load(app.handle())?;
            let tasks_path = TaskRepository::path(&data_dir);
            let tasks = match TaskRepository::load(&tasks_path) {
                Ok(tasks) => tasks,
                Err(error) => {
                    eprintln!("GameSaver 任务记录读取失败，将以空任务列表启动：{error}");
                    HashMap::new()
                }
            };
            if let Err(error) = TaskRepository::persist(&tasks_path, &tasks) {
                eprintln!("GameSaver 任务恢复状态写入失败：{error}");
            }
            let committed_archives = store
                .body_versions
                .iter()
                .map(|version| {
                    version
                        .archive_path
                        .replace('/', "\\")
                        .trim_end_matches('\\')
                        .to_ascii_lowercase()
                })
                .collect::<HashSet<_>>();
            if let Err(error) =
                GameBodyUpdateService::recover_pending_updates(&games_root, &committed_archives)
            {
                eprintln!("GameSaver 游戏更新恢复失败：{error}");
            }
            app.manage(AppState::new(store, tasks, PathBuf::from(tasks_path)));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::game_commands::list_games,
            commands::game_commands::get_game,
            commands::add_game_commands::start_add_game_task,
            commands::task_commands::get_task,
            commands::task_commands::list_tasks,
            commands::task_commands::cancel_task,
            commands::save_commands::start_save_learning_task,
            commands::save_commands::start_finish_save_learning_task,
            commands::save_commands::cancel_save_learning,
            commands::save_commands::confirm_save_profile,
            commands::save_commands::discard_pending_game,
            commands::launch_commands::precheck_game_launch,
            commands::launch_commands::launch_game,
            commands::launch_commands::get_game_runtime,
            commands::launch_commands::list_save_versions,
            commands::save_version_commands::restore_save_version,
            commands::save_version_commands::delete_save_version,
            commands::save_version_commands::prune_save_versions,
            commands::game_body_commands::list_game_body_versions,
            commands::game_body_commands::update_game_body,
            commands::game_body_commands::package_game_body,
            commands::game_body_commands::restore_game_body_package,
            commands::game_body_commands::delete_game_body_package
            ,commands::baidu_commands::get_baidu_status
            ,commands::baidu_commands::list_cloud_games
            ,commands::baidu_commands::install_cloud_game
            ,commands::baidu_commands::list_remote_body_packages
            ,commands::baidu_commands::repair_cloud_body_manifest
            ,commands::baidu_commands::upload_game_body_package
            ,commands::baidu_commands::download_game_body_package
            ,commands::baidu_config_commands::get_baidu_config
            ,commands::baidu_config_commands::save_baidu_config
            ,commands::baidu_config_commands::build_baidu_authorize_url
            ,commands::baidu_config_commands::exchange_baidu_code
            ,commands::baidu_config_commands::set_baidu_auto_upload
            ,commands::baidu_commands::get_baidu_quota
            ,commands::baidu_commands::delete_remote_body_package
        ])
        .run(tauri::generate_context!())
        .expect("error while running GameSaver");
}
