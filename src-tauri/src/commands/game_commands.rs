pub(crate) use crate::services::game_cover_service::safe_cover_path;
use crate::services::game_cover_service::validate_component;
use crate::{
    app_state::AppState,
    domain::{compare_created_at, CoverCrop, CoverPosition, GameCover, TaskCategory, TaskStatus},
    repositories::{GameRepository, SaveRepository},
    services::{
        CoverCaptureService, GameLibraryService, GameRemovalService, RemovalKind, RemovalOutcome,
        RemovalRecovery, TaskService,
    },
};
use std::{fs, path::Path};
use tauri::{AppHandle, Manager, State};

#[tauri::command]
pub fn list_games(state: State<AppState>) -> Result<Vec<crate::domain::Game>, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "lock GameSaver store failed".to_string())?;
    Ok(GameLibraryService::list(&store))
}

#[tauri::command]
pub fn get_game(
    state: State<AppState>,
    game_uid: String,
) -> Result<Option<crate::domain::Game>, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "lock GameSaver store failed".to_string())?;
    Ok(GameLibraryService::find(&store, game_uid.trim()))
}

pub fn validate_game_display_name(new_display_name: &str) -> Result<String, String> {
    let new_name = new_display_name.trim();
    if new_name.is_empty() {
        return Err("游戏名称不能为空".to_string());
    }
    if new_name.chars().count() > 100 {
        return Err("游戏名称不能超过 100 个字符".to_string());
    }
    if new_name.chars().any(char::is_control) {
        return Err("游戏名称包含非法控制字符".to_string());
    }
    Ok(new_name.to_string())
}

#[tauri::command]
pub fn rename_game(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
    new_display_name: String,
) -> Result<crate::domain::Game, String> {
    let game_uid = game_uid.trim().to_string();
    let new_name = validate_game_display_name(&new_display_name)?;

    let updated_game = state.with_store_mut(|candidate| {
        let game = candidate
            .games
            .iter_mut()
            .find(|game| game.game_uid == game_uid)
            .ok_or_else(|| "游戏不存在".to_string())?;

        game.display_name = new_name.to_string();
        let updated_game = game.clone();

        GameRepository::persist(&app, candidate)?;
        Ok(updated_game)
    })?;

    maybe_sync_catalog_to_cloud_async(app, updated_game.clone());

    Ok(updated_game)
}

fn maybe_sync_catalog_to_cloud_async(app: AppHandle, game: crate::domain::Game) {
    if game.game_key.trim().is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let Ok(client) = crate::commands::baidu_commands::load_baidu_client(&app) else {
            return;
        };
        let Ok(remote_dir) = crate::commands::baidu_commands::remote_body_dir(&game.game_key)
        else {
            return;
        };
        let Ok(remote_files) = client.list(&remote_dir) else {
            return;
        };
        let has_cloud_game = remote_files
            .iter()
            .any(|file| file.path.ends_with("/manifest.json") || file.path.ends_with("/game.json"));
        if !has_cloud_game {
            return;
        }
        let Ok(base_data_dir) = crate::commands::baidu_commands::app_data_dir(&app) else {
            return;
        };
        let temporary_root = base_data_dir.join("cloud-manifest-temp");
        let cache_root = base_data_dir.join("cloud-manifest-cache");
        let catalog = crate::services::CloudManifestService::catalog_from_game(&game);
        let _ = crate::services::CloudManifestService::write_catalog(
            &client,
            &remote_dir,
            &catalog,
            &temporary_root,
        );
        app.state::<AppState>().invalidate_cloud_store(false);
        if let Some(remote_catalog) = remote_files.iter().find(|file| {
            file.path == crate::services::CloudManifestService::catalog_path(&remote_dir)
        }) {
            let _ = crate::services::CloudManifestService::save_cached_catalog(
                &cache_root,
                &remote_dir,
                remote_catalog,
                &catalog,
            );
        }
    });
}

#[tauri::command]
pub async fn save_game_cover(
    app: AppHandle,
    game_uid: String,
    original_bytes: Vec<u8>,
    display_bytes: Vec<u8>,
    original_extension: String,
    crop: CoverCrop,
    position: CoverPosition,
) -> Result<GameCover, String> {
    super::run_blocking(move || {
        crate::services::game_cover_service::save_game_cover(
            &app,
            &game_uid,
            &original_bytes,
            &display_bytes,
            &original_extension,
            crop,
            position,
        )
    })
    .await
}

