use crate::{
    app_state::AppState,
    domain::{Game, GameLifecycle, GameRuntime, GameRuntimeStatus, SaveProfile, SaveVersion, TaskStatus},
    repositories::{GameRepository, SaveRepository},
    services::{GameLibraryService, TaskService},
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Manager};
use crate::services::process_service::TrackedProcessHandle;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchPrecheck {
    pub game_uid: String,
    pub can_launch: bool,
    pub executable_exists: bool,
    pub save_profile_ready: bool,
    pub valid_scope_count: usize,
    pub issues: Vec<String>,
}

pub struct LaunchService;

impl LaunchService {
    pub fn precheck(store: &crate::domain::AppStore, game_uid: &str) -> Result<LaunchPrecheck, String> {
        let game = GameLibraryService::find(store, game_uid).ok_or_else(|| "游戏不存在".to_string())?;
        let mut issues = Vec::new();
        if !matches!(game.lifecycle, GameLifecycle::Active) {
            issues.push("游戏尚未完成设置".to_string());
        }
        if !Path::new(&game.managed_path).is_dir() {
            issues.push("游戏本体目录不存在".to_string());
        }
        let executable = match managed_executable_path(&game) {
            Ok(path) => path,
            Err(error) => {
                issues.push(error);
                PathBuf::new()
            }
        };
        let executable_exists = executable.is_file();
        if !executable_exists {
            issues.push("启动程序不存在".to_string());
        }
        let profile = store.save_profiles.iter().find(|profile| {
            profile.game_uid == game.game_uid
                && game.save_profile_id.as_deref() == Some(profile.profile_id.as_str())
                && profile.enabled
        });
        let save_profile_ready = profile.is_some();
        let valid_scope_count = profile.map(GameLibraryService::valid_scope_count).unwrap_or_default();
        Ok(LaunchPrecheck {
            game_uid: game.game_uid,
            can_launch: issues.is_empty(),
            executable_exists,
            save_profile_ready,
            valid_scope_count,
            issues,
        })
    }

    pub fn launch(app: &AppHandle, state: &AppState, game_uid: String) -> Result<String, String> {
        let game_uid = game_uid.trim().to_string();
        let operation_lock = state.save_operations.lock().map_err(|_| "lock save operation state failed".to_string())?;
        if operation_lock.contains(&game_uid) {
            return Err("游戏正在进行存档版本操作".to_string());
        }
        if let Ok(sessions) = state.learning_sessions.lock() {
            if sessions.values().any(|s| s.view.game_uid == game_uid) {
                return Err("游戏正在进行存档识别学习，无法直接启动".to_string());
            }
        }
        {
            let mut running = state.running_games.lock().map_err(|_| "lock running game state failed".to_string())?;
            if running.contains_key(&game_uid) {
                return Err("游戏已经在运行".to_string());
            }
            running.insert(
                game_uid.clone(),
                GameRuntime {
                    game_uid: game_uid.clone(),
                    status: GameRuntimeStatus::Launching,
                    pid: None,
                    started_at: Some(now_iso()),
                    task_id: None,
                },
            );
        }
        drop(operation_lock);
        let loaded = (|| -> Result<(Game, Option<SaveProfile>, Option<SaveVersion>), String> {
            let store = state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())?;
            let check = Self::precheck(&store, &game_uid)?;
            if !check.can_launch {
                return Err(format!("无法启动游戏：{}", check.issues.join("、")));
            }
            let game = GameLibraryService::find(&store, &game_uid).ok_or_else(|| "游戏不存在".to_string())?;
            let profile = store
                .save_profiles
                .iter()
                .find(|profile| profile.game_uid == game_uid && game.save_profile_id.as_deref() == Some(profile.profile_id.as_str()) && profile.enabled)
                .cloned();
            let latest = game
                .latest_save_version_id
                .as_ref()
                .and_then(|id| store.save_versions.iter().find(|version| &version.version_id == id))
                .cloned();
            Ok((game, profile, latest))
        })();
        let (game, profile, latest) = match loaded {
            Ok(value) => value,
            Err(error) => {
                if let Ok(mut running) = state.running_games.lock() {
                    running.remove(&game_uid);
                }
                return Err(error);
            }
        };
        let task_id = match TaskService::create(state, "launch_game", Some(game_uid.clone()), "准备启动游戏") {
            Ok(task_id) => task_id,
            Err(error) => {
                if let Ok(mut running) = state.running_games.lock() {
                    running.remove(&game_uid);
                }
                return Err(error);
            }
        };
        if let Ok(mut running) = state.running_games.lock() {
            if let Some(runtime) = running.get_mut(&game_uid) {
                runtime.task_id = Some(task_id.clone());
            }
        }
        let app_handle = app.clone();
        let task_id_for_thread = task_id.clone();
        thread::spawn(move || {
            let result = run_game_session(&app_handle, &game, profile.as_ref(), latest.as_ref(), &task_id_for_thread);
            let state = app_handle.state::<AppState>();
            if let Ok(mut running) = state.running_games.lock() {
                running.remove(&game.game_uid);
            }
            match result {
                Ok(summary) => {
                    TaskService::finish(&state, &task_id_for_thread, TaskStatus::Success, 100, summary.0, Some(summary.1.clone()), None);
                    if summary.1.get("created").and_then(|v| v.as_bool()).unwrap_or(false) {
                        if let Some(version_id) = summary.1.get("versionId").and_then(|v| v.as_str()) {
                            let app_data_dir = app_handle.path().app_data_dir().ok();
                            let config = app_data_dir.as_deref().and_then(|dir| crate::repositories::BaiduConfigRepository::load(dir).ok().flatten());
                            if config.map(|c| c.auto_sync_save).unwrap_or(true) {
                                let _ = crate::commands::cloud_save_commands::start_upload_save_version_task(
                                    app_handle.clone(),
                                    app_handle.state::<AppState>(),
                                    game.game_uid.clone(),
                                    version_id.to_string(),
                                );
                            }
                        }
                    }
                }
                Err(error) => TaskService::finish(&state, &task_id_for_thread, TaskStatus::Failed, 100, "游戏会话结束，但存档版本提交失败", None, Some(error)),
            }
        });
        Ok(task_id)
    }
}

