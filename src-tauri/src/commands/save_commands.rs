use crate::{
    app_state::AppState,
    domain::{GameLifecycle, SaveProfile, SaveScope, TaskStatus},
    repositories::GameRepository,
    services::{learning::stop_etw_capture, GameLibraryService, SaveLearningService, TaskService},
};
use sha2::{Digest, Sha256};
use std::{fs, io::Read, path::{Path, PathBuf}};
use tauri::{AppHandle, Manager, State};

#[tauri::command]
pub fn start_save_learning_task(app: AppHandle, state: State<AppState>, game_uid: String) -> Result<String, String> {
    let game_uid = game_uid.trim().to_string();
    let game = {
        let store = state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())?;
        let game = GameLibraryService::find(&store, &game_uid).ok_or_else(|| "游戏不存在".to_string())?;
        if !matches!(game.lifecycle, GameLifecycle::PendingSetup) {
            return Err("只有等待设置的游戏可以开始存档识别".to_string());
        }
        game
    };
    let task_id = TaskService::create(&state, "learn_saves", Some(game_uid), "准备识别存档范围")?;
    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        let result = SaveLearningService::start(
            &app_handle,
            &game,
            |progress, message| TaskService::update(&app_handle.state(), &task_id_for_thread, TaskStatus::Running, progress, message, None),
            || TaskService::is_cancelled(&app_handle.state(), &task_id_for_thread),
        );
        match result {
            Ok(active) => {
                let view = active.view.clone();
                let state: State<AppState> = app_handle.state();
                if let Ok(mut sessions) = state.learning_sessions.lock() {
                    sessions.insert(view.session_id.clone(), active);
                }
                TaskService::finish(&state, &task_id_for_thread, TaskStatus::Success, 100, "游戏已启动，请完成一次保存后回来分析", serde_json::to_value(view).ok(), None);
            }
            Err(error) if error == "任务已取消" => TaskService::finish(&app_handle.state(), &task_id_for_thread, TaskStatus::Cancelled, 100, "已取消存档识别", None, None),
            Err(error) => TaskService::finish(&app_handle.state(), &task_id_for_thread, TaskStatus::Failed, 100, "启动存档识别失败", None, Some(error)),
        }
    });
    Ok(task_id)
}

#[tauri::command]
pub fn start_finish_save_learning_task(app: AppHandle, state: State<AppState>, session_id: String) -> Result<String, String> {
    let session_id = session_id.trim().to_string();
    let active = state.learning_sessions.lock().map_err(|_| "lock learning session state failed".to_string())?.get(&session_id).cloned().ok_or_else(|| "存档识别会话不存在".to_string())?;
    let task_id = TaskService::create(&state, "analyze_saves", Some(active.view.game_uid.clone()), "准备分析存档变化")?;
    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        let result = SaveLearningService::finish(
            &active,
            |progress, message| TaskService::update(&app_handle.state(), &task_id_for_thread, TaskStatus::Running, progress, message, None),
            || TaskService::is_cancelled(&app_handle.state(), &task_id_for_thread),
        );
        match result {
            Ok(result) => {
                let state: State<AppState> = app_handle.state();
                if let Ok(mut sessions) = state.learning_sessions.lock() { sessions.remove(&session_id); }
                TaskService::finish(&state, &task_id_for_thread, TaskStatus::Success, 100, format!("分析完成，发现 {} 个候选文件", result.changed_files.len()), serde_json::to_value(result).ok(), None);
            }
            Err(error) if error == "任务已取消" => TaskService::finish(&app_handle.state(), &task_id_for_thread, TaskStatus::Cancelled, 100, "已取消存档分析", None, None),
            Err(error) => TaskService::finish(&app_handle.state(), &task_id_for_thread, TaskStatus::Failed, 100, "存档分析失败", None, Some(error)),
        }
    });
    Ok(task_id)
}

#[tauri::command]
pub fn cancel_save_learning(state: State<AppState>, session_id: String) -> Result<(), String> {
    let session_id = session_id.trim();
    let active = state.learning_sessions.lock().map_err(|_| "lock learning session state failed".to_string())?.remove(session_id);
    if let Some(active) = active {
        active.process_tracker_stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(capture) = active.etw_capture.as_ref() {
            let _ = stop_etw_capture(&capture.trace_name);
        }
    }
    Ok(())
}

