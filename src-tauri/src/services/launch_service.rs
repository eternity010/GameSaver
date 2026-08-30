use crate::{
    app_state::AppState,
    domain::{Game, GameLifecycle, GameRuntime, GameRuntimeStatus, SaveProfile, SaveVersion, TaskStatus},
    repositories::{GameRepository, SaveRepository},
    services::{GameLibraryService, TaskService},
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Manager};

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
                Ok(summary) => TaskService::finish(&state, &task_id_for_thread, TaskStatus::Success, 100, summary.0, Some(summary.1), None),
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
    wait_for_process_tree(&mut child)?;
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

fn wait_for_process_tree(child: &mut Child) -> Result<ExitStatus, String> {
    let root_pid = child.id();
    let mut tracked = HashSet::from([root_pid]);
    let mut root_status = None;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let _ = crate::services::learning::extend_tracked_process_tree(&mut tracked);
        if root_status.is_none() {
            root_status = child.try_wait().map_err(|err| format!("等待游戏退出失败：{err}"))?;
        }
        let descendants_alive = tracked.iter().any(|pid| *pid != root_pid && process_is_running(*pid));
        if let Some(status) = root_status {
            if !descendants_alive || Instant::now() >= deadline {
                return Ok(status);
            }
        }
        thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(target_os = "windows")]
fn process_is_running(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut exit_code = 0u32;
    let result = unsafe { GetExitCodeProcess(handle, &mut exit_code) } != 0;
    unsafe { CloseHandle(handle) };
    result && exit_code == 259
}

#[cfg(not(target_os = "windows"))]
fn process_is_running(_pid: u32) -> bool {
    false
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