#[tauri::command]
pub async fn arm_game_cover_capture(
    app: AppHandle,
    game_uid: String,
) -> Result<crate::services::CaptureArmView, String> {
    super::run_blocking(move || CoverCaptureService::arm_for_game(&app, &game_uid)).await
}

#[tauri::command]
pub fn discard_game_cover_capture(capture_id: String) -> Result<(), String> {
    let capture_id = capture_id.trim();
    validate_component(capture_id, "封面截图标识")?;
    CoverCaptureService::discard(capture_id);
    Ok(())
}

#[tauri::command]
pub fn get_game_cover(state: State<AppState>, game_uid: String) -> Result<Option<Vec<u8>>, String> {
    let game_uid = game_uid.trim();
    validate_component(game_uid, "游戏标识")?;
    let cover = {
        let store = state
            .store
            .lock()
            .map_err(|_| "读取游戏封面记录失败".to_string())?;
        GameLibraryService::find(&store, game_uid).and_then(|game| game.cover)
    };
    let Some(cover) = cover else {
        return Ok(None);
    };
    let root = state.library_root_path()?;
    let path = safe_cover_path(&root, game_uid, &cover.display_path)?;
    if !path.is_file() {
        return Ok(None);
    }
    fs::read(path)
        .map(Some)
        .map_err(|error| format!("读取游戏封面失败：{error}"))
}

#[tauri::command]
pub fn get_game_cover_path(
    state: State<AppState>,
    game_uid: String,
) -> Result<Option<String>, String> {
    let game_uid = game_uid.trim();
    validate_component(game_uid, "游戏标识")?;
    let cover = {
        let store = state
            .store
            .lock()
            .map_err(|_| "读取游戏封面记录失败".to_string())?;
        GameLibraryService::find(&store, game_uid).and_then(|game| game.cover)
    };
    let Some(cover) = cover else {
        return Ok(None);
    };
    let root = state.library_root_path()?;
    let path = safe_cover_path(&root, game_uid, &cover.display_path)?;
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(path.to_string_lossy().to_string()))
}

#[tauri::command]
pub fn get_game_cover_paths(
    state: State<AppState>,
) -> Result<std::collections::HashMap<String, String>, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "读取游戏库失败".to_string())?;
    let root = state.library_root_path()?;
    let mut paths = std::collections::HashMap::new();
    for game in GameLibraryService::list(&store) {
        if let Some(cover) = game.cover {
            if let Ok(path) = safe_cover_path(&root, &game.game_uid, &cover.display_path) {
                if path.is_file() {
                    paths.insert(game.game_uid, path.to_string_lossy().to_string());
                }
            }
        }
    }
    Ok(paths)
}

#[tauri::command]
pub fn remove_game_from_library(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
) -> Result<(), String> {
    start_game_removal(app, &state, game_uid, Some(RemovalKind::Delete)).map(|_| ())
}

#[tauri::command]
pub fn retry_game_removal(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
) -> Result<String, String> {
    start_game_removal(app, &state, game_uid, None)
}

pub(crate) fn start_game_removal(
    app: AppHandle,
    state: &AppState,
    game_uid: String,
    kind: Option<RemovalKind>,
) -> Result<String, String> {
    start_game_removal_checked(app, state, game_uid, kind, false)
}

pub(crate) fn start_pending_game_removal(
    app: AppHandle,
    state: &AppState,
    game_uid: String,
) -> Result<String, String> {
    start_game_removal_checked(app, state, game_uid, Some(RemovalKind::Delete), true)
}

