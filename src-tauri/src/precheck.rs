use crate::{
    app_state::AppState,
    path_utils::expand_confirmed_path_for_runtime,
    runtime::{file_sha256_hex, system_time_to_unix},
    shared::{
        GameLaunchPrecheck, GameSaveRule, LaunchPrecheckCheck, LaunchSyncDecision,
        SaveLocationSummary,
    },
    storage::{
        has_unresolved_primary_rule_conflict_for_exe_hash, match_enabled_rule_for_exe_hash,
        normalize_game_key, normalize_game_uid, select_rule_for_game,
    },
};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tauri::{AppHandle, State};
use walkdir::WalkDir;

const SYNC_TIMESTAMP_TOLERANCE_SECS: u64 = 120;

#[tauri::command]
pub(crate) fn precheck_game_launch(
    _app: AppHandle,
    state: State<AppState>,
    game_id: String,
) -> Result<GameLaunchPrecheck, String> {
    let trimmed_game_id = game_id.trim().to_string();
    if trimmed_game_id.is_empty() {
        return Err("gameId cannot be empty".to_string());
    }
    let normalized_game_key = normalize_game_key(&trimmed_game_id);
    if normalized_game_key.is_empty() {
        return Err("gameId cannot be empty".to_string());
    }

    let (rules, execution_config, selected_rule, preferred_exe_path) = {
        let store = state
            .store
            .lock()
            .map_err(|_| "failed to lock app state".to_string())?;
        let selected_rule = select_rule_for_game(&store, &trimmed_game_id);
        let preferred_exe_path = selected_rule.as_ref().and_then(|rule| {
            let game_uid = normalize_game_uid(&rule.game_uid);
            if game_uid.is_empty() {
                None
            } else {
                store.execution_config.preferred_exe_by_uid.get(&game_uid).cloned()
            }
        });
        (
            store.rules.clone(),
            store.execution_config.clone(),
            selected_rule,
            preferred_exe_path,
        )
    };

    let mut checks = Vec::new();
    let mut exe_hash: Option<String> = None;
    let mut matched_rule: Option<GameSaveRule> = None;
    checks.push(LaunchPrecheckCheck {
        key: "rule_available".to_string(),
        label: "Available rule".to_string(),
        ok: selected_rule.is_some(),
        detail: if let Some(rule) = selected_rule.as_ref() {
            format!("Selected rule: {} ({})", rule.game_id, rule.rule_id)
        } else {
            "No usable rule found yet. Learn and confirm a rule first.".to_string()
        },
    });

    let trimmed_exe_path = preferred_exe_path
        .as_ref()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    checks.push(LaunchPrecheckCheck {
        key: "exe_bound".to_string(),
        label: "Bound EXE".to_string(),
        ok: trimmed_exe_path.is_some(),
        detail: trimmed_exe_path
            .as_ref()
            .map(|value| format!("Current binding: {value}"))
            .unwrap_or_else(|| "No EXE bound yet. Choose an EXE first.".to_string()),
    });

    if let Some(exe_path_value) = trimmed_exe_path.as_ref() {
        let exe_path = Path::new(exe_path_value);
        let path_ok = exe_path.exists()
            && exe_path.is_file()
            && exe_path_value.to_ascii_lowercase().ends_with(".exe");
        checks.push(LaunchPrecheckCheck {
            key: "exe_exists".to_string(),
            label: "EXE accessible".to_string(),
            ok: path_ok,
            detail: if !exe_path.exists() {
                "Bound EXE does not exist anymore.".to_string()
            } else if !exe_path.is_file() {
                "Bound path is not a file.".to_string()
            } else if !exe_path_value.to_ascii_lowercase().ends_with(".exe") {
                "Bound path is not an .exe file.".to_string()
            } else {
                "EXE exists and is readable.".to_string()
            },
        });
        if path_ok {
            match file_sha256_hex(exe_path) {
                Ok(hash) => {
                    exe_hash = Some(hash.clone());
                    let (resolved_rule, hash_matched_any) = match_enabled_rule_for_exe_hash(
                        &rules,
                        &execution_config,
                        &hash,
                        Some(&normalized_game_key),
                    );
                    matched_rule = resolved_rule;
                    let primary_rule_resolved = !has_unresolved_primary_rule_conflict_for_exe_hash(
                        &rules,
                        &execution_config,
                        &hash,
                        Some(&normalized_game_key),
                    );
                    let match_ok = matched_rule.is_some();
                    checks.push(LaunchPrecheckCheck {
                        key: "rule_match".to_string(),
                        label: "Rule hash match".to_string(),
                        ok: match_ok,
                        detail: if let Some(rule) = matched_rule.as_ref() {
                            format!("Matched rule: {} ({})", rule.game_id, rule.rule_id)
                        } else if hash_matched_any {
                            "This EXE matched rules for another game, so the binding likely needs correction."
                                .to_string()
                        } else {
                            "No enabled rule matched this EXE hash.".to_string()
                        },
                    });
                    checks.push(LaunchPrecheckCheck {
                        key: "rule_primary".to_string(),
                        label: "Primary rule resolved".to_string(),
                        ok: primary_rule_resolved,
                        detail: if !match_ok {
                            "No matched rule for this game yet, so primary resolution cannot be confirmed."
                                .to_string()
                        } else if primary_rule_resolved {
                            "The effective rule for this EXE is uniquely resolved.".to_string()
                        } else {
                            "Multiple enabled rules match this EXE and no primary rule has been chosen yet."
                                .to_string()
                        },
                    });
                }
                Err(err) => {
                    checks.push(LaunchPrecheckCheck {
                        key: "rule_match".to_string(),
                        label: "Rule hash match".to_string(),
                        ok: false,
                        detail: format!("Failed to hash EXE: {err}"),
                    });
                    checks.push(LaunchPrecheckCheck {
                        key: "rule_primary".to_string(),
                        label: "Primary rule resolved".to_string(),
                        ok: false,
                        detail: "Could not hash the EXE, so rule conflict status is unknown.".to_string(),
                    });
                }
            }
        } else {
            checks.push(LaunchPrecheckCheck {
                key: "rule_match".to_string(),
                label: "Rule hash match".to_string(),
                ok: false,
                detail: "A valid EXE binding is required first.".to_string(),
            });
            checks.push(LaunchPrecheckCheck {
                key: "rule_primary".to_string(),
                label: "Primary rule resolved".to_string(),
                ok: false,
                detail: "A valid EXE binding is required first.".to_string(),
            });
        }
    } else {
        for (key, label) in [
            ("exe_exists", "EXE accessible"),
            ("rule_match", "Rule hash match"),
            ("rule_primary", "Primary rule resolved"),
        ] {
            checks.push(LaunchPrecheckCheck {
                key: key.to_string(),
                label: label.to_string(),
                ok: false,
                detail: "No EXE bound yet.".to_string(),
            });
        }
    }

    let resolved_rule_for_paths = matched_rule.as_ref().or(selected_rule.as_ref());
    let sync_decision = resolved_rule_for_paths.map(|rule| {
        build_launch_sync_decision(rule, &execution_config.backup_root, trimmed_exe_path.as_deref())
    });
    let path_resolution = if let Some(rule) = resolved_rule_for_paths {
        describe_rule_path_resolution(&rule.confirmed_paths, trimmed_exe_path.as_deref())
    } else {
        (false, "No rule is available yet for path resolution.".to_string())
    };
    checks.push(LaunchPrecheckCheck {
        key: "rule_path_resolution".to_string(),
        label: "Rule paths resolvable".to_string(),
        ok: path_resolution.0,
        detail: path_resolution.1,
    });

    let backup_probe_root = if let Some(rule) = resolved_rule_for_paths {
        let uid = normalize_game_uid(&rule.game_uid);
        if uid.is_empty() {
            Path::new(&execution_config.backup_root).to_path_buf()
        } else {
            backup_game_root(&execution_config.backup_root, &uid)
        }
    } else {
        Path::new(&execution_config.backup_root).to_path_buf()
    };
    let backup_writable = ensure_directory_writable(&backup_probe_root);
    checks.push(LaunchPrecheckCheck {
        key: "backup_writable".to_string(),
        label: "Backup writable".to_string(),
        ok: backup_writable.is_ok(),
        detail: match backup_writable {
            Ok(_) => format!("Writable: {}", backup_probe_root.to_string_lossy()),
            Err(err) => format!("Not writable: {err}"),
        },
    });
    let backup_ready = checks
        .iter()
        .find(|check| check.key == "backup_writable")
        .is_some_and(|check| check.ok);

    Ok(GameLaunchPrecheck {
        game_id: trimmed_game_id,
        preferred_exe_path: trimmed_exe_path,
        exe_hash,
        matched_rule_id: matched_rule.as_ref().map(|rule| rule.rule_id.clone()),
        backup_ready,
        sync_decision,
        checks,
        checked_at: crate::runtime::now_iso_string(),
    })
}

