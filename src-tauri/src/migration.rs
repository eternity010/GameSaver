use crate::{
    app_state::{AppState, BackgroundTask},
    runtime::now_iso_string,
    shared::{ExportMigrationZipResult, GameSaveRule, ImportMigrationZipResult},
    storage::{decode_text_bytes, new_game_uid, normalize_game_uid, JsonStoreRepository, StoreRepository},
    task_support::update_background_task,
};
use std::{
    collections::HashSet,
    fs,
    path::{Component, Path, PathBuf},
};
use tauri::{AppHandle, Manager, State};
use uuid::Uuid;
use walkdir::WalkDir;

fn create_migration_temp_dir(prefix: &str) -> Result<PathBuf, String> {
    let path = std::env::temp_dir()
        .join("gamesaver-migration")
        .join(format!("{}-{}", prefix, Uuid::new_v4()));
    fs::create_dir_all(&path).map_err(|err| format!("create temp directory failed: {err}"))?;
    Ok(path)
}

fn normalize_zip_entry_name(relative_path: &Path) -> Result<String, String> {
    let mut parts = Vec::new();
    for component in relative_path.components() {
        match component {
            Component::Normal(value) => {
                let part = value.to_string_lossy().trim().to_string();
                if part.is_empty() {
                    return Err("zip entry has empty path segment".to_string());
                }
                parts.push(part);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("zip entry contains invalid path".to_string());
            }
        }
    }
    if parts.is_empty() {
        return Err("zip entry path is empty".to_string());
    }
    Ok(parts.join("/"))
}

fn write_pretty_json_file<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("create directory failed: {err}"))?;
    }
    let content = serde_json::to_string_pretty(value).map_err(|err| format!("serialize json failed: {err}"))?;
    fs::write(path, content).map_err(|err| format!("write json failed: {err}"))
}

fn sync_directory(source: &Path, target: &Path) -> Result<usize, String> {
    if !source.exists() {
        return Ok(0);
    }
    fs::create_dir_all(target).map_err(|err| format!("create directory failed: {err}"))?;
    let mut copied = 0usize;
    for entry in WalkDir::new(source).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = match entry.path().strip_prefix(source) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let dest = target.join(relative);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|err| format!("create directory failed: {err}"))?;
        }
        fs::copy(entry.path(), &dest).map_err(|err| format!("copy file failed: {err}"))?;
        copied += 1;
    }
    Ok(copied)
}

fn zip_directory_contents(source_dir: &Path, output_zip_path: &Path) -> Result<usize, String> {
    if !source_dir.exists() {
        return Err("source directory does not exist".to_string());
    }
    if let Some(parent) = output_zip_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|err| format!("create zip directory failed: {err}"))?;
        }
    }
    let output_file = fs::File::create(output_zip_path).map_err(|err| format!("create zip failed: {err}"))?;
    let mut zip_writer = zip::ZipWriter::new(output_file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);

    let mut file_count = 0usize;
    for entry in WalkDir::new(source_dir).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(source_dir)
            .map_err(|err| format!("build zip relative path failed: {err}"))?;
        let zip_entry_name = normalize_zip_entry_name(relative)?;
        zip_writer
            .start_file(zip_entry_name, options)
            .map_err(|err| format!("write zip entry failed: {err}"))?;
        let mut source_file = fs::File::open(entry.path()).map_err(|err| format!("read file failed: {err}"))?;
        std::io::copy(&mut source_file, &mut zip_writer).map_err(|err| format!("zip file failed: {err}"))?;
        file_count += 1;
    }
    zip_writer.finish().map_err(|err| format!("finalize zip failed: {err}"))?;
    Ok(file_count)
}

fn unzip_archive_to_directory(zip_path: &Path, destination: &Path) -> Result<usize, String> {
    if !zip_path.exists() {
        return Err("migration zip does not exist".to_string());
    }
    fs::create_dir_all(destination).map_err(|err| format!("create unzip directory failed: {err}"))?;
    let zip_file = fs::File::open(zip_path).map_err(|err| format!("open zip failed: {err}"))?;
    let mut archive = zip::ZipArchive::new(zip_file).map_err(|err| format!("read zip failed: {err}"))?;

    let mut extracted_files = 0usize;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|err| format!("read zip entry failed: {err}"))?;
        let entry_name = entry.name().to_string();
        let enclosed = entry
            .enclosed_name()
            .ok_or_else(|| format!("zip contains invalid path: {entry_name}"))?
            .to_path_buf();
        if enclosed
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
        {
            return Err(format!("zip contains invalid path: {entry_name}"));
        }
        let output_path = destination.join(&enclosed);
        if entry.is_dir() {
            fs::create_dir_all(&output_path).map_err(|err| format!("create directory failed: {err}"))?;
            continue;
        }
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent).map_err(|err| format!("create directory failed: {err}"))?;
        }
        let mut output_file = fs::File::create(&output_path).map_err(|err| format!("write file failed: {err}"))?;
        std::io::copy(&mut entry, &mut output_file).map_err(|err| format!("unzip file failed: {err}"))?;
        extracted_files += 1;
    }
    Ok(extracted_files)
}