fn start_game_removal_checked(
    app: AppHandle,
    state: &AppState,
    game_uid: String,
    kind: Option<RemovalKind>,
    pending_only: bool,
) -> Result<String, String> {
    let game_uid = game_uid.trim().to_string();
    if game_uid.is_empty() {
        return Err("游戏标识不能为空".into());
    }
    state.claim_game_removal(&game_uid, kind.is_none())?;
    let context = (|| {
        let root = state.library_root_path()?;
        let pending = GameRemovalService::pending_game_uids(&root)?;
        let game = if kind.is_some() {
            if pending.contains(&game_uid) {
                return Err("该游戏有未完成的删除事务，请在传输中心重试".into());
            }
            let store = state.store.lock().map_err(|_| "读取游戏记录失败")?;
            let game = GameLibraryService::find(&store, &game_uid).ok_or("游戏不存在")?;
            if pending_only && game.lifecycle != crate::domain::GameLifecycle::PendingSetup {
                return Err("只能放弃等待设置的游戏".into());
            }
            GameRemovalService::validate_game(&root, &game)?;
            Some(game)
        } else {
            if !pending.contains(&game_uid) {
                return Err("该游戏没有待恢复的卸载或删除事务".into());
            }
            None
        };
        Ok::<_, String>((root, game))
    })();
    let (root, game) = match context {
        Ok(context) => context,
        Err(error) => {
            let pending = state
                .library_root_path()
                .ok()
                .and_then(|root| GameRemovalService::pending_game_uids(&root).ok())
                .is_none_or(|pending| pending.contains(&game_uid));
            state.finish_game_removal(&game_uid, pending);
            return Err(error);
        }
    };
    let task_type = match kind {
        Some(RemovalKind::Delete) => "remove_game_from_library",
        Some(RemovalKind::Uninstall) => "uninstall_game_body",
        None => "retry_game_removal",
    };
    let task_id = match TaskService::create(
        state,
        task_type,
        TaskCategory::BodyTransfer,
        Some(game_uid.clone()),
        "准备卸载、删除或恢复未完成的清理",
    ) {
        Ok(task_id) => task_id,
        Err(error) => {
            state.finish_game_removal(&game_uid, kind.is_none());
            return Err(error);
        }
    };
    let worker_app = app.clone();
    let worker_uid = game_uid.clone();
    let worker_task = task_id.clone();
    let worker_root = root.clone();
    let spawn = std::thread::Builder::new()
        .name("game-removal".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_game_removal(
                    &worker_app,
                    &worker_task,
                    &worker_uid,
                    &worker_root,
                    game,
                    kind,
                )
            }))
            .unwrap_or_else(|_| Err("卸载或删除线程异常中止，已保留事务现场，请重试".into()));
            finish_game_removal_task(&worker_app, &worker_task, &worker_uid, &worker_root, result);
        });
    if let Err(error) = spawn {
        finish_game_removal_task(
            &app,
            &task_id,
            &game_uid,
            &root,
            Err(format!("启动删除线程失败：{error}")),
        );
        return Err(format!("启动删除线程失败：{error}"));
    }
    Ok(task_id)
}

fn run_game_removal(
    app: &AppHandle,
    task_id: &str,
    game_uid: &str,
    root: &Path,
    game: Option<crate::domain::Game>,
    kind: Option<RemovalKind>,
) -> Result<RemovalOutcome, String> {
    let state = app.state::<AppState>();
    let plan = if let (Some(game), Some(kind)) = (game, kind) {
        let mut plan = GameRemovalService::prepare(root, &game, kind)?;
        if let Err(error) = TaskService::begin_commit(&state, task_id) {
            GameRemovalService::abort(root, &plan)?;
            return Err(error);
        }
        TaskService::update(
            &state,
            task_id,
            TaskStatus::Running,
            15,
            "正在提交卸载或删除状态",
            None,
        );
        let warning = state.with_store_mut(|store| {
            GameRemovalService::commit(root, &mut plan, store, |candidate| {
                GameRepository::persist(app, candidate)
            })
        })?;
        if let Some(warning) = warning {
            return Err(warning);
        }
        plan
    } else {
        TaskService::begin_commit(&state, task_id)?;
        let recovered = state.with_store_mut(|store| {
            GameRemovalService::recover_metadata(root, game_uid, store, |candidate| {
                GameRepository::persist(app, candidate)
            })
        })?;
        match recovered {
            RemovalRecovery::Cleanup(plan) => plan,
            RemovalRecovery::Finished(outcome) => return Ok(outcome),
        }
    };
    GameRemovalService::cleanup(
        root,
        &plan,
        || {
            if plan.kind == RemovalKind::Uninstall {
                return state.with_store_mut(|store| {
                    if GameRemovalService::forget_local_packages(store, game_uid, None) {
                        GameRepository::persist(app, store)?;
                    }
                    Ok(0)
                });
            }
            let store = state.store.lock().map_err(|_| "读取存档引用失败")?;
            SaveRepository::collect_garbage_in_library(root, &store.save_versions)
        },
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
    )
}

