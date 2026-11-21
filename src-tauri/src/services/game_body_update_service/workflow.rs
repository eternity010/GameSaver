use crate::{
    app_state::AppState,
    domain::{GameBodyVersion, GameHealth, GameLifecycle, SaveProfile, SaveVersion, TaskStatus},
    repositories::{GameRepository, SaveRepository},
    services::{GameBodyUpdateService, TaskService},
};
use std::path::Path;
use tauri::{AppHandle, Manager};
use uuid::Uuid;

pub(crate) fn run_update(
    app: &AppHandle,
    task_id: &str,
    game: &crate::domain::Game,
    profile: &SaveProfile,
    latest: Option<&SaveVersion>,
    source: &Path,
) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    SaveRepository::ensure_no_pending_restore(app, &game.game_uid)?;
    let games_root = state.games_root()?;
    GameBodyUpdateService::validate_managed_game(&games_root, game)?;
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        1,
        "正在检查新版游戏目录",
        None,
    );
    let plan = GameBodyUpdateService::validate_source_with(
        source,
        game,
        |count, _| {
            TaskService::update(
                &state,
                task_id,
                TaskStatus::Running,
                3,
                format!("正在检查新版目录，已发现 {count} 个文件"),
                None,
            );
        },
        || TaskService::is_cancelled(&state, task_id),
    )?;
    let staging = GameBodyUpdateService::copy_to_staging(
        &plan,
        &games_root,
        &game.game_uid,
        |progress, message| {
            TaskService::update(
                &state,
                task_id,
                TaskStatus::Running,
                progress,
                message,
                None,
            );
        },
        || TaskService::is_cancelled(&state, task_id),
    )?;
    if TaskService::is_cancelled(&state, task_id) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err("任务已取消".to_string());
    }
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        92,
        "正在保护并恢复当前存档",
        None,
    );
    let protected = if Path::new(&game.managed_path).is_dir() {
        match SaveRepository::commit(app, game, profile, latest, |progress, message| {
            TaskService::update(
                &state,
                task_id,
                TaskStatus::Running,
                92 + progress / 20,
                message,
                None,
            );
        }) {
            Ok(protected) => protected,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(error);
            }
        }
    } else {
        None
    };
    let pending_protected = protected.clone();
    if TaskService::is_cancelled(&state, task_id) {
        if let Some(version) = pending_protected.as_ref() {
            crate::repositories::release_pending_objects(version);
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err("任务已取消".to_string());
    }
    let save_target = protected.as_ref().or(latest);
    let version_id = Uuid::new_v4().to_string();
    let archive_path = games_root
        .join(".versions")
        .join(&game.game_uid)
        .join(&version_id);
    let journal_path = GameBodyUpdateService::journal_path(&games_root, &game.game_uid);
    if let Err(error) = TaskService::begin_commit(&state, task_id) {
        if let Some(version) = pending_protected.as_ref() {
            crate::repositories::release_pending_objects(version);
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    if let Err(error) = GameBodyUpdateService::validate_managed_game(&games_root, game) {
        if let Some(version) = pending_protected.as_ref() {
            crate::repositories::release_pending_objects(version);
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    if let Err(error) = GameBodyUpdateService::write_journal(
        &games_root,
        &game.game_uid,
        Path::new(&game.managed_path),
        &staging,
        &archive_path,
    ) {
        if let Some(version) = pending_protected.as_ref() {
            crate::repositories::release_pending_objects(version);
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    let swap =
        match GameBodyUpdateService::swap(Path::new(&game.managed_path), &staging, &archive_path) {
            Ok(swap) => swap,
            Err(error) => {
                if let Some(version) = pending_protected.as_ref() {
                    crate::repositories::release_pending_objects(version);
                }
                return Err(error);
            }
        };
    let receipt = match save_target {
        Some(target) => {
            match SaveRepository::restore(app, game, profile, target, |progress, message| {
                TaskService::update(
                    &state,
                    task_id,
                    TaskStatus::Running,
                    95 + progress / 20,
                    message,
                    None,
                );
            }) {
                Ok(receipt) => Some(receipt),
                Err(error) => {
                    return rollback_update(
                        game,
                        &swap,
                        &journal_path,
                        None,
                        pending_protected.as_ref(),
                        format!("更新后恢复存档失败：{error}"),
                    );
                }
            }
        }
        None => None,
    };
    let body_version = GameBodyVersion {
        version_id: version_id.clone(),
        game_uid: game.game_uid.clone(),
        created_at: now_iso(),
        archive_path: swap.archive_path.to_string_lossy().to_string(),
        file_count: plan.file_count,
        total_bytes: plan.total_bytes,
        package_path: None,
        sha256: None,
        excluded_items: Vec::new(),
        upload_status: None,
        remote_path: None,
        remote_fs_id: None,
        remote_size: None,
    };
    // 全程持有 store 锁：此前是「取快照 → 释放锁 → 落盘 → 覆盖写回」，
    // 两次加锁之间若有并发写入（例如游戏退出提交存档），会被这里整块覆盖掉。
    let mut guard = match state.store.lock() {
        Ok(store) => store,
        Err(_) => {
            return rollback_update(
                game,
                &swap,
                &journal_path,
                receipt,
                pending_protected.as_ref(),
                "读取游戏更新记录失败".to_string(),
            )
        }
    };
    let mut candidate = guard.clone();
    if let Some(protected) = protected {
        candidate.save_versions.push(protected);
    }
    candidate.body_versions.push(body_version);
    let Some(game_record) = candidate
        .games
        .iter_mut()
        .find(|item| item.game_uid == game.game_uid)
    else {
        drop(guard);
        return rollback_update(
            game,
            &swap,
            &journal_path,
            receipt,
            pending_protected.as_ref(),
            "游戏记录不存在".to_string(),
        );
    };
    if let Some(protected) = candidate
        .save_versions
        .last()
        .filter(|version| version.game_uid == game.game_uid)
    {
        game_record.latest_save_version_id = Some(protected.version_id.clone());
    }
    game_record.lifecycle = GameLifecycle::Active;
    game_record.health = GameHealth::Ready;
    if let Some(receipt) = receipt.as_ref() {
        receipt.record_commit(&mut candidate);
    }
    if let Err(error) = GameRepository::persist(app, &candidate) {
        drop(guard);
        let save_rollback = receipt.map(SaveRepository::rollback_restore);
        let body_rollback = GameBodyUpdateService::rollback(Path::new(&game.managed_path), &swap);
        if let Some(version) = pending_protected.as_ref() {
            crate::repositories::release_pending_objects(version);
        }
        let journal_cleanup = if body_rollback.is_ok() && !matches!(save_rollback, Some(Err(_))) {
            GameBodyUpdateService::clear_journal(&journal_path)
        } else {
            Ok(())
        };
        let mut errors = vec![format!("保存本体更新记录失败：{error}")];
        if let Err(cleanup_error) = journal_cleanup {
            errors.push(format!("更新回滚完成，但事务清理失败：{cleanup_error}"));
        }
        if let Some(Err(save_error)) = save_rollback {
            errors.push(format!("存档回滚失败：{save_error}"));
        }
        if let Err(body_error) = body_rollback {
            errors.push(format!("游戏本体回滚失败：{body_error}"));
        }
        return Err(errors.join("；"));
    }
    if let Some(receipt) = receipt {
        SaveRepository::finalize_restore(receipt, &mut candidate);
    }
    *guard = candidate;
    drop(guard);
    if let Some(version) = pending_protected.as_ref() {
        crate::repositories::release_pending_objects(version);
    }
    let committed = std::collections::HashSet::from([crate::domain::path_utils::normalize_path(
        &swap.archive_path,
    )]);
    settle_update(app, &game.game_uid, &committed)
        .map_err(|error| format!("游戏本体已更新，但旧文件或事务清理未完成：{error}"))?;
    Ok(serde_json::json!({ "fileCount": plan.file_count, "totalBytes": plan.total_bytes }))
}

fn settle_update(
    app: &AppHandle,
    game_uid: &str,
    committed: &std::collections::HashSet<String>,
) -> Result<bool, String> {
    let state = app.state::<AppState>();
    let root = state.games_root()?;
    GameBodyUpdateService::recover_game_update(&root, game_uid, committed, |_| {
        state.with_store_mut(|store| {
            GameBodyUpdateService::clear_missing_archives(
                &root,
                game_uid,
                &mut store.body_versions,
            )?;
            GameRepository::persist(app, store)
        })
    })
}

pub(crate) fn retry_update(
    app: &AppHandle,
    task_id: &str,
    game_uid: &str,
) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    TaskService::begin_commit(&state, task_id)?;
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        90,
        "正在恢复更新事务并清理旧本体",
        None,
    );
    let committed = state
        .store
        .lock()
        .map_err(|_| "读取更新记录失败")?
        .body_versions
        .iter()
        .filter(|version| version.game_uid == game_uid && !version.archive_path.is_empty())
        .map(|version| crate::domain::path_utils::normalize_path(Path::new(&version.archive_path)))
        .collect();
    let committed = settle_update(app, game_uid, &committed)?;
    Ok(serde_json::json!({ "committed": committed }))
}

pub(crate) fn finish_update_task(
    app: &AppHandle,
    task_id: &str,
    game_uid: &str,
    result: Result<serde_json::Value, String>,
) -> bool {
    let state = app.state::<AppState>();
    let pending = state
        .games_root()
        .and_then(|root| GameBodyUpdateService::pending_game_uids(&root))
        .map_or(true, |uids| uids.iter().any(|uid| uid == game_uid));
    if pending {
        if let Err(error) = TaskService::set_retry(
            &state,
            task_id,
            crate::domain::TaskRetry {
                operation: "retry_game_body_update".into(),
                game_uid: game_uid.into(),
                game_key: None,
                version_id: None,
                remote_path: None,
                remote_fs_id: None,
            },
        ) {
            crate::logging::error(format!("保存游戏更新重试信息失败：{error}"));
        }
    }
    let success = result.is_ok() && !pending;
    match result {
        Ok(summary) if !pending => TaskService::finish(
            &state,
            task_id,
            TaskStatus::Success,
            100,
            "游戏本体更新或事务恢复完成，旧本体已清理",
            Some(summary),
            None,
        ),
        Err(error) if error == "任务已取消" && !pending => TaskService::finish(
            &state,
            task_id,
            TaskStatus::Cancelled,
            0,
            "已取消游戏本体更新",
            None,
            None,
        ),
        result => {
            let error = result
                .err()
                .unwrap_or_else(|| "更新事务记录尚未完成清理".into());
            TaskService::finish(
                &state,
                task_id,
                TaskStatus::Failed,
                99,
                if pending {
                    "游戏更新恢复或清理未完成，现场已保留，请重试"
                } else {
                    "游戏本体更新失败"
                },
                None,
                Some(error),
            );
        }
    }
    state.finish_game_update(game_uid, pending);
    success
}

pub(crate) fn record_pending_updates(app: &AppHandle, uids: Vec<String>) {
    let state = app.state::<AppState>();
    for uid in uids {
        let existing = state.tasks.lock().ok().and_then(|tasks| {
            tasks
                .values()
                .filter(|task| {
                    task.game_uid.as_deref() == Some(&uid)
                        && matches!(
                            task.task_type.as_str(),
                            "update_game_body" | "retry_game_body_update"
                        )
                        && matches!(task.status, TaskStatus::Failed | TaskStatus::Interrupted)
                })
                .max_by(|left, right| {
                    crate::domain::compare_created_at(&left.created_at, &right.created_at)
                })
                .map(|task| task.task_id.clone())
        });
        let task_id = match existing.map(Ok).unwrap_or_else(|| {
            TaskService::create(
                &state,
                "retry_game_body_update",
                crate::domain::TaskCategory::BodyTransfer,
                Some(uid.clone()),
                "游戏更新事务待恢复",
            )
        }) {
            Ok(id) => id,
            Err(error) => {
                crate::logging::error(format!("记录更新恢复任务失败：{error}"));
                continue;
            }
        };
        finish_update_task(
            app,
            &task_id,
            &uid,
            Err(
                "上次游戏更新事务尚未完成，相关游戏已暂时锁定；可重试恢复，详细原因请查看日志"
                    .into(),
            ),
        );
    }
}

fn rollback_update(
    game: &crate::domain::Game,
    swap: &crate::services::game_body_update_service::BodySwap,
    journal_path: &Path,
    receipt: Option<crate::repositories::save_repository::RestoreReceipt>,
    pending_version: Option<&crate::domain::SaveVersion>,
    error: String,
) -> Result<serde_json::Value, String> {
    if let Some(version) = pending_version {
        crate::repositories::release_pending_objects(version);
    }
    let save_rollback = receipt.map(SaveRepository::rollback_restore);
    let body_rollback = GameBodyUpdateService::rollback(Path::new(&game.managed_path), swap);
    let journal_cleanup = if body_rollback.is_ok() && !matches!(save_rollback, Some(Err(_))) {
        GameBodyUpdateService::clear_journal(journal_path)
    } else {
        Ok(())
    };
    let mut errors = vec![error];
    if let Some(Err(save_error)) = save_rollback {
        errors.push(format!("存档回滚失败：{save_error}"));
    }
    if let Err(body_error) = body_rollback {
        errors.push(format!("游戏本体回滚失败：{body_error}"));
    }
    if let Err(cleanup_error) = journal_cleanup {
        errors.push(format!("更新事务清理失败：{cleanup_error}"));
    }
    Err(errors.join("；"))
}

fn now_iso() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs().to_string())
        .unwrap_or_else(|_| "0".into())
}