#[tauri::command]
pub(crate) fn export_migration_zip(
    state: State<AppState>,
    file_path: String,
) -> Result<ExportMigrationZipResult, String> {
    let target_path = file_path.trim().to_string();
    if target_path.is_empty() {
        return Err("filePath cannot be empty".to_string());
    }
    let (rules, backup_root) = {
        let store = state
            .store
            .lock()
            .map_err(|_| "failed to lock app state".to_string())?;
        (store.rules.clone(), store.execution_config.backup_root.clone())
    };

    let temp_root = create_migration_temp_dir("export")?;
    let result = (|| -> Result<ExportMigrationZipResult, String> {
        let rules_dir = temp_root.join("rules");
        let backups_dir = temp_root.join("backups");
        fs::create_dir_all(&rules_dir).map_err(|err| format!("create export directory failed: {err}"))?;
        fs::create_dir_all(&backups_dir).map_err(|err| format!("create export directory failed: {err}"))?;
        write_pretty_json_file(&rules_dir.join("gamesaver-rules.json"), &rules)?;

        let mut processed_uids = HashSet::new();
        let mut backup_games = 0usize;
        let mut skipped_backup_games = 0usize;
        for rule in &rules {
            let game_uid = normalize_game_uid(&rule.game_uid);
            if game_uid.is_empty() || !processed_uids.insert(game_uid.clone()) {
                continue;
            }
            let uid_root = Path::new(&backup_root).join(&game_uid);
            let legacy_root = Path::new(&backup_root).join(rule.game_id.trim());
            let source_root = if uid_root.exists() {
                Some(uid_root)
            } else if legacy_root.exists() {
                Some(legacy_root)
            } else {
                None
            };

            if let Some(source_root) = source_root {
                let copied = sync_directory(&source_root, &backups_dir.join(&game_uid))?;
                if copied > 0 {
                    backup_games += 1;
                } else {
                    skipped_backup_games += 1;
                }
            } else {
                skipped_backup_games += 1;
            }
        }
        let manifest = serde_json::json!({
            "format": "gamesaver-migration-v1",
            "createdAt": now_iso_string(),
            "ruleCount": rules.len(),
            "backupGames": backup_games
        });
        write_pretty_json_file(&temp_root.join("manifest.json"), &manifest)?;
        let exported_files = zip_directory_contents(&temp_root, Path::new(&target_path))?;
        Ok(ExportMigrationZipResult {
            rule_count: rules.len(),
            backup_games,
            exported_files,
            skipped_backup_games,
        })
    })();
    let _ = fs::remove_dir_all(&temp_root);
    result
}