pub(crate) fn finish_game_removal_task(
    app: &AppHandle,
    task_id: &str,
    game_uid: &str,
    root: &Path,
    result: Result<RemovalOutcome, String>,
) {
    let state = app.state::<AppState>();
    let pending = GameRemovalService::pending_game_uids(root)
        .map_or(true, |pending| pending.contains(&game_uid.to_string()));
    if pending {
        if let Err(error) = TaskService::set_retry(
            &state,
            task_id,
            crate::domain::TaskRetry {
                operation: "retry_game_removal".into(),
                game_uid: game_uid.into(),
                game_key: None,
                version_id: None,
                remote_path: None,
                remote_fs_id: None,
            },
        ) {
            crate::logging::error(format!("保存删除重试信息失败：{error}"));
        }
    }
    match result {
        Ok(outcome) if !outcome.cleanup_pending && !pending => {
            let message = if outcome.rolled_back {
                "已恢复中断前的游戏本体，本次卸载或删除已撤销"
            } else {
                "本地卸载或删除已完成，相关磁盘文件已清理"
            };
            TaskService::finish(
                &state,
                task_id,
                TaskStatus::Success,
                100,
                message,
                Some(serde_json::json!(outcome)),
                None,
            );
        }
        Ok(outcome) => {
            let detail = if outcome.failed_paths.is_empty() {
                "事务记录尚未清理，请重试".into()
            } else {
                outcome.failed_paths.join("；")
            };
            TaskService::finish(
                &state,
                task_id,
                TaskStatus::Failed,
                99,
                "游戏记录已更新，但磁盘清理未完成；可重试或重启后继续",
                Some(serde_json::json!(outcome)),
                Some(detail),
            );
        }
        Err(error) if error == "任务已取消" && !pending => {
            TaskService::finish(
                &state,
                task_id,
                TaskStatus::Cancelled,
                0,
                "已取消操作，游戏本体及记录保持不变",
                None,
                None,
            );
        }
        Err(error) => {
            TaskService::finish(
                &state,
                task_id,
                TaskStatus::Failed,
                99,
                if pending {
                    "操作未完成，现场已保留；可重试或重启恢复"
                } else {
                    "卸载或删除失败"
                },
                None,
                Some(error),
            );
        }
    }
    state.finish_game_removal(game_uid, pending);
}

pub(crate) fn record_recovered_game_removals(
    app: &AppHandle,
    notices: Vec<crate::services::game_removal_service::RemovalRecoveryNotice>,
) {
    let state = app.state::<AppState>();
    let root = match state.library_root_path() {
        Ok(root) => root,
        Err(error) => {
            crate::logging::error(format!("读取删除恢复的游戏库位置失败：{error}"));
            return;
        }
    };
    for notice in notices {
        let existing = state.tasks.lock().ok().and_then(|mut tasks| {
            let task_id = tasks
                .values()
                .filter(|task| {
                    task.game_uid.as_deref() == Some(&notice.game_uid)
                        && [
                            "uninstall_game_body",
                            "remove_game_from_library",
                            "retry_game_removal",
                        ]
                        .contains(&task.task_type.as_str())
                })
                .max_by(|left, right| compare_created_at(&left.created_at, &right.created_at))
                .map(|task| task.task_id.clone());
            if let Some(task) = task_id.as_ref().and_then(|task_id| tasks.get_mut(task_id)) {
                task.category = Some(TaskCategory::BodyTransfer);
                task.cancellable = Some(false);
            }
            task_id
        });
        let task_id = existing.map(Ok).unwrap_or_else(|| {
            TaskService::create(
                &state,
                "retry_game_removal",
                TaskCategory::BodyTransfer,
                Some(notice.game_uid.clone()),
                "正在恢复上次卸载或删除",
            )
        });
        match task_id {
            Ok(task_id) => {
                finish_game_removal_task(app, &task_id, &notice.game_uid, &root, notice.result)
            }
            Err(error) => crate::logging::error(format!("记录卸载或删除恢复结果失败：{error}")),
        }
    }
}

