mod app_state;
mod commands;
mod cover_protocol;
mod domain;
mod logging;
mod repositories;
mod services;

use app_state::AppState;
use repositories::{GameRepository, LibraryConfigRepository, TaskRepository};
use services::{learning::cleanup_stale_captures, BodyPackageService, GameBodyUpdateService};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let result = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            if let Err(error) = logging::init(&data_dir) {
                eprintln!("GameSaver 日志系统初始化失败：{error}");
            }
            logging::install_panic_hook();
            logging::info(format!("应用启动，数据目录：{}", data_dir.display()));
            cleanup_stale_captures(app.handle());
            let library_root = LibraryConfigRepository::resolve_root(&data_dir)
                .map_err(|error| format!("解析游戏库根目录失败：{error}"))?;
            let games_root = library_root.join("games");
            let body_package_root = library_root.join("body-packages");
            if let Err(error) = BodyPackageService::cleanup_temporary_packages(&body_package_root) {
                logging::error(format!("本体包临时文件清理失败：{error}"));
                eprintln!("GameSaver 本体包临时文件清理失败：{error}");
            }
            let store = GameRepository::load(app.handle())?;
            let removed_restore_archives =
                match GameBodyUpdateService::cleanup_removed_restore_archives(&games_root) {
                    Ok(paths) => paths,
                    Err(error) => {
                        logging::error(format!("清理已取消功能的本体恢复副本失败：{error}"));
                        Vec::new()
                    }
                };
            let mut store = store;
            if !removed_restore_archives.is_empty() {
                let removed = removed_restore_archives
                    .iter()
                    .map(|path| {
                        path.to_string_lossy()
                            .replace('/', "\\")
                            .trim_end_matches('\\')
                            .to_ascii_lowercase()
                    })
                    .collect::<HashSet<_>>();
                let mut cleaned = store.clone();
                for version in &mut cleaned.body_versions {
                    let archive = version
                        .archive_path
                        .replace('/', "\\")
                        .trim_end_matches('\\')
                        .to_ascii_lowercase();
                    if removed.contains(&archive) {
                        version.archive_path.clear();
                    }
                }
                if let Err(error) = GameRepository::persist(app.handle(), &cleaned) {
                    logging::error(format!("清理本体恢复副本记录失败：{error}"));
                } else {
                    store = cleaned;
                }
            }
            let referenced_packages = store
                .body_versions
                .iter()
                .filter_map(|version| version.package_path.as_deref())
                .map(|path| {
                    path.replace('\\', "/")
                        .trim_end_matches('/')
                        .to_ascii_lowercase()
                })
                .collect::<HashSet<_>>();
            if let Err(error) = BodyPackageService::cleanup_orphan_packages(
                &body_package_root,
                &referenced_packages,
            ) {
                logging::error(format!("孤立本体包清理失败：{error}"));
                eprintln!("GameSaver 孤立本体包清理失败：{error}");
            }
            let tasks_path = TaskRepository::path(&data_dir);
            let tasks = match TaskRepository::load(&tasks_path) {
                Ok(tasks) => tasks,
                Err(error) => {
                    logging::error(format!("任务记录读取失败，将以空任务列表启动：{error}"));
                    eprintln!("GameSaver 任务记录读取失败，将以空任务列表启动：{error}");
                    HashMap::new()
                }
            };
            if let Err(error) = TaskRepository::persist(&tasks_path, &tasks) {
                logging::error(format!("任务恢复状态写入失败：{error}"));
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
                logging::error(format!("游戏更新恢复失败：{error}"));
                eprintln!("GameSaver 游戏更新恢复失败：{error}");
            }
            if let Err(error) = GameBodyUpdateService::cleanup_archived_body_versions(
                &games_root,
                &mut store.body_versions,
            ) {
                logging::error(format!("清理历史游戏本体失败：{error}"));
                eprintln!("GameSaver 清理历史游戏本体失败：{error}");
            }
            if let Err(error) = GameRepository::persist(app.handle(), &store) {
                logging::error(format!("清理历史游戏本体记录失败：{error}"));
                eprintln!("GameSaver 清理历史游戏本体记录失败：{error}");
            }
            app.manage(AppState::new(
                store,
                library_root,
                tasks,
                PathBuf::from(tasks_path),
            ));
            Ok(())
        })
        .register_uri_scheme_protocol("gamesaver-cover", cover_protocol::handle_cover_request)
        .invoke_handler(tauri::generate_handler![
            commands::game_commands::list_games,
            commands::game_commands::get_game,
            commands::game_commands::rename_game,
            commands::game_commands::save_game_cover,
            commands::game_commands::get_game_cover,
            commands::game_commands::get_game_cover_path,
            commands::game_commands::get_game_cover_paths,
            commands::add_game_commands::start_add_game_task,
            commands::task_commands::get_task,
            commands::task_commands::list_tasks,
            commands::task_commands::cancel_task,
            commands::task_commands::delete_tasks,
            commands::save_commands::start_save_learning_task,
            commands::save_commands::start_save_candidate_verification_task,
            commands::save_commands::start_finish_save_learning_task,
            commands::save_commands::cancel_save_learning,
            commands::save_commands::confirm_save_profile,
            commands::save_commands::get_save_profile,
            commands::save_commands::update_save_profile_keep_versions,
            commands::save_commands::update_save_profile_scopes,
            commands::save_commands::discard_pending_game,
            commands::save_commands::open_path_in_explorer,
            commands::save_commands::get_default_save_exclusions,
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
            commands::game_body_commands::delete_game_body_package,
            commands::game_body_commands::uninstall_game_body,
            commands::baidu_commands::get_baidu_status,
            commands::baidu_commands::list_cloud_games,
            commands::baidu_commands::get_cloud_game_cover,
            commands::baidu_commands::get_cloud_game_cover_path,
            commands::baidu_commands::get_cloud_game_cover_paths,
            commands::baidu_commands::install_cloud_game,
            commands::baidu_commands::list_remote_body_packages,
            commands::baidu_commands::repair_cloud_body_manifest,
            commands::baidu_commands::upload_game_body_package,
            commands::baidu_commands::download_game_body_package,
            commands::baidu_config_commands::get_baidu_config,
            commands::baidu_config_commands::save_baidu_config,
            commands::baidu_config_commands::build_baidu_authorize_url,
            commands::baidu_config_commands::exchange_baidu_code,
            commands::baidu_config_commands::set_baidu_auto_upload,
            commands::baidu_commands::get_baidu_quota,
            commands::baidu_commands::delete_remote_body_package,
            commands::diagnostics_commands::report_frontend_error,
            commands::admin_commands::get_elevation_status,
            commands::admin_commands::restart_as_admin,
            commands::library_commands::get_library_settings,
            commands::library_commands::start_set_library_root_task,
            commands::cloud_account_commands::get_cloud_account_status,
            commands::cloud_account_commands::start_upload_cloud_account_task,
            commands::cloud_account_commands::start_download_cloud_account_task,
            commands::baidu_config_commands::update_baidu_save_sync_settings,
            commands::cloud_save_commands::get_cloud_save_status,
            commands::cloud_save_commands::list_cloud_save_versions,
            commands::cloud_save_commands::start_upload_save_version_task,
            commands::cloud_save_commands::start_restore_cloud_save_task,
            commands::cloud_save_commands::delete_cloud_save_version
        ])
        .run(tauri::generate_context!());
    if let Err(error) = result {
        logging::error(format!("Tauri 运行失败：{error}"));
        eprintln!("GameSaver 运行失败：{error}");
    }
}
