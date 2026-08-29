mod app_state;
mod commands;
mod domain;
mod repositories;
mod services;

use app_state::AppState;
use repositories::GameRepository;
use services::{learning::cleanup_stale_captures, GameBodyUpdateService};
use std::collections::HashSet;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            cleanup_stale_captures(app.handle());
            let games_root = app.path().app_data_dir()?.join("games");
            let store = GameRepository::load(app.handle())?;
            let committed_archives = store.body_versions.iter().map(|version| version.archive_path.replace('/', "\\").trim_end_matches('\\').to_ascii_lowercase()).collect::<HashSet<_>>();
            if let Err(error) = GameBodyUpdateService::recover_pending_updates(&games_root, &committed_archives) {
                eprintln!("GameSaver 游戏更新恢复失败：{error}");
            }
            app.manage(AppState::new(store));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::game_commands::list_games,
            commands::game_commands::get_game,
            commands::add_game_commands::start_add_game_task,
            commands::task_commands::get_task,
            commands::task_commands::list_tasks,
            commands::task_commands::cancel_task
            ,commands::save_commands::start_save_learning_task
            ,commands::save_commands::start_finish_save_learning_task
            ,commands::save_commands::cancel_save_learning
            ,commands::save_commands::confirm_save_profile
            ,commands::save_commands::discard_pending_game
            ,commands::launch_commands::precheck_game_launch
            ,commands::launch_commands::launch_game
            ,commands::launch_commands::get_game_runtime
            ,commands::launch_commands::list_save_versions
            ,commands::save_version_commands::restore_save_version
            ,commands::save_version_commands::delete_save_version
            ,commands::save_version_commands::prune_save_versions
            ,commands::game_body_commands::list_game_body_versions
            ,commands::game_body_commands::update_game_body
        ])
        .run(tauri::generate_context!())
        .expect("error while running GameSaver");
}