fn run_game_session(
    app: &AppHandle,
    game: &Game,
    profile: Option<&SaveProfile>,
    latest: Option<&SaveVersion>,
    task_id: &str,
) -> Result<(String, serde_json::Value), String> {
    let executable = managed_executable_path(game)?;
    let working_directory = game
        .launch
        .working_directory_relative_path
        .as_deref()
        .map(|relative| safe_join(Path::new(&game.managed_path), relative))
        .transpose()?
        .unwrap_or_else(|| executable.parent().unwrap_or(Path::new(&game.managed_path)).to_path_buf());
    let mut command = Command::new(&executable);
    command.args(&game.launch.arguments).current_dir(&working_directory);
    let mut child = command.spawn().map_err(|err| format!("启动游戏失败：{err}"))?;
    let pid = child.id();
    let state = app.state::<AppState>();
    if let Ok(mut running) = state.running_games.lock() {
        running.insert(
            game.game_uid.clone(),
            GameRuntime {
                game_uid: game.game_uid.clone(),
                status: GameRuntimeStatus::Running,
                pid: Some(pid),
                started_at: Some(now_iso()),
                task_id: Some(task_id.to_string()),
            },
        );
    }
    TaskService::update(&state, task_id, TaskStatus::Running, 10, if profile.is_some() { "游戏正在运行，退出后将提交存档版本" } else { "游戏正在运行，存档保护尚未设置" }, None);
    wait_for_game_session(&mut child, Path::new(&game.managed_path), &state, task_id)?;
    if let Ok(mut running) = state.running_games.lock() {
        if let Some(runtime) = running.get_mut(&game.game_uid) {
            runtime.status = GameRuntimeStatus::Saving;
        }
    }
    TaskService::update(&state, task_id, TaskStatus::Running, 70, if profile.is_some() { "游戏已退出，正在提交存档版本" } else { "游戏已退出，正在更新游戏状态" }, None);
    let version = profile
        .map(|profile| SaveRepository::commit(app, game, profile, latest, |progress, message| {
            TaskService::update(&state, task_id, TaskStatus::Running, 70 + progress / 3, message, None);
        }))
        .transpose()?
        .flatten();
    let pending_version = version.clone();
    let mut candidate = state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())?.clone();
    let now = now_iso();
    let version_summary = if let Some(version) = version {
        let version_id = version.version_id.clone();
        let file_count = version.files.len();
        candidate.save_versions.push(version);

        let keep_versions = profile.map(|p| p.keep_versions).unwrap_or(5);
        if keep_versions > 0 {
            let mut game_versions: Vec<_> = candidate
                .save_versions
                .iter()
                .filter(|v| v.game_uid == game.game_uid)
                .cloned()
                .collect();
            game_versions.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.version_id.cmp(&a.version_id)));
            if game_versions.len() > keep_versions {
                let to_remove: HashSet<String> = game_versions
                    .into_iter()
                    .skip(keep_versions)
                    .map(|v| v.version_id)
                    .collect();
                candidate.save_versions.retain(|v| !(v.game_uid == game.game_uid && to_remove.contains(&v.version_id)));
                let _ = SaveRepository::collect_garbage(app, &candidate.save_versions);
            }
        }

        let Some(game_record) = candidate.games.iter_mut().find(|item| item.game_uid == game.game_uid) else {
            if let Some(version) = pending_version.as_ref() { crate::repositories::release_pending_objects(version); }
            return Err("游戏记录不存在".to_string());
        };
        game_record.latest_save_version_id = Some(version_id.clone());
        game_record.last_played_at = Some(now);
        if let Err(error) = GameRepository::persist(app, &candidate) {
            if let Some(version) = pending_version.as_ref() { crate::repositories::release_pending_objects(version); }
            return Err(error);
        }
        serde_json::json!({ "created": true, "versionId": version_id, "fileCount": file_count })
    } else {
        if let Some(game_record) = candidate.games.iter_mut().find(|item| item.game_uid == game.game_uid) {
            game_record.last_played_at = Some(now);
        }
        GameRepository::persist(app, &candidate)?;
        serde_json::json!({ "created": false, "fileCount": 0 })
    };
    *state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())? = candidate;
    if let Some(version) = pending_version.as_ref() { crate::repositories::release_pending_objects(version); }
    let message = if version_summary.get("created").and_then(|value| value.as_bool()).unwrap_or(false) {
        "游戏已退出，存档版本已提交"
    } else {
        "游戏已退出，没有新的存档变化"
    };
    Ok((message.to_string(), version_summary))
}