#[tauri::command]
pub(crate) fn import_migration_zip(
    app: AppHandle,
    state: State<AppState>,
    file_path: String,
) -> Result<ImportMigrationZipResult, String> {
    let source_path = file_path.trim().to_string();
    if source_path.is_empty() {
        return Err("filePath cannot be empty".to_string());
    }
    let temp_root = create_migration_temp_dir("import")?;
    let result = (|| -> Result<ImportMigrationZipResult, String> {
        unzip_archive_to_directory(Path::new(&source_path), &temp_root)?;
        let rules_file_path = [
            temp_root.join("rules").join("gamesaver-rules.json"),
            temp_root.join("gamesaver-rules.json"),
        ]
        .into_iter()
        .find(|path| path.exists())
        .ok_or_else(|| "migration zip missing rules/gamesaver-rules.json".to_string())?;
        let raw_rules = fs::read(&rules_file_path).map_err(|err| format!("read rules failed: {err}"))?;
        let rules_text = decode_text_bytes(&raw_rules);
        let rules_value: serde_json::Value =
            serde_json::from_str(&rules_text).map_err(|err| format!("parse rules failed: {err}"))?;
        let rules_array = rules_value
            .as_array()
            .ok_or_else(|| "rules json must be array".to_string())?;

        let mut imported = 0usize;
        let mut overwritten = 0usize;
        let mut skipped = 0usize;
        let mut imported_backup_games = 0usize;
        let mut copied_backup_files = 0usize;
        let mut skipped_backup_games = 0usize;

        let mut store = state
            .store
            .lock()
            .map_err(|_| "failed to lock app state".to_string())?;
        for item in rules_array {
            let parsed = match serde_json::from_value::<crate::shared::ImportRuleInput>(item.clone()) {
                Ok(value) => value,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let game_id = parsed.game_id.trim().to_string();
            let exe_hash = parsed.exe_hash.trim().to_ascii_lowercase();
            let confirmed_paths = parsed
                .confirmed_paths
                .into_iter()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .collect::<Vec<_>>();
            if game_id.is_empty() || exe_hash.is_empty() || confirmed_paths.is_empty() {
                skipped += 1;
                continue;
            }
            let now = now_iso_string();
            let game_uid = parsed
                .game_uid
                .map(|v| normalize_game_uid(&v))
                .filter(|v| !v.is_empty())
                .unwrap_or_else(new_game_uid);
            let created_at = parsed.created_at.unwrap_or_else(|| now.clone());
            let updated_at = parsed.updated_at.unwrap_or_else(|| now.clone());
            let confidence = parsed.confidence.unwrap_or(45);
            let enabled = parsed.enabled.unwrap_or(true);
            let incoming_rule_id = parsed.rule_id.unwrap_or_default();

            if let Some(existing) = store
                .rules
                .iter_mut()
                .find(|rule| !incoming_rule_id.is_empty() && rule.rule_id == incoming_rule_id)
            {
                existing.game_id = game_id;
                existing.game_uid = game_uid;
                existing.exe_hash = exe_hash;
                existing.confirmed_paths = confirmed_paths;
                existing.created_at = created_at;
                existing.updated_at = updated_at;
                existing.confidence = confidence;
                existing.enabled = enabled;
                overwritten += 1;
            } else {
                store.rules.push(GameSaveRule {
                    rule_id: if incoming_rule_id.is_empty() {
                        Uuid::new_v4().to_string()
                    } else {
                        incoming_rule_id
                    },
                    game_id,
                    game_uid,
                    exe_hash,
                    confirmed_paths,
                    created_at,
                    confidence,
                    enabled,
                    updated_at,
                });
                imported += 1;
            }
        }

        let backups_root = temp_root.join("backups");
        if backups_root.exists() {
            let entries = fs::read_dir(&backups_root).map_err(|err| format!("read backups failed: {err}"))?;
            for entry in entries.filter_map(Result::ok) {
                let file_type = entry.file_type().map_err(|err| format!("read backup entry failed: {err}"))?;
                if !file_type.is_dir() {
                    skipped_backup_games += 1;
                    continue;
                }
                let game_uid = normalize_game_uid(&entry.file_name().to_string_lossy());
                if game_uid.is_empty() {
                    skipped_backup_games += 1;
                    continue;
                }
                let copied = sync_directory(
                    &entry.path(),
                    &Path::new(&store.execution_config.backup_root).join(&game_uid),
                )?;
                copied_backup_files += copied;
                imported_backup_games += 1;
            }
        }

        JsonStoreRepository::new().normalize(&mut store);
        JsonStoreRepository::new().persist(&app, &store)?;
        Ok(ImportMigrationZipResult {
            imported_rules: imported,
            overwritten_rules: overwritten,
            skipped_rules: skipped,
            imported_backup_games,
            copied_backup_files,
            skipped_backup_games,
        })
    })();
    let _ = fs::remove_dir_all(&temp_root);
    result
}

#[tauri::command]
pub(crate) fn start_export_migration_zip_task(
    app: AppHandle,
    state: State<AppState>,
    file_path: String,
) -> Result<String, String> {
    if file_path.trim().is_empty() {
        return Err("filePath cannot be empty".to_string());
    }
    let task_id = Uuid::new_v4().to_string();
    let now = now_iso_string();
    let task = BackgroundTask {
        task_id: task_id.clone(),
        task_type: "export_migration_zip".to_string(),
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
    let file_path_for_thread = file_path.trim().to_string();
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        update_background_task(
            &app_handle,
            &task_id_for_thread,
            "running",
            Some(5),
            Some("starting migration export".to_string()),
            None,
            None,
        );
        let app_state: State<AppState> = app_handle.state();
        match export_migration_zip(app_state, file_path_for_thread) {
            Ok(summary) => update_background_task(
                &app_handle,
                &task_id_for_thread,
                "success",
                Some(100),
                Some("migration export completed".to_string()),
                serde_json::to_value(summary).ok(),
                None,
            ),
            Err(err) => update_background_task(
                &app_handle,
                &task_id_for_thread,
                "failed",
                Some(100),
                Some("migration export failed".to_string()),
                None,
                Some(err),
            ),
        }
    });
    Ok(task_id)
}

#[tauri::command]
pub(crate) fn start_import_migration_zip_task(
    app: AppHandle,
    state: State<AppState>,
    file_path: String,
) -> Result<String, String> {
    if file_path.trim().is_empty() {
        return Err("filePath cannot be empty".to_string());
    }
    let task_id = Uuid::new_v4().to_string();
    let now = now_iso_string();
    let task = BackgroundTask {
        task_id: task_id.clone(),
        task_type: "import_migration_zip".to_string(),
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
    let file_path_for_thread = file_path.trim().to_string();
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        update_background_task(
            &app_handle,
            &task_id_for_thread,
            "running",
            Some(5),
            Some("starting migration import".to_string()),
            None,
            None,
        );
        let app_state: State<AppState> = app_handle.state();
        match import_migration_zip(app_handle.clone(), app_state, file_path_for_thread) {
            Ok(summary) => update_background_task(
                &app_handle,
                &task_id_for_thread,
                "success",
                Some(100),
                Some("migration import completed".to_string()),
                serde_json::to_value(summary).ok(),
                None,
            ),
            Err(err) => update_background_task(
                &app_handle,
                &task_id_for_thread,
                "failed",
                Some(100),
                Some("migration import failed".to_string()),
                None,
                Some(err),
            ),
        }
    });
    Ok(task_id)
}