fn describe_rule_path_resolution(paths: &[String], exe_path: Option<&str>) -> (bool, String) {
    if paths.is_empty() {
        return (false, "The rule has no confirmed save paths.".to_string());
    }

    let mut summaries = Vec::new();
    let mut has_failure = false;
    for path in paths {
        match expand_confirmed_path_for_runtime(path, exe_path) {
            Ok(resolved) => summaries.push(format!("{path} -> {resolved}")),
            Err(err) => {
                has_failure = true;
                summaries.push(format!("{path} -> {err}"));
            }
        }
    }

    (!has_failure, summaries.join(" | "))
}

fn ensure_directory_writable(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|err| format!("create directory failed: {err}"))?;
    let probe = path.join(".gamesaver_write_probe.tmp");
    fs::write(&probe, b"ok").map_err(|err| format!("write probe file failed: {err}"))?;
    let _ = fs::remove_file(&probe);
    Ok(())
}

fn backup_game_root(backup_root: &str, game_uid: &str) -> PathBuf {
    Path::new(backup_root).join(game_uid.trim())
}

fn ensure_backup_root_for_rule(rule: &GameSaveRule, backup_root: &str) -> Result<PathBuf, String> {
    let game_uid = normalize_game_uid(&rule.game_uid);
    if game_uid.is_empty() {
        return Err(format!("rule {} is missing gameUid", rule.rule_id));
    }
    Ok(backup_game_root(backup_root, &game_uid))
}

