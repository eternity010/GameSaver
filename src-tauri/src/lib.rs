use std::{collections::HashMap, sync::Mutex};

use tauri::{Manager, State};

mod app_state;
mod launcher;
mod library;
mod learning;
mod migration;
mod path_utils;
mod precheck;
mod rules;
mod runtime;
mod runtime_commands;
mod settings;
mod shared;
mod storage;
mod tasks;
mod task_support;

use app_state::AppState;
use shared::PersistedStore;
use storage::{JsonStoreRepository, StoreRepository};

// New library entry under construction.
// This file is intentionally not wired into Cargo yet. We can keep migrating
// commands here while the current lib.rs remains the runnable fallback.
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            store: Mutex::new(PersistedStore::default()),
            tasks: Mutex::new(HashMap::new()),
        })
        .setup(|app| {
            let loaded = match JsonStoreRepository::new().load(app.handle()) {
                Ok(store) => store,
                Err(err) => {
                    eprintln!("[GameSaver] load_store failed, using default store: {err}");
                    PersistedStore::default()
                }
            };
            let state: State<AppState> = app.state();
            if let Ok(mut guard) = state.store.lock() {
                *guard = loaded;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            learning::commands::start_learning,
            learning::commands::launch_game,
            learning::commands::start_finish_learning_task,
            learning::commands::start_retry_finish_learning_task,
            tasks::get_task,
            learning::commands::finish_learning,
            learning::commands::cancel_learning,
            learning::commands::confirm_rule,
            learning::commands::open_candidate_path,
            learning::commands::get_learning_session,
            rules::list_rules,
            rules::list_rule_conflicts,
            rules::set_primary_rule,
            rules::update_rule,
            rules::delete_rule,
            rules::export_rules,
            rules::import_rules,
            library::resolve_rule_for_exe,
            library::get_launcher_session,
            library::list_launcher_sessions,
            library::list_game_library_items,
            launcher::launch_with_rule,
            launcher::launch_game_from_library,
            launcher::list_backup_versions,
            launcher::get_backup_stats,
            launcher::set_backup_keep_versions,
            launcher::prune_backup_versions,
            launcher::restore_backup_version,
            launcher::start_restore_backup_version_task,
            migration::export_migration_zip,
            migration::start_export_migration_zip_task,
            migration::import_migration_zip,
            migration::start_import_migration_zip_task,
            precheck::precheck_game_launch,
            library::set_preferred_exe_path,
            settings::get_settings_paths,
            settings::update_settings_paths,
            settings::start_migrate_data_path_task,
            runtime_commands::get_runtime_status,
            runtime_commands::restart_as_admin
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
