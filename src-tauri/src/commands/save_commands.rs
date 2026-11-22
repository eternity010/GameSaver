use crate::{
    app_state::AppState,
    commands::save_version_commands::{release_maintenance, reserve_maintenance},
    domain::{
        path_utils::{normalize_path, strip_verbatim_prefix}, save_profile::SaveRootBinding,
        Game, GameLifecycle, SaveLearningResult, SaveProfile, SaveRootType, SaveScope,
        TaskCategory, TaskStatus,
    },
    repositories::{
        save_repository::{matching_directory_exclusion, matching_exclusion},
        GameRepository, SaveRepository,
    },
    services::{
        learning::stop_etw_capture, save_learning_service::ActiveLearningSession, GameLibraryService,
        SaveLearningService, TaskService,
    },
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};
use tauri::{AppHandle, Manager, State};

#[tauri::command]
pub fn get_pending_game_setup(
    state: State<AppState>,
    game_uid: Option<String>,
) -> Result<serde_json::Value, String> {
    let game_uid = game_uid.filter(|uid| !uid.trim().is_empty());
    let game = match game_uid.as_deref() {
        Some(uid) => {
            let store = state.store.lock().map_err(|_| "读取游戏记录失败")?;
            Some(GameLibraryService::find(&store, uid).ok_or("游戏记录已不存在")?)
        }
        None => None,
    };
    let session = state
        .learning_sessions
        .lock()
        .map_err(|_| "读取存档识别状态失败")?
        .values()
        .find(|active| Some(active.view.game_uid.as_str()) == game_uid.as_deref())
        .map(|active| (active.view.clone(), active.validation_only));
    let task = {
        let tasks = state.tasks.lock().map_err(|_| "读取添加任务失败")?;
        let mut relevant = tasks
            .values()
            .filter(|task| {
                let matches_game = if let Some(uid) = game_uid.as_deref() {
                    task.game_uid.as_deref() == Some(uid)
                } else {
                    task.task_type == "add_game"
                };
                matches_game
                    && [
                        "add_game",
                        "learn_saves",
                        "verify_save_candidates",
                        "analyze_saves",
                    ]
                    .contains(&task.task_type.as_str())
            })
            .collect::<Vec<_>>();
        relevant.sort_by(|left, right| {
            crate::domain::compare_created_at(&right.created_at, &left.created_at)
        });
        relevant
            .first()
            .filter(|task| {
                matches!(task.status, TaskStatus::Pending | TaskStatus::Running)
                    || (game_uid.is_some()
                        && task.task_type == "analyze_saves"
                        && task.status == TaskStatus::Success)
            })
            .map(|task| (task.task_id.clone(), task.task_type.clone()))
    };
    Ok(
        serde_json::json!({ "game": game, "session": session.as_ref().map(|item| &item.0),
        "validationOnly": session.as_ref().is_some_and(|item| item.1), "taskId": task.as_ref().map(|item| &item.0),
        "taskType": task.as_ref().map(|item| &item.1) }),
    )
}

#[tauri::command]
pub fn start_save_learning_task(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
) -> Result<String, String> {
    let game_uid = game_uid.trim().to_string();
    state.claim_learning_start(&game_uid)?;
    let mut reservation = SaveProfileOperationGuard {
        state: &state,
        key: game_uid.clone(),
        active: true,
    };
    let game = {
        let store = state
            .store
            .lock()
            .map_err(|_| "lock GameSaver store failed".to_string())?;
        let game =
            GameLibraryService::find(&store, &game_uid).ok_or_else(|| "游戏不存在".to_string())?;
        if !matches!(game.lifecycle, GameLifecycle::PendingSetup) {
            return Err("只有等待设置的游戏可以开始存档识别".to_string());
        }
        game
    };
    let task_id = TaskService::create(
        &state,
        "learn_saves",
        TaskCategory::Maintenance,
        Some(game_uid),
        "准备识别存档范围",
    )?;
    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    let worker_uid = game.game_uid.clone();
    let spawn = std::thread::Builder::new()
        .name("learn-saves".into())
        .spawn(move || {
            let worker_state = app_handle.state::<AppState>();
            let _reservation = SaveProfileOperationGuard {
                state: &worker_state,
                key: worker_uid,
                active: true,
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let current = {
                    let state = app_handle.state::<AppState>();
                    let store = state
                        .store
                        .lock()
                        .map_err(|_| "读取游戏记录失败".to_string())?;
                    let current =
                        GameLibraryService::find(&store, &game.game_uid).ok_or("游戏已不存在")?;
                    if current.lifecycle != GameLifecycle::PendingSetup {
                        return Err("游戏已完成设置".into());
                    }
                    current
                };
                SaveLearningService::start(
                    &app_handle,
                    &current,
                    |progress, message| {
                        TaskService::update(
                            &app_handle.state(),
                            &task_id_for_thread,
                            TaskStatus::Running,
                            progress,
                            message,
                            None,
                        )
                    },
                    || TaskService::is_cancelled(&app_handle.state(), &task_id_for_thread),
                )
            }))
            .unwrap_or_else(|_| Err("存档识别启动异常，请重新开始识别".into()));
            match result {
                Ok(active) => {
                    if TaskService::begin_commit(&app_handle.state(), &task_id_for_thread).is_err()
                    {
                        cleanup_active_learning(&active);
                        TaskService::finish(
                            &app_handle.state(),
                            &task_id_for_thread,
                            TaskStatus::Cancelled,
                            100,
                            "已取消存档识别",
                            None,
                            None,
                        );
                        return;
                    }
                    let view = active.view.clone();
                    let state: State<AppState> = app_handle.state();
                    match state.learning_sessions.lock() {
                        Ok(mut sessions) => {
                            sessions.insert(view.session_id.clone(), active);
                        }
                        Err(_) => {
                            cleanup_active_learning(&active);
                            TaskService::finish(
                                &state,
                                &task_id_for_thread,
                                TaskStatus::Failed,
                                100,
                                "保存存档识别会话失败",
                                None,
                                Some("存档识别状态不可用".into()),
                            );
                            return;
                        }
                    };
                    state.set_runtime(crate::domain::GameRuntime {
                        game_uid: view.game_uid.clone(),
                        pid: Some(view.root_pid),
                        status: crate::domain::GameRuntimeStatus::Running,
                        started_at: Some(view.started_at.clone()),
                        task_id: None,
                    });
                    TaskService::finish(
                        &state,
                        &task_id_for_thread,
                        TaskStatus::Success,
                        100,
                        "游戏已启动，请完成一次保存后回来分析",
                        serde_json::to_value(view).ok(),
                        None,
                    );
                }
                Err(error) if error == "任务已取消" => TaskService::finish(
                    &app_handle.state(),
                    &task_id_for_thread,
                    TaskStatus::Cancelled,
                    100,
                    "已取消存档识别",
                    None,
                    None,
                ),
                Err(error) => TaskService::finish(
                    &app_handle.state(),
                    &task_id_for_thread,
                    TaskStatus::Failed,
                    100,
                    "启动存档识别失败",
                    None,
                    Some(error),
                ),
            }
        });
    if let Err(error) = spawn {
        TaskService::finish(
            &state,
            &task_id,
            TaskStatus::Failed,
            0,
            "启动识别线程失败",
            None,
            Some(error.to_string()),
        );
        return Err(format!("启动识别线程失败：{error}"));
    }
    reservation.active = false;
    Ok(task_id)
}

