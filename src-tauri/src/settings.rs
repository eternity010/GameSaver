use crate::{
    app_state::{AppState, BackgroundTask},
    runtime::now_iso_string,
    shared::{DataPathKind, DataPathMigrationResult, SettingsPaths, UpdateSettingsPathsInput},
    storage::{
        default_backup_max_file_bytes, default_backup_root, normalize_backup_max_file_bytes,
        JsonStoreRepository, StoreRepository,
    },
    task_support::update_background_task,
};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tauri::{AppHandle, Manager, State};
use uuid::Uuid;
use walkdir::WalkDir;

fn count_directories(path: &Path) -> usize {
    if !path.exists() {
        return 0;
    }
    WalkDir::new(path)
        .min_depth(1)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_dir())
        .count()
}

fn ensure_directory_writable(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|err| format!("create directory failed: {err}"))?;
    let probe = path.join(format!(".gamesaver-write-test-{}", Uuid::new_v4()));
    fs::write(&probe, b"ok").map_err(|err| format!("directory is not writable: {err}"))?;
    fs::remove_file(&probe).map_err(|err| format!("cleanup write test failed: {err}"))?;
    Ok(())
}

fn validate_settings_path(raw: &str, field_name: &str) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("{field_name} cannot be empty"));
    }
    let path = PathBuf::from(trimmed);
    ensure_directory_writable(&path)?;
    Ok(path)
}

fn copy_directory_contents(source: &Path, target: &Path) -> Result<(usize, usize), String> {
    fs::create_dir_all(target).map_err(|err| format!("create target directory failed: {err}"))?;
    let mut copied_files = 0usize;
    let mut created_directories = count_directories(target);
    for entry in WalkDir::new(source).into_iter().filter_map(Result::ok) {
        let relative = match entry.path().strip_prefix(source) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target_path = target.join(relative);
        if entry.file_type().is_dir() {
            let existed = target_path.exists();
            fs::create_dir_all(&target_path).map_err(|err| format!("create directory failed: {err}"))?;
            if !existed {
                created_directories += 1;
            }
            continue;
        }
        if let Some(parent) = target_path.parent() {
            let existed = parent.exists();
            fs::create_dir_all(parent).map_err(|err| format!("create directory failed: {err}"))?;
            if !existed {
                created_directories += 1;
            }
        }
        fs::copy(entry.path(), &target_path).map_err(|err| format!("copy file failed: {err}"))?;
        copied_files += 1;
    }
    Ok((copied_files, created_directories))
}

fn build_settings_paths(app_state: &AppState) -> Result<SettingsPaths, String> {
    let store = app_state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    Ok(SettingsPaths {
        backup_root: store.execution_config.backup_root.clone(),
        default_backup_root: default_backup_root(),
        backup_max_file_bytes: store
            .execution_config
            .backup_max_file_bytes
            .unwrap_or_else(default_backup_max_file_bytes),
        default_backup_max_file_bytes: default_backup_max_file_bytes(),
    })
}

fn set_store_path(
    store: &mut crate::shared::PersistedStore,
    kind: DataPathKind,
    target: &str,
) {
    match kind {
        DataPathKind::BackupRoot => store.execution_config.backup_root = target.to_string(),
    }
}

fn get_store_path(store: &crate::shared::PersistedStore, kind: DataPathKind) -> String {
    match kind {
        DataPathKind::BackupRoot => store.execution_config.backup_root.clone(),
    }
}

fn migrate_data_path_impl<R: tauri::Runtime>(
    app: &AppHandle<R>,
    state: &AppState,
    kind: DataPathKind,
    target_path: String,
) -> Result<DataPathMigrationResult, String> {
    let target = validate_settings_path(
        &target_path,
        match kind {
            DataPathKind::BackupRoot => "backupRoot",
        },
    )?;

    let (source_path, source_exists) = {
        let store = state
            .store
            .lock()
            .map_err(|_| "failed to lock app state".to_string())?;
        let source = PathBuf::from(get_store_path(&store, kind));
        let exists = source.exists();
        (source, exists)
    };

    let (copied_files, created_directories) = if source_exists {
        copy_directory_contents(&source_path, &target)?
    } else {
        fs::create_dir_all(&target).map_err(|err| format!("create target directory failed: {err}"))?;
        (0, 0)
    };

    let mut store = state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    set_store_path(&mut store, kind, &target.to_string_lossy());
    JsonStoreRepository::new().normalize(&mut store);
    JsonStoreRepository::new().persist(app, &store)?;

    Ok(DataPathMigrationResult {
        kind,
        source_path: source_path.to_string_lossy().to_string(),
        target_path: target.to_string_lossy().to_string(),
        copied_files,
        created_directories,
        kept_original: true,
    })
}

#[tauri::command]
pub(crate) fn get_settings_paths(state: State<AppState>) -> Result<SettingsPaths, String> {
    build_settings_paths(state.inner())
}

