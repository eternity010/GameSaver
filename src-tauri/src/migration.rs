use crate::{
    app_state::{AppState, BackgroundTask},
    path_utils::normalize_paths,
    runtime::now_iso_string,
    shared::{ExportMigrationZipResult, GameSaveRule, ImportMigrationZipResult, PersistedStore, PreviewMigrationZipResult},
    storage::{decode_text_bytes, new_game_uid, normalize_exe_hash, normalize_game_key, normalize_game_uid, JsonStoreRepository, StoreRepository},
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
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};

const MIGRATION_FORMAT: &str = "gamesaver-migration-v1";
const MAX_MIGRATION_ENTRIES: usize = 100_000;
const MAX_MIGRATION_FILE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const MAX_MIGRATION_TOTAL_BYTES: u64 = 100 * 1024 * 1024 * 1024;
const MAX_MIGRATION_COMPRESSION_RATIO: u64 = 1_000;

struct DirectoryCleanup(PathBuf);

impl Drop for DirectoryCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

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
    for entry in WalkDir::new(source) {
        let entry = entry.map_err(|err| format!("scan directory failed: {err}"))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|err| format!("build relative path failed: {err}"))?;
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
    for entry in WalkDir::new(source_dir) {
        let entry = entry.map_err(|err| format!("scan export directory failed: {err}"))?;
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

fn unzip_archive_to_directory(
    zip_path: &Path,
    destination: &Path,
    expected_sha256: Option<&str>,
) -> Result<(usize, String), String> {
    if !zip_path.exists() {
        return Err("migration zip does not exist".to_string());
    }
    fs::create_dir_all(destination).map_err(|err| format!("create unzip directory failed: {err}"))?;
    let mut zip_file = fs::File::open(zip_path).map_err(|err| format!("open zip failed: {err}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = zip_file.read(&mut buffer).map_err(|err| format!("hash zip failed: {err}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let archive_sha256 = hex::encode(hasher.finalize());
    if let Some(expected) = expected_sha256.filter(|value| !value.trim().is_empty()) {
        if !archive_sha256.eq_ignore_ascii_case(expected.trim()) {
            return Err("migration zip changed after preview; preview it again before importing".to_string());
        }
    }
    zip_file.seek(SeekFrom::Start(0)).map_err(|err| format!("rewind zip failed: {err}"))?;
    let mut archive = zip::ZipArchive::new(zip_file).map_err(|err| format!("read zip failed: {err}"))?;
    if archive.len() > MAX_MIGRATION_ENTRIES {
        return Err(format!("migration zip contains too many entries: {}", archive.len()));
    }

    let mut extracted_files = 0usize;
    let mut total_uncompressed_bytes = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|err| format!("read zip entry failed: {err}"))?;
        let entry_name = entry.name().to_string();
        let entry_size = entry.size();
        if entry_size > MAX_MIGRATION_FILE_BYTES {
            return Err(format!("migration zip entry is too large: {entry_name}"));
        }
        total_uncompressed_bytes = total_uncompressed_bytes
            .checked_add(entry_size)
            .ok_or_else(|| "migration zip size overflow".to_string())?;
        if total_uncompressed_bytes > MAX_MIGRATION_TOTAL_BYTES {
            return Err("migration zip expands beyond the allowed total size".to_string());
        }
        let compressed_size = entry.compressed_size();
        if compressed_size > 0 && entry_size / compressed_size > MAX_MIGRATION_COMPRESSION_RATIO {
            return Err(format!("migration zip entry has an unsafe compression ratio: {entry_name}"));
        }
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
    Ok((extracted_files, archive_sha256))
}

fn validate_migration_manifest(temp_root: &Path) -> Result<serde_json::Value, String> {
    let manifest_path = temp_root.join("manifest.json");
    let raw = fs::read(&manifest_path).map_err(|err| format!("read manifest failed: {err}"))?;
    let manifest: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|err| format!("parse manifest failed: {err}"))?;
    let format = manifest
        .get("format")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "migration manifest is missing format".to_string())?;
    if format != MIGRATION_FORMAT {
        return Err(format!("unsupported migration format: {format}"));
    }
    Ok(manifest)
}