/// 详情页版本列表的传输摘要。
///
/// 详情页只展示「几个文件 / 多大 / 什么时候」，从不读取单条文件记录；而
/// [`crate::domain::SaveVersion`] 的 `files` 承载的是每个版本的全部文件清单
/// （相对路径 + 对象哈希 + 大小），体积随「版本数 × 文件数」线性膨胀。把这些
/// 清单原样序列化给前端，会让 WebView 主线程付出与页面展示无关的 JSON 解析成本，
/// 因此这里只传计数。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveVersionSummary {
    pub version_id: String,
    pub created_at: String,
    pub total_bytes: u64,
    pub file_count: usize,
    pub is_current: bool,
}

impl From<&crate::domain::SaveVersion> for SaveVersionSummary {
    fn from(version: &crate::domain::SaveVersion) -> Self {
        Self {
            version_id: version.version_id.clone(),
            created_at: version.created_at.clone(),
            total_bytes: version.total_bytes,
            file_count: version.files.len(),
            is_current: false,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameDetailView {
    pub precheck: crate::services::LaunchPrecheck,
    pub versions: Vec<SaveVersionSummary>,
    pub runtime: Option<crate::domain::GameRuntime>,
    pub body_versions: Vec<crate::commands::game_body_commands::GameBodyVersionView>,
    pub save_profile: Option<crate::domain::SaveProfile>,
}

pub fn query_game_detail_view(
    store: &crate::domain::AppStore,
    running_games: &std::collections::HashMap<String, crate::domain::GameRuntime>,
    game_uid: &str,
) -> Result<GameDetailView, String> {
    let game = GameLibraryService::find(store, game_uid).ok_or_else(|| "游戏不存在".to_string())?;
    let precheck = crate::services::LaunchService::precheck(store, game_uid)?;

    let current_version_id = game.latest_save_version_id.as_deref();
    let mut versions = store
        .save_versions
        .iter()
        .filter(|version| version.game_uid == game_uid)
        .map(|version| {
            let mut summary = SaveVersionSummary::from(version);
            summary.is_current = Some(version.version_id.as_str()) == current_version_id;
            summary
        })
        .collect::<Vec<_>>();
    versions.sort_by(|left, right| compare_created_at(&right.created_at, &left.created_at));

    let mut body_versions = store
        .body_versions
        .iter()
        .filter(|version| {
            version.game_uid == game_uid
                && (version
                    .package_path
                    .as_deref()
                    .is_some_and(|path| Path::new(path).is_file())
                    || (!version.archive_path.trim().is_empty()
                        && Path::new(&version.archive_path).is_dir()))
        })
        .cloned()
        .collect::<Vec<_>>();
    body_versions.sort_by(|left, right| compare_created_at(&right.created_at, &left.created_at));

    let body_version_views = body_versions
        .into_iter()
        .map(
            |version| crate::commands::game_body_commands::GameBodyVersionView {
                package_size: version
                    .package_path
                    .as_deref()
                    .and_then(|path| std::fs::metadata(path).ok())
                    .map(|metadata| metadata.len()),
                version,
            },
        )
        .collect();

    let save_profile = store
        .save_profiles
        .iter()
        .find(|p| {
            p.game_uid == game_uid
                && game.save_profile_id.as_deref() == Some(p.profile_id.as_str())
                && p.enabled
        })
        .cloned();

    let runtime = running_games.get(game_uid).cloned();

    Ok(GameDetailView {
        precheck,
        versions,
        runtime,
        body_versions: body_version_views,
        save_profile,
    })
}

#[tauri::command]
pub fn get_game_detail_view(
    state: State<AppState>,
    game_uid: String,
) -> Result<GameDetailView, String> {
    let game_uid = game_uid.trim();
    let store = state
        .store
        .lock()
        .map_err(|_| "lock GameSaver store failed".to_string())?;
    let running = state
        .running_games
        .lock()
        .map_err(|_| "lock running game state failed".to_string())?;
    query_game_detail_view(&store, &running, game_uid)
}

#[cfg(test)]
mod tests {
    use super::{query_game_detail_view, validate_game_display_name};
    use crate::domain::game::{CloudStatus, LaunchConfig};
    use crate::domain::{AppStore, Game, GameHealth, GameLifecycle};
    use std::collections::HashMap;

    #[test]
    fn query_game_detail_view_fails_when_game_missing() {
        let store = AppStore::default();
        let running = HashMap::new();
        let result = query_game_detail_view(&store, &running, "nonexistent");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "游戏不存在");
    }

    fn detail_view_game() -> Game {
        Game {
            game_uid: "test-uid-1".to_string(),
            game_key: "test-key".to_string(),
            display_name: "Test Game".to_string(),
            managed_path: "C:\\Games\\Test".to_string(),
            lifecycle: GameLifecycle::Active,
            health: GameHealth::Ready,
            cloud_status: CloudStatus::Disabled,
            save_profile_id: None,
            save_protection_skipped: false,
            launch: LaunchConfig {
                executable_relative_path: "test.exe".to_string(),
                arguments: vec![],
                working_directory_relative_path: None,
            },
            cover: None,
            last_played_at: None,
            latest_save_version_id: None,
            added_at: None,
            pending_body_upload: None,
        }
    }

    #[test]
    fn query_game_detail_view_returns_aggregated_details() {
        let mut store = AppStore::default();
        let running = HashMap::new();
        store.games.push(detail_view_game());

        let detail = query_game_detail_view(&store, &running, "test-uid-1").unwrap();
        assert_eq!(detail.precheck.game_uid, "test-uid-1");
        assert_eq!(detail.versions.len(), 0);
        assert_eq!(detail.body_versions.len(), 0);
        assert!(detail.runtime.is_none());
        assert!(detail.save_profile.is_none());
    }

    #[test]
    fn detail_view_versions_carry_counts_but_not_file_lists() {
        use crate::domain::{SaveFileEntry, SaveRootType, SaveVersion};

        let entry = |name: &str| SaveFileEntry {
            root_type: SaveRootType::SavedGames,
            root_path: None,
            relative_path: name.to_string(),
            object_hash: Some("a".repeat(64)),
            size: 2048,
            deleted: false,
            mtime_ms: None,
        };

        let mut store = AppStore::default();
        let running = HashMap::new();
        store.games.push(detail_view_game());
        store.save_versions.push(SaveVersion {
            version_id: "v1".to_string(),
            game_uid: "test-uid-1".to_string(),
            created_at: "1700000000".to_string(),
            total_bytes: 4096,
            files: vec![entry("slot1.sav"), entry("slot2.sav")],
        });

        let detail = query_game_detail_view(&store, &running, "test-uid-1").unwrap();
        assert_eq!(detail.versions.len(), 1);
        assert_eq!(detail.versions[0].file_count, 2);
        assert_eq!(detail.versions[0].total_bytes, 4096);

        // 详情页只消费计数，文件清单不得进入传输载荷。
        let version = serde_json::to_value(&detail).unwrap();
        assert!(
            version["versions"][0].get("files").is_none(),
            "详情页不应再序列化文件清单"
        );
        assert_eq!(version["versions"][0]["fileCount"], 2);
    }

    #[test]
    fn validates_valid_game_display_name() {
        assert_eq!(
            validate_game_display_name("  Cyberpunk 2077  ").unwrap(),
            "Cyberpunk 2077"
        );
        assert_eq!(
            validate_game_display_name("小猫的金币大冒险").unwrap(),
            "小猫的金币大冒险"
        );
    }

    #[test]
    fn rejects_empty_or_whitespace_game_display_name() {
        assert!(validate_game_display_name("").is_err());
        assert!(validate_game_display_name("   \t  ").is_err());
    }

    #[test]
    fn rejects_overly_long_game_display_name() {
        let long_name = "a".repeat(101);
        assert!(validate_game_display_name(&long_name).is_err());
    }

    #[test]
    fn rejects_control_characters_in_game_display_name() {
        assert!(validate_game_display_name("Game\nName").is_err());
        assert!(validate_game_display_name("Game\r\nName").is_err());
        assert!(validate_game_display_name("Game\0Name").is_err());
    }
}