#[tauri::command]
pub fn confirm_save_profile(app: AppHandle, state: State<AppState>, game_uid: String, scopes: Vec<SaveScope>, confidence: u8) -> Result<SaveProfile, String> {
    let game_uid = game_uid.trim().to_string();
    crate::logging::info(format!("开始确认存档保护：game_uid={game_uid} scopes={}", scopes.len()));
    if scopes.is_empty() { return Err("至少需要一个存档保护范围".to_string()); }
    let operation_reserved = {
        let mut operations = state.save_operations.lock().map_err(|_| "锁定存档确认状态失败".to_string())?;
        if !operations.insert(format!("confirm:{game_uid}")) {
            return Err("该游戏正在确认存档保护，请勿重复点击".to_string());
        }
        true
    };
    let _operation_guard = SaveProfileOperationGuard { state: &state, key: format!("confirm:{game_uid}"), active: operation_reserved };
    let game = {
        let store = state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())?;
        let game = GameLibraryService::find(&store, &game_uid).ok_or_else(|| "游戏不存在".to_string())?;
        if !matches!(game.lifecycle, GameLifecycle::PendingSetup) { return Err("该游戏已经完成设置".to_string()); }
        game
    };
    for scope in &scopes { validate_scope(scope)?; }
    crate::logging::info(format!("存档保护范围校验完成：game_uid={game_uid} scopes={}", scopes.len()));
    let executable_path = managed_executable_path(&game).map_err(|error| {
        crate::logging::error(format!("确认存档保护失败：启动程序路径无效：game_uid={game_uid} error={error}"));
        error
    })?;
    let executable_metadata = fs::metadata(&executable_path).map_err(|error| {
        let message = format!("读取启动程序信息失败：{}：{error}", executable_path.display());
        crate::logging::error(format!("确认存档保护失败：game_uid={game_uid} error={message}"));
        message
    })?;
    if !executable_metadata.is_file() {
        return Err(format!("启动程序不是有效文件：{}", executable_path.display()));
    }
    crate::logging::info(format!("启动程序路径已确认：game_uid={game_uid} path={} size={}", executable_path.display(), executable_metadata.len()));
    let executable_hash = sha256_file(&executable_path).map_err(|error| {
        crate::logging::error(format!("确认存档保护失败：启动程序哈希失败：game_uid={game_uid} error={error}"));
        error
    })?;
    crate::logging::info(format!("启动程序哈希完成：game_uid={game_uid}"));
    let profile = SaveProfile::new(game_uid.clone(), executable_hash, scopes, confidence.min(100), now_iso());
    crate::logging::info(format!("存档保护配置已创建：game_uid={game_uid} profile_id={}", profile.profile_id));
    let mut store = state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())?;
    crate::logging::info(format!("开始准备存档保护持久化：game_uid={game_uid}"));
    let mut candidate = store.clone();
    candidate.save_profiles.retain(|item| item.game_uid != game_uid);
    candidate.save_profiles.push(profile.clone());
    let target = candidate.games.iter_mut().find(|item| item.game_uid == game_uid).ok_or_else(|| "游戏登记已不存在".to_string())?;
    target.activate(profile.profile_id.clone());
    GameRepository::persist(&app, &candidate).map_err(|error| {
        crate::logging::error(format!("确认存档保护失败：持久化失败：game_uid={game_uid} error={error}"));
        error
    })?;
    crate::logging::info(format!("存档保护持久化完成：game_uid={game_uid}"));
    *store = candidate;
    crate::logging::info(format!("存档保护确认完成：game_uid={game_uid} profile_id={}", profile.profile_id));
    Ok(profile)
}

struct SaveProfileOperationGuard<'a> {
    state: &'a State<'a, AppState>,
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
pub fn discard_pending_game(app: AppHandle, state: State<AppState>, game_uid: String) -> Result<(), String> {
    let game_uid = game_uid.trim().to_string();
    let game = {
        let store = state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())?;
        let game = GameLibraryService::find(&store, &game_uid).ok_or_else(|| "游戏不存在".to_string())?;
        if !matches!(game.lifecycle, GameLifecycle::PendingSetup) { return Err("只能放弃等待设置的游戏".to_string()); }
        game
    };
    let managed_path = PathBuf::from(&game.managed_path);
    let quarantine = managed_path.with_file_name(format!(".{}.discarding", game_uid));
    if managed_path.exists() { fs::rename(&managed_path, &quarantine).map_err(|err| format!("准备清理游戏本体失败：{err}"))?; }
    let result = (|| -> Result<(), String> {
        let mut store = state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())?;
        let mut candidate = store.clone();
        candidate.games.retain(|item| item.game_uid != game_uid);
        candidate.save_profiles.retain(|item| item.game_uid != game_uid);
        GameRepository::persist(&app, &candidate)?;
        *store = candidate;
        Ok(())
    })();
    if result.is_err() && quarantine.exists() { let _ = fs::rename(&quarantine, &managed_path); }
    if result.is_ok() { let _ = fs::remove_dir_all(quarantine); }
    result
}

fn validate_scope(scope: &SaveScope) -> Result<(), String> {
    if scope.root_path.trim().is_empty() || !Path::new(&scope.root_path).is_dir() { return Err(format!("存档目录不存在：{}", scope.root_path)); }
    if scope.confirmed_files.is_empty() && scope.include_directories.is_empty() { return Err(format!("存档范围没有确认文件：{}", scope.root_path)); }
    for value in scope.confirmed_files.iter().chain(scope.include_directories.iter()).chain(scope.exclude_exact.iter()).chain(scope.exclude_directories.iter()) {
        let path = Path::new(value);
        if path.is_absolute() || path.components().any(|component| matches!(component, std::path::Component::ParentDir)) { return Err(format!("存档范围包含无效相对路径：{value}")); }
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
        || relative.components().any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err("启动程序相对路径无效".to_string());
    }
    Ok(root.join(relative))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|err| format!("读取启动程序失败：{err}"))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop { let read = file.read(&mut buffer).map_err(|err| format!("读取启动程序失败：{err}"))?; if read == 0 { break; } digest.update(&buffer[..read]); }
    Ok(hex::encode(digest.finalize()))
}

fn now_iso() -> String {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|value| value.as_secs().to_string()).unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use super::sha256_file;
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn hashes_executable_without_using_large_stack_buffer() {
        let path = std::env::temp_dir().join(format!("gamesaver-hash-{}.bin", Uuid::new_v4()));
        fs::write(&path, b"abc").expect("write test file");
        assert_eq!(sha256_file(&path).expect("hash test file"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        fs::remove_file(path).expect("remove test file");
    }
}