fn replace_file_from_temp(temp_path: &Path, target_path: &Path) -> Result<(), String> {
    let backup_path = target_path.with_extension(format!("bak-{}", Uuid::new_v4().simple()));
    let had_target = target_path.exists();
    if had_target {
        fs::rename(target_path, &backup_path).map_err(|err| format!("backup existing export failed: {err}"))?;
    }
    if let Err(err) = fs::rename(temp_path, target_path) {
        if had_target {
            let _ = fs::rename(&backup_path, target_path);
        }
        return Err(format!("replace export file failed: {err}"));
    }
    if had_target {
        let _ = fs::remove_file(backup_path);
    }
    Ok(())
}

fn parse_import_rules(rules_array: &[serde_json::Value]) -> (Vec<GameSaveRule>, usize) {
    let mut rules = Vec::new();
    let mut skipped = 0usize;
    for item in rules_array {
        let parsed = match serde_json::from_value::<crate::shared::ImportRuleInput>(item.clone()) {
            Ok(value) => value,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let game_id = parsed.game_id.trim().to_string();
        let exe_hash = normalize_exe_hash(&parsed.exe_hash);
        let confirmed_paths = normalize_paths(parsed.confirmed_paths, None);
        if game_id.is_empty() || exe_hash.is_empty() || confirmed_paths.is_empty() {
            skipped += 1;
            continue;
        }
        let now = now_iso_string();
        rules.push(GameSaveRule {
            rule_id: parsed
                .rule_id
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
            game_id,
            game_uid: parsed
                .game_uid
                .map(|value| normalize_game_uid(&value))
                .filter(|value| !value.is_empty())
                .unwrap_or_else(new_game_uid),
            exe_hash,
            confirmed_paths,
            created_at: parsed.created_at.unwrap_or_else(|| now.clone()),
            confidence: parsed.confidence.unwrap_or(45),
            enabled: parsed.enabled.unwrap_or(true),
            updated_at: parsed.updated_at.unwrap_or(now),
        });
    }
    (rules, skipped)
}

fn find_matching_rule_index(rules: &[GameSaveRule], incoming: &GameSaveRule) -> Option<usize> {
    rules
        .iter()
        .position(|rule| rule.rule_id == incoming.rule_id)
        .or_else(|| {
            let incoming_game = normalize_game_key(&incoming.game_id);
            let incoming_hash = normalize_exe_hash(&incoming.exe_hash);
            rules.iter().position(|rule| {
                normalize_game_key(&rule.game_id) == incoming_game
                    && normalize_exe_hash(&rule.exe_hash) == incoming_hash
            })
        })
}

fn apply_import_rules(store: &mut PersistedStore, incoming_rules: &[GameSaveRule]) -> (usize, usize) {
    let mut imported = 0usize;
    let mut overwritten = 0usize;
    for incoming in incoming_rules {
        if let Some(index) = find_matching_rule_index(&store.rules, incoming) {
            let mut replacement = incoming.clone();
            replacement.rule_id = store.rules[index].rule_id.clone();
            replacement.game_uid = store.rules[index].game_uid.clone();
            store.rules[index] = replacement;
            overwritten += 1;
        } else {
            store.rules.push(incoming.clone());
            imported += 1;
        }
    }
    (imported, overwritten)
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
            "format": MIGRATION_FORMAT,
            "createdAt": now_iso_string(),
            "ruleCount": rules.len(),
            "backupGames": backup_games
        });
        write_pretty_json_file(&temp_root.join("manifest.json"), &manifest)?;
        let target = Path::new(&target_path);
        let temp_output = target.with_extension(format!("zip.tmp-{}", Uuid::new_v4().simple()));
        let exported_files = match zip_directory_contents(&temp_root, &temp_output) {
            Ok(count) => count,
            Err(err) => {
                let _ = fs::remove_file(&temp_output);
                return Err(err);
            }
        };
        if let Err(err) = replace_file_from_temp(&temp_output, target) {
            let _ = fs::remove_file(&temp_output);
            return Err(err);
        }
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
pub(crate) fn preview_migration_zip(
    state: State<AppState>,
    file_path: String,
) -> Result<PreviewMigrationZipResult, String> {
    let source_path = file_path.trim().to_string();
    if source_path.is_empty() {
        return Err("filePath cannot be empty".to_string());
    }
    let temp_root = create_migration_temp_dir("preview")?;
    let result = (|| -> Result<PreviewMigrationZipResult, String> {
        let (_, archive_sha256) =
            unzip_archive_to_directory(Path::new(&source_path), &temp_root, None)?;
        let manifest = validate_migration_manifest(&temp_root)?;
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

        let store = {
            let store = state
                .store
                .lock()
                .map_err(|_| "failed to lock app state".to_string())?;
            store.clone()
        };
        let (incoming_rules, skipped_rules) = parse_import_rules(rules_array);
        let rule_count = incoming_rules.len();
        let mut preview_store = store.clone();
        let (new_rules, overwritten_rules) = apply_import_rules(&mut preview_store, &incoming_rules);
        let preview_rule_uids = preview_store
            .rules
            .iter()
            .map(|rule| normalize_game_uid(&rule.game_uid))
            .filter(|uid| !uid.is_empty())
            .collect::<HashSet<_>>();

        let backups_root = temp_root.join("backups");
        let (backup_games, backup_files, conflicting_backup_games) = if backups_root.exists() {
            let mut backup_games = 0usize;
            let mut conflicting_backup_games = 0usize;
            for entry in fs::read_dir(&backups_root).map_err(|err| format!("read backups failed: {err}"))? {
                let entry = entry.map_err(|err| format!("read backup entry failed: {err}"))?;
                if !entry.file_type().map_err(|err| format!("read backup type failed: {err}"))?.is_dir() {
                    continue;
                }
                backup_games += 1;
                let game_uid = normalize_game_uid(&entry.file_name().to_string_lossy());
                if game_uid.is_empty()
                    || !preview_rule_uids.contains(&game_uid)
                    || Path::new(&store.execution_config.backup_root).join(game_uid).exists()
                {
                    conflicting_backup_games += 1;
                }
            }
            let mut backup_files = 0usize;
            for entry in WalkDir::new(&backups_root) {
                let entry = entry.map_err(|err| format!("scan migration backups failed: {err}"))?;
                if entry.file_type().is_file() {
                    backup_files += 1;
                }
            }
            (backup_games, backup_files, conflicting_backup_games)
        } else {
            (0, 0, 0)
        };

        Ok(PreviewMigrationZipResult {
            rule_count,
            new_rules,
            overwritten_rules,
            skipped_rules,
            backup_games,
            backup_files,
            conflicting_backup_games,
            manifest_format: manifest
                .get("format")
                .and_then(|value| value.as_str())
                .map(ToString::to_string),
            created_at: manifest
                .get("createdAt")
                .and_then(|value| value.as_str())
                .map(ToString::to_string),
            archive_sha256,
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
    expected_archive_sha256: Option<String>,
) -> Result<ImportMigrationZipResult, String> {
    let source_path = file_path.trim().to_string();
    if source_path.is_empty() {
        return Err("filePath cannot be empty".to_string());
    }
    let temp_root = create_migration_temp_dir("import")?;
    let result = (|| -> Result<ImportMigrationZipResult, String> {
        unzip_archive_to_directory(
            Path::new(&source_path),
            &temp_root,
            expected_archive_sha256.as_deref(),
        )?;
        validate_migration_manifest(&temp_root)?;
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

        let (incoming_rules, skipped) = parse_import_rules(rules_array);
        let backup_root = {
            let store = state
                .store
                .lock()
                .map_err(|_| "failed to lock app state".to_string())?;
            store.execution_config.backup_root.clone()
        };
        let staging_root = Path::new(&backup_root)
            .join(".gamesaver-migration-stage")
            .join(Uuid::new_v4().to_string());
        fs::create_dir_all(&staging_root).map_err(|err| format!("create backup staging directory failed: {err}"))?;
        let _staging_cleanup = DirectoryCleanup(staging_root.clone());
        let mut staged_backups = Vec::new();
        let mut skipped_backup_games = 0usize;
        let backups_root = temp_root.join("backups");
        if backups_root.exists() {
            let entries = fs::read_dir(&backups_root).map_err(|err| format!("read backups failed: {err}"))?;
            for entry in entries {
                let entry = entry.map_err(|err| format!("read backup entry failed: {err}"))?;
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
                if Path::new(&backup_root).join(&game_uid).exists() {
                    skipped_backup_games += 1;
                    continue;
                }
                let staged_path = staging_root.join(&game_uid);
                let copied = sync_directory(&entry.path(), &staged_path)?;
                if copied == 0 {
                    skipped_backup_games += 1;
                    let _ = fs::remove_dir_all(staged_path);
                    continue;
                }
                staged_backups.push((game_uid, copied));
            }
        }
        let commit_result = (|| -> Result<ImportMigrationZipResult, String> {
            let mut store = state
                .store
                .lock()
                .map_err(|_| "failed to lock app state".to_string())?;
            if store.execution_config.backup_root != backup_root {
                return Err("backup root changed during migration import".to_string());
            }
            let mut candidate_store = store.clone();
            let (imported, overwritten) = apply_import_rules(&mut candidate_store, &incoming_rules);
            JsonStoreRepository::new().normalize(&mut candidate_store);
            let candidate_rule_uids = candidate_store
                .rules
                .iter()
                .map(|rule| normalize_game_uid(&rule.game_uid))
                .filter(|uid| !uid.is_empty())
                .collect::<HashSet<_>>();

            let mut moved_targets = Vec::new();
            let mut imported_backup_games = 0usize;
            let mut copied_backup_files = 0usize;
            let mut final_skipped_backup_games = skipped_backup_games;
            for (game_uid, copied) in &staged_backups {
                let staged_path = staging_root.join(game_uid);
                let target_path = Path::new(&backup_root).join(game_uid);
                if !candidate_rule_uids.contains(game_uid) || target_path.exists() {
                    final_skipped_backup_games += 1;
                    continue;
                }
                if let Err(err) = fs::rename(&staged_path, &target_path) {
                    for moved in moved_targets.iter().rev() {
                        let _ = fs::remove_dir_all(moved);
                    }
                    return Err(format!("commit imported backup failed: {err}"));
                }
                copied_backup_files += *copied;
                imported_backup_games += 1;
                moved_targets.push(target_path);
            }
            if let Err(err) = JsonStoreRepository::new().persist(&app, &candidate_store) {
                for moved in moved_targets.iter().rev() {
                    let _ = fs::remove_dir_all(moved);
                }
                return Err(err);
            }
            *store = candidate_store;
            Ok(ImportMigrationZipResult {
                imported_rules: imported,
                overwritten_rules: overwritten,
                skipped_rules: skipped,
                imported_backup_games,
                copied_backup_files,
                skipped_backup_games: final_skipped_backup_games,
            })
        })();
        commit_result
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
    expected_archive_sha256: Option<String>,
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
    let expected_archive_sha256_for_thread = expected_archive_sha256.clone();
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
        match import_migration_zip(
            app_handle.clone(),
            app_state,
            file_path_for_thread,
            expected_archive_sha256_for_thread,
        ) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_entry_normalization_rejects_parent_paths() {
        assert!(normalize_zip_entry_name(Path::new("rules/../outside.json")).is_err());
    }

    #[test]
    fn migration_rule_overwrite_preserves_local_uid() {
        let mut store = PersistedStore::default();
        store.rules.push(GameSaveRule {
            rule_id: "local".to_string(),
            game_id: "Example".to_string(),
            game_uid: "local-uid".to_string(),
            exe_hash: "abc".to_string(),
            confirmed_paths: vec!["old".to_string()],
            created_at: "1".to_string(),
            confidence: 10,
            enabled: true,
            updated_at: "1".to_string(),
        });
        let incoming = GameSaveRule {
            rule_id: "foreign".to_string(),
            game_id: "example".to_string(),
            game_uid: "foreign-uid".to_string(),
            exe_hash: "ABC".to_string(),
            confirmed_paths: vec!["new".to_string()],
            created_at: "2".to_string(),
            confidence: 80,
            enabled: true,
            updated_at: "2".to_string(),
        };
        let result = apply_import_rules(&mut store, &[incoming]);
        assert_eq!(result, (0, 1));
        assert_eq!(store.rules[0].rule_id, "local");
        assert_eq!(store.rules[0].game_uid, "local-uid");
    }
}
