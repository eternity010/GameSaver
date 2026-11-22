use crate::{
    app_state::AppState,
    domain::{
        AppStore, AppTask, GameBodyVersion, GameHealth, GameLifecycle, SaveProfile, SaveVersion,
        TaskStatus,
    },
    repositories::{GameRepository, SaveRepository},
    services::{GameBodyUpdateService, TaskService},
};
use std::{collections::HashMap, path::Path};
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
    GameBodyUpdateService::ensure_no_pending_update(&games_root, &game.game_uid)?;
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
    let restored_version_id = save_target.map(|version| version.version_id.clone());
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
            match SaveRepository::restore_after_body_swap(app, game, profile, target, |progress, message| {
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
                        app,
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
                app,
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
    if let Err(error) = record_update(
        &mut candidate,
        body_version,
        protected,
        restored_version_id,
    ) {
        drop(guard);
        return rollback_update(
            app,
            game,
            &swap,
            &journal_path,
            receipt,
            pending_protected.as_ref(),
            error,
        );
    }
    if let Some(receipt) = receipt.as_ref() {
        receipt.record_commit(&mut candidate);
    }
    if let Err(error) = GameRepository::persist(app, &candidate) {
        drop(guard);
        return rollback_update(
            app,
            game,
            &swap,
            &journal_path,
            receipt,
            pending_protected.as_ref(),
            format!("保存本体更新记录失败：{error}"),
        );
    }
    let save_cleanup = receipt.map_or(Ok(()), |receipt| {
        SaveRepository::try_finalize_restore(receipt, &mut candidate)
    });
    *guard = candidate;
    drop(guard);
    if let Some(version) = pending_protected.as_ref() {
        crate::repositories::release_pending_objects(version);
    }
    save_cleanup.map_err(|error| {
        format!("新版已安装，存档恢复事务尚未收尾；旧本体与事务均已保留，请重试：{error}")
    })?;
    let committed = std::collections::HashSet::from([crate::domain::path_utils::normalize_path(
        &swap.archive_path,
    )]);
    settle_update(app, &game.game_uid, &committed)
        .map_err(|error| format!("游戏本体已更新，但旧文件或事务清理未完成：{error}"))?;
    Ok(serde_json::json!({ "fileCount": plan.file_count, "totalBytes": plan.total_bytes }))
}

fn record_update(
    store: &mut AppStore,
    body_version: GameBodyVersion,
    protected: Option<SaveVersion>,
    restored_version_id: Option<String>,
) -> Result<(), String> {
    let game = store
        .games
        .iter_mut()
        .find(|game| game.game_uid == body_version.game_uid)
        .ok_or("游戏记录不存在")?;
    if let Some(version_id) = restored_version_id {
        game.latest_save_version_id = Some(version_id);
    }
    game.lifecycle = GameLifecycle::Active;
    game.health = GameHealth::Ready;
    if let Some(protected) = protected {
        store.save_versions.push(protected);
    }
    store.body_versions.push(body_version);
    Ok(())
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
        "正在恢复存档和本体更新事务",
        None,
    );
    let root = state.games_root()?;
    let has_update = GameBodyUpdateService::has_pending_update(&root, game_uid)?;
    state.with_store_mut(|store| {
        SaveRepository::recover_game_restore_journals(app, store, game_uid)
    })?;
    if !has_update {
        return Ok(serde_json::json!({ "committed": false }));
    }
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
    let remaining = state
        .games_root()
        .and_then(|root| GameBodyUpdateService::has_pending_update(&root, game_uid))
        .and_then(|has_update| {
            if has_update {
                Ok(true)
            } else {
                SaveRepository::has_pending_restore(app, game_uid)
            }
        });
    let pending = remaining.as_ref().copied().unwrap_or(true);
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
    state.finish_game_update(game_uid, pending);
    match result {
        Ok(summary) if !pending => TaskService::finish(
            &state,
            task_id,
            TaskStatus::Success,
            100,
            "游戏本体更新或事务恢复完成",
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
                .or_else(|| remaining.err())
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
    success
}

pub(crate) fn pending_updates_at_startup(
    games_root: &Path,
    tasks: &HashMap<String, AppTask>,
) -> Vec<String> {
    match GameBodyUpdateService::pending_game_uids(games_root) {
        Ok(uids) => uids,
        Err(error) => {
            crate::logging::warn(format!(
                "无法检查游戏库中的更新事务，将保留已知更新任务的恢复入口：{error}"
            ));
            let mut uids = tasks
                .values()
                .filter(|task| {
                    matches!(
                        task.task_type.as_str(),
                        "update_game_body" | "retry_game_body_update"
                    )
                        && (task.status == TaskStatus::Interrupted
                            || task.retry.as_ref().is_some_and(|retry| {
                                retry.operation == "retry_game_body_update"
                            }))
                })
                .filter_map(|task| task.game_uid.clone())
                .collect::<Vec<_>>();
            uids.sort_unstable();
            uids.dedup();
            uids
        }
    }
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
    app: &AppHandle,
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
    let save_rollback = receipt
        .map_or(Ok(()), SaveRepository::rollback_restore)
        .and_then(|()| SaveRepository::ensure_no_pending_restore(app, &game.game_uid));
    if let Err(save_error) = save_rollback {
        return Err(format!("{error}；存档回滚尚未收尾，本体和事务已保留：{save_error}"));
    }
    if let Err(body_error) = GameBodyUpdateService::rollback(Path::new(&game.managed_path), swap) {
        return Err(format!("{error}；游戏本体回滚失败：{body_error}"));
    }
    if let Err(cleanup_error) = GameBodyUpdateService::clear_journal(journal_path) {
        return Err(format!("{error}；更新事务清理失败：{cleanup_error}"));
    }
    Err(error)
}

fn now_iso() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs().to_string())
        .unwrap_or_else(|_| "0".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Game;

    #[test]
    fn update_records_the_actual_restored_version_instead_of_the_last_entry() {
        let mut game = Game::new_pending("Test", "game", "game.exe");
        let restored = SaveVersion::new(game.game_uid.clone(), "1".into(), Vec::new());
        let newer = SaveVersion::new(game.game_uid.clone(), "2".into(), Vec::new());
        game.latest_save_version_id = Some(restored.version_id.clone());
        let mut store = AppStore {
            games: vec![game],
            save_versions: vec![restored.clone(), newer],
            ..AppStore::default()
        };
        let body = GameBodyVersion {
            version_id: "updated-body".into(),
            game_uid: restored.game_uid.clone(),
            created_at: "3".into(),
            archive_path: String::new(),
            file_count: 1,
            total_bytes: 1,
            package_path: None,
            sha256: None,
            excluded_items: Vec::new(),
            upload_status: None,
            remote_path: None,
            remote_fs_id: None,
            remote_size: None,
        };
        record_update(
            &mut store,
            body.clone(),
            None,
            Some(restored.version_id.clone()),
        )
        .unwrap();
        assert_eq!(
            store.games[0].latest_save_version_id.as_deref(),
            Some(restored.version_id.as_str())
        );
        assert_eq!(store.save_versions.len(), 2);

        let protected = SaveVersion::new(restored.game_uid, "4".into(), Vec::new());
        let protected_id = protected.version_id.clone();
        record_update(&mut store, body, Some(protected), Some(protected_id.clone())).unwrap();
        assert_eq!(store.games[0].latest_save_version_id, Some(protected_id));
        assert_eq!(store.save_versions.len(), 3);
    }
}