fn wait_for_game_session(
    child: &mut Child,
    managed_path: &Path,
    state: &AppState,
    task_id: &str,
) -> Result<ExitStatus, String> {
    let root_pid = child.id();
    let mut tracked_pids = HashSet::from([root_pid]);
    let mut tracked_handles: HashMap<u32, TrackedProcessHandle> = HashMap::new();
    let mut root_status = None;
    let mut last_scan = Instant::now();

    let refresh_processes = |pids: &mut HashSet<u32>, handles: &mut HashMap<u32, TrackedProcessHandle>| {
        // 1. Expand process tree via Toolhelp snapshot (parent -> child)
        let _ = crate::services::learning::extend_tracked_process_tree(pids);

        // 2. Discover any process whose executable image is located within managed_path (handles UAC/detached launchers)
        let dir_pids = crate::services::process_service::find_processes_in_directory(managed_path);
        pids.extend(dir_pids);

        // 3. For any newly discovered PID (except root_pid and ignored crash handlers), open and hold handle
        for &pid in pids.iter() {
            if pid != root_pid && !handles.contains_key(&pid) {
                if let Some(handle) = TrackedProcessHandle::open(pid) {
                    if handle.is_alive() {
                        if let Some(image_path) = crate::services::process_service::get_process_image_path(pid) {
                            let file_name = image_path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or_default();
                            if crate::services::process_service::is_ignored_process_name(file_name) {
                                continue;
                            }
                        }
                        handles.insert(pid, handle);
                    }
                }
            }
        }
    };

    // Initial scan to catch immediate launcher sub-processes
    refresh_processes(&mut tracked_pids, &mut tracked_handles);

    loop {
        if TaskService::is_cancelled(state, task_id) {
            if let Some(status) = root_status {
                return Ok(status);
            }
            let _ = child.kill();
            let status = child
                .wait()
                .map_err(|err| format!("终止游戏进程失败：{err}"))?;
            return Ok(status);
        }

        // Check root process exit status
        if root_status.is_none() {
            root_status = child.try_wait().map_err(|err| format!("等待游戏退出失败：{err}"))?;
            if root_status.is_some() {
                // When root exits, perform immediate refresh to catch any final spawned process
                refresh_processes(&mut tracked_pids, &mut tracked_handles);
            }
        }

        // Periodic process tree and directory scan (every 1 second)
        if last_scan.elapsed() >= Duration::from_secs(1) {
            refresh_processes(&mut tracked_pids, &mut tracked_handles);
            last_scan = Instant::now();
        }

        // Prune terminated child processes!
        // Because TrackedProcessHandle keeps an open handle, Windows cannot recycle the PID.
        // Once is_alive() returns false, the handle is dropped (closing it), and pruned.
        tracked_handles.retain(|_, handle| handle.is_alive());

        // Exit evaluation:
        // When root process has exited and no non-ignored child/directory processes remain alive:
        if let Some(status) = root_status {
            if tracked_handles.is_empty() {
                return Ok(status);
            }
        }

        thread::sleep(Duration::from_millis(500));
    }
}

fn managed_executable_path(game: &Game) -> Result<PathBuf, String> {
    safe_join(Path::new(&game.managed_path), &game.launch.executable_relative_path)
}

fn safe_join(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let relative = Path::new(relative);
    if relative.is_absolute() || relative.components().any(|component| matches!(component, std::path::Component::ParentDir)) {
        return Err("启动路径包含无效的上级目录".to_string());
    }
    Ok(root.join(relative))
}

fn now_iso() -> String {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|value| value.as_secs().to_string()).unwrap_or_else(|_| "0".to_string())
}