#[tauri::command]
pub fn start_save_candidate_verification_task(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
    scopes: Vec<SaveScope>,
) -> Result<String, String> {
    let game_uid = game_uid.trim().to_string();
    state.claim_learning_start(&game_uid)?;
    let mut reservation = SaveProfileOperationGuard {
        state: &state,
        key: game_uid.clone(),
        active: true,
    };
    let game = {
        let store = state
            .store
            .lock()
            .map_err(|_| "lock GameSaver store failed".to_string())?;
        let game =
            GameLibraryService::find(&store, &game_uid).ok_or_else(|| "游戏不存在".to_string())?;
        if !matches!(game.lifecycle, GameLifecycle::PendingSetup) {
            return Err("只有等待设置的游戏可以再次验证存档候选".to_string());
        }
        game
    };
    if scopes.is_empty() {
        return Err("没有可再次验证的存档候选".to_string());
    }
    let task_id = TaskService::create(
        &state,
        "verify_save_candidates",
        TaskCategory::Maintenance,
        Some(game_uid),
        "准备再次验证存档候选",
    )?;
    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    let worker_uid = game.game_uid.clone();
    let spawn = std::thread::Builder::new()
        .name("verify-saves".into())
        .spawn(move || {
            let worker_state = app_handle.state::<AppState>();
            let _reservation = SaveProfileOperationGuard {
                state: &worker_state,
                key: worker_uid,
                active: true,
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                SaveLearningService::start_candidate_verification(
                    &app_handle,
                    &game,
                    &scopes,
                    |progress, message| {
                        TaskService::update(
                            &app_handle.state(),
                            &task_id_for_thread,
                            TaskStatus::Running,
                            progress,
                            message,
                            None,
                        )
                    },
                    || TaskService::is_cancelled(&app_handle.state(), &task_id_for_thread),
                )
            }))
            .unwrap_or_else(|_| Err("候选验证启动异常，请重新开始验证".into()));
            match result {
                Ok(active) => {
                    if TaskService::begin_commit(&app_handle.state(), &task_id_for_thread).is_err()
                    {
                        cleanup_active_learning(&active);
                        TaskService::finish(
                            &app_handle.state(),
                            &task_id_for_thread,
                            TaskStatus::Cancelled,
                            100,
                            "已取消再次验证",
                            None,
                            None,
                        );
                        return;
                    }
                    let view = active.view.clone();
                    let state: State<AppState> = app_handle.state();
                    match state.learning_sessions.lock() {
                        Ok(mut sessions) => {
                            sessions.insert(view.session_id.clone(), active);
                        }
                        Err(_) => {
                            cleanup_active_learning(&active);
                            TaskService::finish(
                                &state,
                                &task_id_for_thread,
                                TaskStatus::Failed,
                                100,
                                "保存验证会话失败",
                                None,
                                Some("存档识别状态不可用".into()),
                            );
                            return;
                        }
                    };
                    state.set_runtime(crate::domain::GameRuntime {
                        game_uid: view.game_uid.clone(),
                        pid: Some(view.root_pid),
                        status: crate::domain::GameRuntimeStatus::Running,
                        started_at: Some(view.started_at.clone()),
                        task_id: None,
                    });
                    TaskService::finish(
                        &state,
                        &task_id_for_thread,
                        TaskStatus::Success,
                        100,
                        "游戏已启动，请再次完成一次保存后回来分析",
                        serde_json::to_value(view).ok(),
                        None,
                    );
                }
                Err(error) if error == "任务已取消" => TaskService::finish(
                    &app_handle.state(),
                    &task_id_for_thread,
                    TaskStatus::Cancelled,
                    100,
                    "已取消再次验证",
                    None,
                    None,
                ),
                Err(error) => TaskService::finish(
                    &app_handle.state(),
                    &task_id_for_thread,
                    TaskStatus::Failed,
                    100,
                    "启动再次验证失败",
                    None,
                    Some(error),
                ),
            }
        });
    if let Err(error) = spawn {
        TaskService::finish(
            &state,
            &task_id,
            TaskStatus::Failed,
            0,
            "启动验证线程失败",
            None,
            Some(error.to_string()),
        );
        return Err(format!("启动验证线程失败：{error}"));
    }
    reservation.active = false;
    Ok(task_id)
}