fn summarize_path_tree(path: &Path) -> (usize, u64, Option<u64>) {
    if !path.exists() {
        return (0, 0, None);
    }
    if path.is_file() {
        let metadata = match path.metadata() {
            Ok(metadata) => metadata,
            Err(_) => return (0, 0, None),
        };
        let modified = metadata.modified().ok().and_then(system_time_to_unix);
        return (1, metadata.len(), modified);
    }

    let mut file_count = 0usize;
    let mut total_bytes = 0u64;
    let mut latest_modified = None;
    for entry in WalkDir::new(path).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        file_count += 1;
        if let Ok(metadata) = entry.metadata() {
            total_bytes = total_bytes.saturating_add(metadata.len());
            if let Ok(modified) = metadata.modified() {
                if let Some(modified_unix) = system_time_to_unix(modified) {
                    latest_modified = Some(
                        latest_modified.map_or(modified_unix, |current: u64| current.max(modified_unix)),
                    );
                }
            }
        }
    }
    (file_count, total_bytes, latest_modified)
}

fn summarize_runtime_paths(paths: &[String], exe_path: Option<&str>) -> Result<SaveLocationSummary, String> {
    let mut resolved_paths = Vec::new();
    let mut file_count = 0usize;
    let mut total_bytes = 0u64;
    let mut latest_modified = None;

    for source in paths {
        let runtime_path = expand_confirmed_path_for_runtime(source, exe_path)?;
        resolved_paths.push(runtime_path.clone());
        let (path_file_count, path_total_bytes, path_latest_modified) =
            summarize_path_tree(Path::new(&runtime_path));
        file_count += path_file_count;
        total_bytes = total_bytes.saturating_add(path_total_bytes);
        if let Some(path_modified) = path_latest_modified {
            latest_modified = Some(latest_modified.map_or(path_modified, |current: u64| current.max(path_modified)));
        }
    }

    Ok(SaveLocationSummary {
        exists: file_count > 0,
        file_count,
        total_bytes,
        latest_modified_at: latest_modified.map(|value| value.to_string()),
        resolved_paths,
        latest_version_id: None,
    })
}

fn summarize_latest_backup_for_rule(rule: &GameSaveRule, backup_root: &str) -> Result<SaveLocationSummary, String> {
    let base = ensure_backup_root_for_rule(rule, backup_root)?;
    let latest_dir = base.join("latest");
    let latest_version_id = list_version_directories(&base.join("versions"))?
        .into_iter()
        .last()
        .map(|(version_id, _)| version_id);
    let (file_count, total_bytes, latest_modified) = summarize_path_tree(&latest_dir);
    Ok(SaveLocationSummary {
        exists: file_count > 0,
        file_count,
        total_bytes,
        latest_modified_at: latest_version_id.clone().or_else(|| latest_modified.map(|value| value.to_string())),
        resolved_paths: vec![latest_dir.to_string_lossy().to_string()],
        latest_version_id,
    })
}