#[tauri::command]
pub(crate) fn update_settings_paths(
    app: AppHandle,
    state: State<AppState>,
    input: UpdateSettingsPathsInput,
) -> Result<SettingsPaths, String> {
    let backup_root = input
        .backup_root
        .as_deref()
        .map(|value| validate_settings_path(value, "backupRoot"))
        .transpose()?;

    let mut store = state
        .store
        .lock()
        .map_err(|_| "failed to lock app state".to_string())?;
    if let Some(path) = backup_root {
        store.execution_config.backup_root = path.to_string_lossy().to_string();
    }
    if let Some(value) = input.backup_max_file_bytes {
        store.execution_config.backup_max_file_bytes = Some(normalize_backup_max_file_bytes(value));
    }
    JsonStoreRepository::new().normalize(&mut store);
    JsonStoreRepository::new().persist(&app, &store)?;

    Ok(SettingsPaths {
        backup_root: store.execution_config.backup_root.clone(),
        default_backup_root: default_backup_root(),
        backup_max_file_bytes: store
            .execution_config
            .backup_max_file_bytes
            .unwrap_or_else(default_backup_max_file_bytes),
        default_backup_max_file_bytes: default_backup_max_file_bytes(),
    })
}

#[tauri::command]
pub(crate) fn start_migrate_data_path_task(
    app: AppHandle,
    state: State<AppState>,
    kind: DataPathKind,
    target_path: String,
) -> Result<String, String> {
    if target_path.trim().is_empty() {
        return Err("targetPath cannot be empty".to_string());
    }

    let task_id = Uuid::new_v4().to_string();
    let now = now_iso_string();
    let task = BackgroundTask {
        task_id: task_id.clone(),
        task_type: "migrate_data_path".to_string(),
        status: "pending".to_string(),
        progress: Some(0),
        message: Some("task created".to_string()),
        result: None,
        error: None,
        started_at: now.clone(),
        updated_at: now,
    };
    {
        let mut tasks = state
            .tasks
            .lock()
            .map_err(|_| "failed to lock tasks".to_string())?;
        tasks.insert(task_id.clone(), task);
    }

    let app_handle = app.clone();
    let target_path_for_thread = target_path.trim().to_string();
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        update_background_task(
            &app_handle,
            &task_id_for_thread,
            "running",
            Some(5),
            Some("starting data path migration".to_string()),
            None,
            None,
        );
        let app_state: State<AppState> = app_handle.state();
        match migrate_data_path_impl(&app_handle, app_state.inner(), kind, target_path_for_thread) {
            Ok(summary) => update_background_task(
                &app_handle,
                &task_id_for_thread,
                "success",
                Some(100),
                Some("data path migration completed".to_string()),
                serde_json::to_value(summary).ok(),
                None,
            ),
            Err(err) => update_background_task(
                &app_handle,
                &task_id_for_thread,
                "failed",
                Some(100),
                Some("data path migration failed".to_string()),
                None,
                Some(err),
            ),
        }
    });

    Ok(task_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::PersistedStore;
    use std::sync::Mutex;
    use tauri::test::{mock_builder, mock_context, noop_assets};

    fn build_test_app() -> tauri::App<tauri::test::MockRuntime> {
        let context = mock_context(noop_assets());
        mock_builder()
            .manage(AppState {
                store: Mutex::new(PersistedStore::default()),
                tasks: Mutex::new(Default::default()),
            })
            .build(context)
            .expect("failed to build test app")
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("gamesaver-settings-test-{label}-{}", Uuid::new_v4()))
    }

    #[test]
    fn migrate_backup_root_copies_files_and_keeps_original() {
        let source_root = unique_temp_dir("source");
        let target_root = unique_temp_dir("target");
        fs::create_dir_all(source_root.join("game-a").join("versions")).expect("create source");
        fs::write(
            source_root.join("game-a").join("versions").join("v1.sav"),
            b"backup-data",
        )
        .expect("write source file");

        let app = build_test_app();
        {
            let state = app.state::<AppState>();
            let mut store = state.store.lock().expect("lock store");
            store.execution_config.backup_root = source_root.to_string_lossy().to_string();
        }

        let result = migrate_data_path_impl(
            &app.handle().clone(),
            app.state::<AppState>().inner(),
            DataPathKind::BackupRoot,
            target_root.to_string_lossy().to_string(),
        )
        .expect("migrate backup root");

        assert_eq!(result.kind as u8, DataPathKind::BackupRoot as u8);
        assert!(result.kept_original);
        assert!(result.copied_files >= 1);
        assert!(source_root.join("game-a").join("versions").join("v1.sav").exists());
        assert!(target_root.join("game-a").join("versions").join("v1.sav").exists());

        let state = app.state::<AppState>();
        let store = state.store.lock().expect("lock store after migration");
        assert_eq!(
            store.execution_config.backup_root,
            target_root.to_string_lossy().to_string()
        );

        let _ = fs::remove_dir_all(&source_root);
        let _ = fs::remove_dir_all(&target_root);
    }
}