#[tauri::command]
pub async fn start_finish_save_learning_task(
    app: AppHandle,
    session_id: String,
) -> Result<String, String> {
    super::run_blocking(move || start_finish_save_learning(app, session_id)).await
}

fn start_finish_save_learning(app: AppHandle, session_id: String) -> Result<String, String> {
    let state = app.state::<AppState>();
    let session_id = session_id.trim().to_string();
    let game_uid = state
        .learning_sessions
        .lock()
        .map_err(|_| "lock learning session state failed".to_string())?
        .get(&session_id)
        .map(|active| active.view.game_uid.clone())
        .ok_or_else(|| "存档识别会话不存在".to_string())?;
    state.claim_operation(&game_uid, "该游戏正在分析或执行其他操作")?;
    let mut reservation = SaveProfileOperationGuard {
        state: &state,
        key: AppState::game_operation_key(&game_uid),
        active: true,
    };
    let managed_path = {
        let store = state.store.lock().map_err(|_| "读取游戏记录失败")?;
        store
            .games
            .iter()
            .find(|game| game.game_uid == game_uid)
            .map(|game| PathBuf::from(&game.managed_path))
            .ok_or("游戏记录已不存在")?
    };
    let tracked_processes = state
        .learning_sessions
        .lock()
        .map_err(|_| "读取存档识别会话失败")?
        .get(&session_id)
        .map(|active| active.tracked_processes.clone())
        .ok_or("存档识别会话不存在")?;
    SaveLearningService::ensure_game_exited(&managed_path, Some(&tracked_processes))?;
    let active = state
        .learning_sessions
        .lock()
        .map_err(|_| "读取存档识别会话失败")?
        .get(&session_id)
        .cloned()
        .ok_or("存档识别会话不存在")?;
    let task_id = TaskService::create(
        &state,
        "analyze_saves",
        TaskCategory::Maintenance,
        Some(active.view.game_uid.clone()),
        "准备分析存档变化",
    )?;
    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    let worker_uid = active.view.game_uid.clone();
    let spawn = std::thread::Builder::new()
        .name("analyze-saves".into())
        .spawn(move || {
            let worker_state = app_handle.state::<AppState>();
            let _reservation = SaveProfileOperationGuard {
                state: &worker_state,
                key: worker_uid,
                active: true,
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                SaveLearningService::finish(
                    &active,
                    |progress, message| {
                        TaskService::update(
                            &app_handle.state(),
                            &task_id_for_thread,
                            TaskStatus::Running,
                            progress,
                            message,
                            None,
                        )
                    },
                    || TaskService::is_cancelled(&app_handle.state(), &task_id_for_thread),
                )
            }))
            .unwrap_or_else(|_| Err("存档分析异常，已结束本次识别，请重新开始".into()));
            match result {
                Ok(result) => {
                    let state: State<AppState> = app_handle.state();
                    if let Ok(mut sessions) = state.learning_sessions.lock() {
                        sessions.remove(&session_id);
                    }
                    state.remove_runtime(&active.view.game_uid);
                    cleanup_active_learning(&active);
                    TaskService::finish(
                        &state,
                        &task_id_for_thread,
                        TaskStatus::Success,
                        100,
                        format!("分析完成，发现 {} 个候选文件", result.changed_files.len()),
                        serde_json::to_value(result).ok(),
                        None,
                    );
                }
                Err(error) if error == "任务已取消" => {
                    let state: State<AppState> = app_handle.state();
                    if let Ok(mut sessions) = state.learning_sessions.lock() {
                        sessions.remove(&session_id);
                    }
                    state.remove_runtime(&active.view.game_uid);
                    cleanup_active_learning(&active);
                    TaskService::finish(
                        &app_handle.state(),
                        &task_id_for_thread,
                        TaskStatus::Cancelled,
                        100,
                        "已取消存档分析",
                        None,
                        None,
                    );
                }
                Err(error) => {
                    let state: State<AppState> = app_handle.state();
                    if let Ok(mut sessions) = state.learning_sessions.lock() {
                        sessions.remove(&session_id);
                    }
                    state.remove_runtime(&active.view.game_uid);
                    cleanup_active_learning(&active);
                    TaskService::finish(
                        &app_handle.state(),
                        &task_id_for_thread,
                        TaskStatus::Failed,
                        100,
                        "存档分析失败",
                        None,
                        Some(error),
                    );
                }
            }
        });
    if let Err(error) = spawn {
        TaskService::finish(
            &state,
            &task_id,
            TaskStatus::Failed,
            0,
            "启动分析线程失败",
            None,
            Some(error.to_string()),
        );
        return Err(format!("启动分析线程失败：{error}"));
    }
    reservation.active = false;
    Ok(task_id)
}