fn build_launch_sync_decision(
    rule: &GameSaveRule,
    backup_root: &str,
    exe_path: Option<&str>,
) -> LaunchSyncDecision {
    let local_summary = summarize_runtime_paths(&rule.confirmed_paths, exe_path);
    let backup_summary = summarize_latest_backup_for_rule(rule, backup_root);

    let local_ok = local_summary.as_ref().ok().cloned();
    let backup_ok = backup_summary.as_ref().ok().cloned();

    if let Err(err) = local_summary {
        return LaunchSyncDecision {
            status: "conflict_unknown".to_string(),
            message: format!("Could not inspect local saves: {err}"),
            recommended_action: "launch_after_manual_review".to_string(),
            local_summary: None,
            backup_summary: backup_ok,
        };
    }
    if let Err(err) = backup_summary {
        return LaunchSyncDecision {
            status: "conflict_unknown".to_string(),
            message: format!("Could not inspect latest backup: {err}"),
            recommended_action: "launch_after_manual_review".to_string(),
            local_summary: local_ok,
            backup_summary: None,
        };
    }

    let local_summary = local_ok.unwrap_or(SaveLocationSummary {
        exists: false,
        file_count: 0,
        total_bytes: 0,
        latest_modified_at: None,
        resolved_paths: Vec::new(),
        latest_version_id: None,
    });
    let backup_summary = backup_ok.unwrap_or(SaveLocationSummary {
        exists: false,
        file_count: 0,
        total_bytes: 0,
        latest_modified_at: None,
        resolved_paths: Vec::new(),
        latest_version_id: None,
    });

    let local_ts = local_summary
        .latest_modified_at
        .as_deref()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let backup_ts = backup_summary
        .latest_modified_at
        .as_deref()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let same_shape = local_summary.file_count == backup_summary.file_count
        && local_summary.total_bytes == backup_summary.total_bytes;

    let within_tolerance = local_ts > 0
        && backup_ts > 0
        && local_ts.abs_diff(backup_ts) <= SYNC_TIMESTAMP_TOLERANCE_SECS;

    let (status, message, recommended_action) = if !backup_summary.exists {
        if local_summary.exists {
            (
                "local_only",
                "Local save data exists, but there is no backup yet.",
                "launch_direct",
            )
        } else {
            (
                "no_backup",
                "No backup exists yet, and no current local saves were detected.",
                "launch_direct",
            )
        }
    } else if !local_summary.exists {
        (
            "backup_only",
            "A backup exists, but the live save location is empty or missing.",
            "restore_then_launch",
        )
    } else if local_ts == backup_ts || within_tolerance {
        (
            "in_sync",
            "Local saves and the latest backup appear to be in the same recent timeline.",
            "launch_direct",
        )
    } else if local_ts > 0 && backup_ts > 0 && local_ts > backup_ts {
        (
            "local_newer",
            "Local saves appear newer than the latest backup.",
            "launch_direct",
        )
    } else if local_ts > 0 && backup_ts > 0 && backup_ts > local_ts {
        (
            "backup_newer",
            "The latest backup appears newer than current local saves.",
            "restore_then_launch",
        )
    } else if same_shape {
        (
            "in_sync",
            "Local saves and the latest backup have the same file count and size footprint.",
            "launch_direct",
        )
    } else {
        (
            "conflict_unknown",
            "Both local saves and backups exist, but freshness could not be safely determined.",
            "launch_after_manual_review",
        )
    };

    LaunchSyncDecision {
        status: status.to_string(),
        message: message.to_string(),
        recommended_action: recommended_action.to_string(),
        local_summary: Some(local_summary),
        backup_summary: Some(backup_summary),
    }
}

fn list_version_directories(versions_dir: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    if !versions_dir.exists() {
        return Ok(Vec::new());
    }
    let mut dirs = fs::read_dir(versions_dir)
        .map_err(|err| format!("read versions directory failed: {err}"))?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|entry| {
            let version_id = entry.file_name().to_string_lossy().to_string();
            if version_id.trim().is_empty() {
                None
            } else {
                Some((version_id, entry.path()))
            }
        })
        .collect::<Vec<_>>();
    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(dirs)
}