#[tauri::command]
pub fn cancel_save_learning(state: State<AppState>, session_id: String) -> Result<(), String> {
    let session_id = session_id.trim();
    let game_uid = state
        .learning_sessions
        .lock()
        .map_err(|_| "读取存档识别状态失败")?
        .get(session_id)
        .map(|active| active.view.game_uid.clone());
    let _reservation = if let Some(uid) = game_uid {
        state.claim_operation(&uid, "当前分析尚未结束，请取消分析任务后再操作")?;
        Some(SaveProfileOperationGuard {
            state: &state,
            key: uid,
            active: true,
        })
    } else {
        None
    };
    let active = state
        .learning_sessions
        .lock()
        .map_err(|_| "lock learning session state failed".to_string())?
        .remove(session_id);
    if let Some(active) = active {
        state.remove_runtime(&active.view.game_uid);
        cleanup_active_learning(&active);
        crate::logging::info(format!("已取消存档识别会话：session_id={session_id}"));
    } else {
        crate::logging::info(format!(
            "存档识别会话已不存在，按幂等取消处理：session_id={session_id}"
        ));
    }
    Ok(())
}

fn cleanup_active_learning(active: &ActiveLearningSession) {
    active
        .process_tracker_stop
        .store(true, std::sync::atomic::Ordering::Release);
    if let Some(capture) = active.etw_capture.as_ref() {
        if let Err(error) = stop_etw_capture(&capture.trace_name) {
            crate::logging::error(format!("取消存档识别时停止 ETW 失败：{error}"));
        }
        let _ = fs::remove_file(&capture.etl_path);
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DefaultSaveExclusions {
    pub exclude_patterns: Vec<String>,
    pub exclude_directories: Vec<String>,
}

#[tauri::command]
pub fn get_default_save_exclusions() -> DefaultSaveExclusions {
    DefaultSaveExclusions {
        exclude_patterns: crate::domain::DEFAULT_EXCLUDE_PATTERNS
            .iter()
            .map(|s| s.to_string())
            .collect(),
        exclude_directories: crate::domain::DEFAULT_EXCLUDE_DIRECTORIES
            .iter()
            .map(|s| s.to_string())
            .collect(),
    }
}

#[tauri::command]
pub async fn confirm_save_profile(
    app: AppHandle,
    game_uid: String,
    scopes: Vec<SaveScope>,
    confidence: u8,
    capture_mode: Option<String>,
) -> Result<SaveProfile, String> {
    super::run_blocking(move || {
        confirm_save_profile_blocking(app, game_uid, scopes, confidence, capture_mode)
    })
    .await
}

fn confirm_save_profile_blocking(
    app: AppHandle,
    game_uid: String,
    scopes: Vec<SaveScope>,
    confidence: u8,
    capture_mode: Option<String>,
) -> Result<SaveProfile, String> {
    let state = app.state::<AppState>();
    let game_uid = game_uid.trim().to_string();
    crate::logging::info(format!(
        "开始确认存档保护：game_uid={game_uid} scopes={} capture_mode={:?}",
        scopes.len(),
        capture_mode
    ));
    if scopes.is_empty() {
        return Err("至少需要一个存档保护范围".to_string());
    }
    state.claim_operation(&game_uid, "该游戏已有操作正在进行，请稍后确认存档保护")?;
    let _operation_guard = SaveProfileOperationGuard {
        state: &state,
        key: AppState::game_operation_key(&game_uid),
        active: true,
    };
    state.ensure_save_restore_idle(&game_uid)?;
    // 读 → 改 → 持久化 → 提交必须在**一次**加锁里完成。
    //
    // 旧写法先在一把锁里取 `game` 并检查 `PendingSetup`，释放锁后再在另一把锁里写入：
    // 两次加锁之间任何一次并发改动都能让生命周期判定失效，而且持久化还握着第二把锁。
    // 现在复核与提交同处一个闭包，判定和落盘之间不再留窗口。
    //
    // 代价是启动程序哈希与持久化发生在锁内（本机文件 IO，毫秒到几百毫秒量级），
    // 这正是项目里 `with_store_mut` 的既定形态。
    let profile = state.with_store_mut(|store| {
        let game =
            GameLibraryService::find(store, &game_uid).ok_or_else(|| "游戏不存在".to_string())?;
        if !matches!(game.lifecycle, GameLifecycle::PendingSetup) {
            return Err("该游戏已经完成设置".to_string());
        }
        SaveLearningService::ensure_game_exited(Path::new(&game.managed_path), None)?;
        for scope in &scopes {
            validate_scope_for_confirmation(scope)?;
        }
        crate::logging::info(format!(
            "存档保护范围校验完成：game_uid={game_uid} scopes={}",
            scopes.len()
        ));
        let executable_path = managed_executable_path(&game).map_err(|error| {
            crate::logging::error(format!(
                "确认存档保护失败：启动程序路径无效：game_uid={game_uid} error={error}"
            ));
            error
        })?;
        let executable_metadata = fs::metadata(&executable_path).map_err(|error| {
            let message = format!(
                "读取启动程序信息失败：{}：{error}",
                executable_path.display()
            );
            crate::logging::error(format!(
                "确认存档保护失败：game_uid={game_uid} error={message}"
            ));
            message
        })?;
        if !executable_metadata.is_file() {
            return Err(format!(
                "启动程序不是有效文件：{}",
                executable_path.display()
            ));
        }
        crate::logging::info(format!(
            "启动程序路径已确认：game_uid={game_uid} path={} size={}",
            executable_path.display(),
            executable_metadata.len()
        ));
        let executable_hash = sha256_file(&executable_path).map_err(|error| {
            crate::logging::error(format!(
                "确认存档保护失败：启动程序哈希失败：game_uid={game_uid} error={error}"
            ));
            error
        })?;
        crate::logging::info(format!("启动程序哈希完成：game_uid={game_uid}"));
        let profile = SaveProfile::new(
            game_uid.clone(),
            executable_hash,
            scopes,
            confidence.min(100),
            now_iso(),
        )
        .with_detection_evidence(crate::domain::detection_evidence_for(
            capture_mode.as_deref(),
        ));
        crate::logging::info(format!(
            "存档保护配置已创建：game_uid={game_uid} profile_id={}",
            profile.profile_id
        ));
        store.save_profiles.retain(|item| item.game_uid != game_uid);
        store.save_profiles.push(profile.clone());
        let target = store
            .games
            .iter_mut()
            .find(|item| item.game_uid == game_uid)
            .ok_or_else(|| "游戏登记已不存在".to_string())?;
        target.activate(profile.profile_id.clone());
        GameRepository::persist(&app, store).map_err(|error| {
            crate::logging::error(format!(
                "确认存档保护失败：持久化失败：game_uid={game_uid} error={error}"
            ));
            error
        })?;
        Ok(profile)
    })?;
    crate::logging::info(format!(
        "存档保护确认完成：game_uid={game_uid} profile_id={}",
        profile.profile_id
    ));
    Ok(profile)
}

struct SaveProfileOperationGuard<'a> {
    state: &'a AppState,
    key: String,
    active: bool,
}

impl Drop for SaveProfileOperationGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            if let Ok(mut operations) = self.state.save_operations.lock() {
                operations.remove(&self.key);
            }
        }
    }
}

#[tauri::command]
pub fn discard_pending_game(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
) -> Result<String, String> {
    crate::commands::game_commands::start_pending_game_removal(app, &state, game_uid)
}

fn validate_scope_for_confirmation(scope: &SaveScope) -> Result<(), String> {
    validate_scope(scope)?;
    let root = Path::new(&scope.root_path);
    for file in &scope.confirmed_files {
        if let Some(rule) = matching_exclusion(file, scope) {
            return Err(format!(
                "存档目录「{}」中的保护文件「{file}」命中排除规则「{rule}」。请移除该排除规则，或取消保护此文件。",
                scope.root_path
            ));
        }
        let metadata = std::fs::metadata(root.join(file)).map_err(|error| {
            format!("无法确认保护文件「{file}」（{}）：{error}", scope.root_path)
        })?;
        if !metadata.is_file() {
            return Err(format!("「{file}」不是文件，请从保护文件中移除它。"));
        }
        if let Some(limit) = scope.max_file_bytes.filter(|limit| metadata.len() > *limit) {
            return Err(format!(
                "保护文件「{file}」大小为 {} 字节，超过保护上限 {limit} 字节，不能纳入保护。",
                metadata.len()
            ));
        }
    }
    if scope.confirmed_files.is_empty() {
        if scope.unknown_file_policy == crate::domain::UnknownFilePolicy::Ignore {
            return Err(format!(
                "存档范围「{}」选择了仅保护已确认文件，请至少添加一个保护文件，或开启自动保护新存档。",
                scope.root_path
            ));
        }
        let admits_future_files = !scope.exclude_patterns.iter().any(|rule| {
            !rule.is_empty() && rule.bytes().all(|byte| byte == b'*')
        }) && scope.include_directories.iter().any(|directory| {
            matching_directory_exclusion(directory, scope).is_none() && root.join(directory).is_dir()
        });
        if !admits_future_files {
            return Err(format!(
                "存档范围「{}」没有可保护的文件或目录，请检查排除规则和保护目录。",
                scope.root_path
            ));
        }
    }
    Ok(())
}

fn validate_scope(scope: &SaveScope) -> Result<(), String> {
    if scope.root_path.trim().is_empty() || !Path::new(&scope.root_path).is_dir() {
        return Err(format!("存档目录不存在：{}", scope.root_path));
    }
    if scope.confirmed_files.is_empty() && scope.include_directories.is_empty() {
        return Err(format!("存档范围没有确认文件：{}", scope.root_path));
    }
    for value in scope
        .confirmed_files
        .iter()
        .chain(scope.include_directories.iter())
        .chain(scope.exclude_exact.iter())
        .chain(scope.exclude_directories.iter())
    {
        let path = Path::new(value);
        if path.is_absolute()
            || path
                .components()
                .any(|component| {
                    matches!(
                        component,
                        std::path::Component::Prefix(_)
                            | std::path::Component::RootDir
                            | std::path::Component::ParentDir
                    )
                })
        {
            return Err(format!("存档范围包含无效相对路径：{value}"));
        }
    }
    let root = Path::new(&scope.root_path);
    for confirmed in &scope.confirmed_files {
        let full = root.join(confirmed);
        if !full.exists() {
            crate::logging::warn(format!(
                "存档范围确认文件当前在磁盘上不存在（可能为临时文件或已重命名）：root={} file={}",
                scope.root_path, confirmed
            ));
        }
    }
    Ok(())
}

fn managed_executable_path(game: &crate::domain::Game) -> Result<PathBuf, String> {
    let root = Path::new(&game.managed_path);
    if !root.is_dir() {
        return Err(format!("受管游戏目录不存在：{}", root.display()));
    }
    let relative = Path::new(&game.launch.executable_relative_path);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err("启动程序相对路径无效".to_string());
    }
    Ok(root.join(relative))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|err| format!("读取启动程序失败：{err}"))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|err| format!("读取启动程序失败：{err}"))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

/// 只读推断一份存档范围初稿：不启动游戏、不采集 ETW。
///
/// 对应评审 A1 的「允许跳过学习」—— 给用户一条不必先跑完整识别就能进到确认界面的入口。
/// 返回结构与 `finish_save_learning` 一致（`eventCaptureMode = "preview"`），前端复用同一套
/// 审阅界面；证据等级一律是「待确认」，不会有任何范围被当成已确认。
#[tauri::command]
pub async fn preview_save_scopes(
    app: AppHandle,
    game_uid: String,
) -> Result<SaveLearningResult, String> {
    super::run_blocking(move || preview_save_scopes_blocking(app, game_uid)).await
}

fn preview_save_scopes_blocking(
    app: AppHandle,
    game_uid: String,
) -> Result<SaveLearningResult, String> {
    let state = app.state::<AppState>();
    let game_uid = game_uid.trim();
    let game = {
        let store = state
            .store
            .lock()
            .map_err(|_| "lock GameSaver store failed".to_string())?;
        GameLibraryService::find(&store, game_uid).ok_or_else(|| "游戏不存在".to_string())?
    };
    crate::logging::info(format!("开始只读推断存档初稿：game_uid={game_uid}"));
    let result = SaveLearningService::preview(&game)?;
    crate::logging::info(format!(
        "只读推断完成：game_uid={game_uid} 候选范围={} 置信度={}",
        result.scope_drafts.len(),
        result.confidence
    ));
    Ok(result)
}

#[tauri::command]
pub fn get_save_profile(
    state: State<AppState>,
    game_uid: String,
) -> Result<Option<SaveProfile>, String> {
    let game_uid = game_uid.trim();
    let store = state
        .store
        .lock()
        .map_err(|_| "lock GameSaver store failed".to_string())?;
    let game =
        GameLibraryService::find(&store, game_uid).ok_or_else(|| "游戏不存在".to_string())?;
    let profile = store
        .save_profiles
        .iter()
        .find(|p| {
            p.game_uid == game_uid
                && game.save_profile_id.as_deref() == Some(p.profile_id.as_str())
                && p.enabled
        })
        .cloned();
    Ok(profile)
}

#[tauri::command]
pub fn update_save_profile_keep_versions(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
    keep_versions: usize,
) -> Result<SaveProfile, String> {
    let game_uid = game_uid.trim().to_string();
    let keep_versions = keep_versions.clamp(1, 100);
    reserve_maintenance(&state, &game_uid)?;
    let result = (|| {
        let (updated_profile, pruned_versions) = {
            let mut store = state
                .store
                .lock()
                .map_err(|_| "lock GameSaver store failed".to_string())?;
            let game = GameLibraryService::find(&store, &game_uid)
                .ok_or_else(|| "游戏不存在".to_string())?;
            let mut candidate = store.clone();
            let profile = candidate
                .save_profiles
                .iter_mut()
                .find(|p| {
                    p.game_uid == game_uid
                        && game.save_profile_id.as_deref() == Some(p.profile_id.as_str())
                        && p.enabled
                })
                .ok_or_else(|| "未找到存档保护配置".to_string())?;
            profile.keep_versions = keep_versions;
            profile.updated_at = now_iso();
            let updated_profile = profile.clone();

            let pruned =
                SaveRepository::prune_game_save_versions(&mut candidate, &game_uid, keep_versions);

            GameRepository::persist(&app, &candidate)?;
            *store = candidate;
            (updated_profile, pruned)
        };
        // 版本清单已落盘，此时回收旧对象才安全（顺序不可颠倒，见
        // SaveRepository::prune_game_save_versions）；放在守卫块之外，顺带让 store 锁早于
        // GC 的全盘扫描释放。
        if let Some(versions) = pruned_versions.as_ref() {
            let _ = SaveRepository::collect_garbage(&app, versions);
        }
        Ok(updated_profile)
    })();
    release_maintenance(&state, &game_uid);
    result
}

#[tauri::command]
pub fn rebind_save_scope(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
    scope_index: usize,
    expected_root_path: String,
    directory: String,
) -> Result<SaveProfile, String> {
    let game_uid = game_uid.trim().to_string();
    let directory = strip_verbatim_prefix(
        &Path::new(&directory)
            .canonicalize()
            .map_err(|err| format!("无法解析新的存档目录：{err}"))?,
    );

    reserve_maintenance(&state, &game_uid)?;
    let result = (|| {
        let mut store = state
            .store
            .lock()
            .map_err(|_| "lock GameSaver store failed".to_string())?;
        let game =
            GameLibraryService::find(&store, &game_uid).ok_or_else(|| "游戏不存在".to_string())?;
        let mut candidate = store.clone();
        let profile = candidate
            .save_profiles
            .iter_mut()
            .find(|p| {
                p.game_uid == game_uid
                    && game.save_profile_id.as_deref() == Some(p.profile_id.as_str())
                    && p.enabled
            })
            .ok_or_else(|| "未找到存档保护配置".to_string())?;

        rebind_scope(profile, &game, scope_index, &expected_root_path, &directory)?;
        profile.updated_at = now_iso();
        let updated_profile = profile.clone();

        GameRepository::persist(&app, &candidate)?;
        *store = candidate;
        crate::logging::info(format!(
            "存档保护范围已更新：game_uid={game_uid} profile_id={}",
            updated_profile.profile_id
        ));
        Ok(updated_profile)
    })();
    release_maintenance(&state, &game_uid);
    result
}

fn rebind_scope(
    profile: &mut SaveProfile,
    game: &Game,
    scope_index: usize,
    expected_root_path: &str,
    directory: &Path,
) -> Result<(), String> {
    let scope = profile.scopes.get(scope_index).ok_or("存档范围已变化，请刷新后重试")?;
    if scope.root_path != expected_root_path {
        return Err("存档范围已变化，请刷新后重试".to_string());
    }
    let target_key = normalize_path(directory);
    if profile.scopes.iter().enumerate().any(|(index, candidate)| {
        index != scope_index && normalize_path(Path::new(&candidate.root_path)) == target_key
    }) || profile.root_bindings.iter().any(|binding| {
        binding.root_type == scope.root_type
            && normalize_path(Path::new(&binding.previous_path)) == target_key
            && normalize_path(Path::new(&binding.current_path)) != normalize_path(Path::new(&scope.root_path))
    }) {
        return Err("这个目录已经关联到另一个存档范围".to_string());
    }
    if scope.root_type == SaveRootType::ManagedGame {
        let managed = Path::new(&game.managed_path).canonicalize()
            .map_err(|err| format!("无法解析受管游戏目录：{err}"))?;
        if !directory.starts_with(strip_verbatim_prefix(&managed)) {
            return Err("游戏目录内的存档范围只能改到当前游戏目录内".to_string());
        }
    }
    let mut updated = scope.clone();
    updated.root_path = directory.to_string_lossy().into_owned();
    validate_scope(&updated)?;
    let previous_path = scope.root_path.clone();
    let previous_key = normalize_path(Path::new(&previous_path));
    if previous_key != target_key {
        for binding in &mut profile.root_bindings {
            if binding.root_type == scope.root_type
                && normalize_path(Path::new(&binding.current_path)) == previous_key
            {
                binding.current_path = updated.root_path.clone();
            }
        }
        profile.root_bindings.retain(|binding| {
            normalize_path(Path::new(&binding.previous_path)) != normalize_path(Path::new(&binding.current_path))
        });
        profile.root_bindings.push(SaveRootBinding {
            root_type: scope.root_type,
            previous_path,
            current_path: updated.root_path.clone(),
        });
    }
    profile.scopes[scope_index] = updated;
    Ok(())
}

#[tauri::command]
pub fn open_path_in_explorer(path: String) -> Result<(), String> {
    let clean_path = path.trim();
    if clean_path.is_empty() {
        return Err("路径不能为空".to_string());
    }
    let p = Path::new(clean_path);
    if !p.exists() {
        let mut parent = p.parent();
        while let Some(par) = parent {
            if par.exists() {
                return open_system_explorer(par);
            }
            parent = par.parent();
        }
        return Err(format!("路径不存在：{clean_path}"));
    }
    open_system_explorer(p)
}

fn open_system_explorer(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use std::process::Command;
        let mut command = Command::new("explorer");
        if path.is_file() {
            command.arg(format!("/select,{}", path.display()));
        } else {
            command.arg(path.as_os_str());
        }
        command
            .spawn()
            .map_err(|err| format!("打开文件资源管理器失败：{err}"))?;
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        let mut command = Command::new("open");
        if path.is_file() {
            command.args(["-R", &path.to_string_lossy()]);
        } else {
            command.arg(path.as_os_str());
        }
        command
            .spawn()
            .map_err(|err| format!("打开访达失败：{err}"))?;
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        use std::process::Command;
        let target = if path.is_file() {
            path.parent().unwrap_or(path)
        } else {
            path
        };
        Command::new("xdg-open")
            .arg(target.as_os_str())
            .spawn()
            .map_err(|err| format!("打开文件管理器失败：{err}"))?;
        Ok(())
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = path;
        Err("当前操作系统不支持打开文件管理器".to_string())
    }
}

fn now_iso() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use super::{sha256_file, validate_scope_for_confirmation, SaveScope};
    use std::fs;

    #[test]
    fn scope_confirmation_rejects_empty_explicit_protection_and_invalid_files() {
        let workspace = crate::test_support::TempWorkspace::new("save-effective-scope");
        let root = workspace.join("saves");
        fs::create_dir_all(root.join("directory")).unwrap();
        fs::write(root.join("slot.sav"), b"save").unwrap();
        let mut scope = SaveScope::new_manual(root.to_string_lossy().into_owned());
        scope.unknown_file_policy = crate::domain::UnknownFilePolicy::Ignore;
        assert!(validate_scope_for_confirmation(&scope).unwrap_err().contains("至少添加"));

        scope.confirmed_files.push("slot.sav".into());
        scope.max_file_bytes = Some(1);
        assert!(validate_scope_for_confirmation(&scope).unwrap_err().contains("超过保护上限"));
        scope.max_file_bytes = Some(4);
        validate_scope_for_confirmation(&scope).unwrap();

        scope.confirmed_files[0] = "directory".into();
        assert!(validate_scope_for_confirmation(&scope).unwrap_err().contains("不是文件"));
        scope.confirmed_files[0] = "missing.sav".into();
        assert!(validate_scope_for_confirmation(&scope).unwrap_err().contains("无法确认保护文件"));

        scope.confirmed_files.clear();
        scope.unknown_file_policy = crate::domain::UnknownFilePolicy::Protect;
        scope.root_path = root.join("directory").to_string_lossy().into_owned();
        validate_scope_for_confirmation(&scope).unwrap();
        scope.exclude_patterns.push("*".into());
        assert!(validate_scope_for_confirmation(&scope).is_err());
    }

    #[test]
    fn saving_a_scope_reports_a_confirmed_file_that_matches_an_exclusion() {
        let workspace = crate::test_support::TempWorkspace::new("save-conflicting-exclusion");
        let file = workspace.join("Slot.CSV");
        fs::write(&file, b"actual save").unwrap();
        let mut scope = SaveScope::new_manual(file.parent().unwrap().to_string_lossy().into_owned());
        scope.confirmed_files.push("Slot.CSV".into());
        let error = validate_scope_for_confirmation(&scope).unwrap_err();
        assert!(error.contains("Slot.CSV"));
        assert!(error.contains("*.csv"));

        scope.exclude_patterns.clear();
        scope.exclude_directories.clear();
        validate_scope_for_confirmation(&scope).unwrap();
        assert!(scope.exclude_patterns.is_empty());
        assert!(scope.exclude_directories.is_empty());
    }

    #[test]
    fn hashes_executable_without_using_large_stack_buffer() {
        // 一个只放这个测试文件的目录，随作用域自动清理（理由见 `crate::test_support::TempWorkspace`）。
        // 此前是 `remove_file(path).expect(...)` 收尾：`assert!` 一失败就执行不到，
        // 而且 `.expect()` 让「删不掉」也能把一次通过的测试判失败。
        let workspace = crate::test_support::TempWorkspace::new("hash");
        let path = workspace.join("sample.bin");
        fs::write(&path, b"abc").expect("write test file");
        assert_eq!(
            sha256_file(&path).expect("hash test file"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn rebind_changes_only_the_selected_root_and_keeps_historical_paths() {
        use crate::domain::{Game, SaveProfile, SaveRootType, SaveScope, UnknownFilePolicy};

        let workspace = crate::test_support::TempWorkspace::new("save-rebind");
        let first = workspace.join("moved");
        let second = workspace.join("moved-again");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let old = r"C:\Users\Old\AppData\LocalLow\Studio\Game";
        let mut original = SaveScope::new_manual(old.to_string());
        original.root_type = SaveRootType::LocalLow;
        original.confirmed_files = vec!["slot1.sav".to_string()];
        original.unknown_file_policy = UnknownFilePolicy::Ignore;
        let unrelated = SaveScope::new_manual(r"E:\Unavailable".to_string());
        let game = Game::new_pending("Example", r"D:\Games\Example", "game.exe");
        let mut profile = SaveProfile::new(
            game.game_uid.clone(), "hash".to_string(), vec![original, unrelated], 100, "0".to_string(),
        );

        super::rebind_scope(&mut profile, &game, 0, old, &first).unwrap();
        let first_path = profile.scopes[0].root_path.clone();
        super::rebind_scope(&mut profile, &game, 0, &first_path, &second).unwrap();
        assert_eq!(profile.scopes[0].root_type, SaveRootType::LocalLow);
        assert_eq!(profile.scopes[0].confirmed_files, ["slot1.sav"]);
        assert_eq!(profile.scopes[0].unknown_file_policy, UnknownFilePolicy::Ignore);
        assert_eq!(profile.scopes[1].root_path, r"E:\Unavailable");
        assert_eq!(profile.root_bindings.len(), 2);
        assert!(profile.root_bindings.iter().any(|binding| binding.previous_path == old));
        assert!(profile.root_bindings.iter().all(|binding| binding.current_path == profile.scopes[0].root_path));
        assert!(super::rebind_scope(&mut profile, &game, 0, old, &first).is_err());
    }

    #[test]
    fn save_profile_prune_logic_selects_oldest_to_remove() {
        use crate::domain::{AppStore, SaveVersion};
        use crate::repositories::SaveRepository;
        let mut store = AppStore {
            save_versions: (1..=6)
                .map(|i| SaveVersion {
                    version_id: format!("v{i}"),
                    game_uid: "game1".to_string(),
                    created_at: format!("170000000{i}"),
                    files: Vec::new(),
                    total_bytes: 100,
                })
                .collect(),
            ..AppStore::default()
        };

        // 走真实剪枝逻辑，而不是在测试里重写一遍排序：返回的存活集就是交给 GC 的依据。
        let alive = SaveRepository::prune_game_save_versions(&mut store, "game1", 5)
            .expect("超出保留数应发生剪枝");
        assert_eq!(alive.len(), 5);
        assert!(
            alive.iter().all(|version| version.version_id != "v1"),
            "最旧的 v1 应当被剪掉"
        );
        assert_eq!(store.save_versions.len(), 5);
    }

    #[test]
    fn open_path_in_explorer_validates_path() {
        assert!(super::open_path_in_explorer("".to_string()).is_err());
        assert!(super::open_path_in_explorer("   ".to_string()).is_err());
        assert!(
            super::open_path_in_explorer(r"Z:\non_existent_drive_12345\not_real".to_string())
                .is_err()
        );
    }
}
