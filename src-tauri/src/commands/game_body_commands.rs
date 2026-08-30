use crate::{
    app_state::AppState,
    domain::{GameBodyVersion, GameLifecycle, SaveProfile, SaveVersion, TaskStatus},
    repositories::{BaiduConfigRepository, GameRepository, SaveRepository},
    services::{BodyPackageService, GameBodyUpdateService, GameLibraryService, TaskService},
};
use serde::Serialize;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager, State};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameBodyVersionView {
    #[serde(flatten)]
    pub version: GameBodyVersion,
    pub package_size: Option<u64>,
}

#[tauri::command]
pub fn list_game_body_versions(
    state: State<AppState>,
    game_uid: String,
) -> Result<Vec<GameBodyVersionView>, String> {
    let game_uid = game_uid.trim();
    let store = state
        .store
        .lock()
        .map_err(|_| "lock GameSaver store failed".to_string())?;
    if !store.games.iter().any(|game| game.game_uid == game_uid) {
        return Err("游戏不存在".to_string());
    }
    let mut versions = store
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
    versions.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    Ok(versions
        .into_iter()
        .map(|version| GameBodyVersionView {
            package_size: version
                .package_path
                .as_deref()
                .and_then(|path| std::fs::metadata(path).ok())
                .map(|metadata| metadata.len()),
            version,
        })
        .collect())
}

#[tauri::command]
pub fn update_game_body(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
    source_path: String,
) -> Result<String, String> {
    let game_uid = game_uid.trim().to_string();
    let source_path = PathBuf::from(source_path.trim());
    let (game, profile, latest) = load_update_context(&state, &game_uid)?;
    let games_root = state.games_root()?;
    if source_path.exists() && paths_overlap(&source_path, &games_root) {
        return Err("新版游戏目录不能位于 GameSaver 受管游戏库内".to_string());
    }
    let plan = GameBodyUpdateService::validate_source(&source_path, &game)?;
    reserve_update(&state, &game_uid)?;
    let task_id = match TaskService::create(
        &state,
        "update_game_body",
        Some(game_uid.clone()),
        "准备更新游戏本体",
    ) {
        Ok(task_id) => task_id,
        Err(error) => {
            release_update(&state, &game_uid);
            return Err(error);
        }
    };
    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        let result = update_game_body_task(
            &app_handle,
            &task_id_for_thread,
            &game,
            &profile,
            latest.as_ref(),
            plan,
        );
        release_update(&app_handle.state(), &game_uid);
        match result {
            Ok(summary) => {
                TaskService::finish(&app_handle.state(), &task_id_for_thread, TaskStatus::Success, 100, "游戏本体更新完成", Some(summary), None);
                if auto_upload_enabled(&app_handle) {
                    if let Err(error) = package_game_body(app_handle.clone(), app_handle.state(), game_uid.clone()) {
                        eprintln!("GameSaver 自动创建本体包失败：{error}");
                    }
                }
            }
            Err(error) if error == "任务已取消" => TaskService::finish(
                &app_handle.state(),
                &task_id_for_thread,
                TaskStatus::Cancelled,
                100,
                "已取消游戏本体更新",
                None,
                None,
            ),
            Err(error) => TaskService::finish(
                &app_handle.state(),
                &task_id_for_thread,
                TaskStatus::Failed,
                100,
                "游戏本体更新失败",
                None,
                Some(error),
            ),
        }
    });
    Ok(task_id)
}

#[tauri::command]
pub fn package_game_body(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
) -> Result<String, String> {
    let game_uid = game_uid.trim().to_string();
    if let Some(task_id) = active_body_package_task(&state, &game_uid) {
        return Ok(task_id);
    }
    let game = load_body_game(&state, &game_uid)?;
    let protected_paths = body_save_exclusion_paths(&state, &game)?;
    if let Err(error) = reserve_update(&state, &game_uid) {
        if let Some(task_id) = active_body_package_task(&state, &game_uid) {
            return Ok(task_id);
        }
        return Err(error);
    }
    let task_id = match TaskService::create(
        &state,
        "package_game_body",
        Some(game_uid.clone()),
        "准备创建游戏本体包",
    ) {
        Ok(task_id) => task_id,
        Err(error) => {
            release_update(&state, &game_uid);
            return Err(error);
        }
    };
    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        let result =
            package_game_body_task(&app_handle, &task_id_for_thread, &game, &protected_paths);
        release_update(&app_handle.state(), &game_uid);
        match result {
            Ok(summary) => {
                let version_id = summary.get("versionId").and_then(serde_json::Value::as_str).map(str::to_string);
                TaskService::finish(&app_handle.state(), &task_id_for_thread, TaskStatus::Success, 100, "游戏本体包创建完成", Some(summary), None);
                if let Some(version_id) = version_id.filter(|_| auto_upload_enabled(&app_handle)) {
                    if let Err(error) = crate::commands::baidu_commands::upload_game_body_package(app_handle.clone(), app_handle.state(), game_uid.clone(), version_id) {
                        eprintln!("GameSaver 自动上传本体包失败：{error}");
                    }
                }
            }
            Err(error) if error == "任务已取消" => TaskService::finish(
                &app_handle.state(),
                &task_id_for_thread,
                TaskStatus::Cancelled,
                100,
                "已取消游戏本体打包",
                None,
                None,
            ),
            Err(error) => TaskService::finish(
                &app_handle.state(),
                &task_id_for_thread,
                TaskStatus::Failed,
                100,
                "游戏本体包创建失败",
                None,
                Some(error),
            ),
        }
    });
    Ok(task_id)
}

#[tauri::command]
pub fn delete_game_body_package(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
    version_id: String,
) -> Result<String, String> {
    let game_uid = game_uid.trim().to_string();
    let version_id = version_id.trim().to_string();
    let (game, version, _, _) = load_body_package_context(&state, &game_uid, &version_id)?;
    let package_path =
        BodyPackageService::package_path(&body_package_cache_root(&app)?, &game_uid, &version_id);
    reserve_update(&state, &game_uid)?;
    let task_id = match TaskService::create(
        &state,
        "delete_game_body_package",
        Some(game_uid.clone()),
        "准备删除本地本体包",
    ) {
        Ok(task_id) => task_id,
        Err(error) => {
            release_update(&state, &game_uid);
            return Err(error);
        }
    };
    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        let result = delete_game_body_package_task(
            &app_handle,
            &task_id_for_thread,
            &game,
            &version,
            package_path,
        );
        release_update(&app_handle.state(), &game_uid);
        match result {
            Ok(summary) => TaskService::finish(
                &app_handle.state(),
                &task_id_for_thread,
                TaskStatus::Success,
                100,
                "本地本体包已删除",
                Some(summary),
                None,
            ),
            Err(error) => TaskService::finish(
                &app_handle.state(),
                &task_id_for_thread,
                TaskStatus::Failed,
                100,
                "删除本地本体包失败",
                None,
                Some(error),
            ),
        }
    });
    Ok(task_id)
}

#[tauri::command]
pub fn uninstall_game_body(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
) -> Result<String, String> {
    let game_uid = game_uid.trim().to_string();
    let game = load_game_for_uninstall(&state, &game_uid)?;
    let games_root = state.games_root()?;
    validate_managed_game_path(&games_root, &game)?;
    reserve_update(&state, &game_uid)?;
    let task_id = match TaskService::create(
        &state,
        "uninstall_game_body",
        Some(game_uid.clone()),
        "准备卸载游戏本体",
    ) {
        Ok(task_id) => task_id,
        Err(error) => {
            release_update(&state, &game_uid);
            return Err(error);
        }
    };
    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        let result = uninstall_game_body_task(&app_handle, &task_id_for_thread, &game, &games_root);
        release_update(&app_handle.state(), &game_uid);
        match result {
            Ok(summary) => TaskService::finish(
                &app_handle.state(),
                &task_id_for_thread,
                TaskStatus::Success,
                100,
                "游戏本体已卸载",
                Some(summary),
                None,
            ),
            Err(error) if error == "任务已取消" => TaskService::finish(
                &app_handle.state(),
                &task_id_for_thread,
                TaskStatus::Cancelled,
                100,
                "已取消游戏本体卸载",
                None,
                None,
            ),
            Err(error) => TaskService::finish(
                &app_handle.state(),
                &task_id_for_thread,
                TaskStatus::Failed,
                100,
                "游戏本体卸载失败",
                None,
                Some(error),
            ),
        }
    });
    Ok(task_id)
}

fn uninstall_game_body_task(
    app: &AppHandle,
    task_id: &str,
    game: &crate::domain::Game,
    games_root: &Path,
) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    let managed_path = Path::new(&game.managed_path);
    validate_managed_game_path(games_root, game)?;
    if !managed_path.exists() {
        mark_game_uninstalled(app, &state, &game.game_uid)?;
        return Ok(serde_json::json!({
            "gameUid": game.game_uid,
            "alreadyMissing": true,
        }));
    }
    if !managed_path.is_dir() {
        return Err("受管游戏路径不是文件夹，已停止卸载以避免误删文件".to_string());
    }

    TaskService::update(&state, task_id, TaskStatus::Running, 20, "正在移出受管游戏本体", None);
    let quarantine = games_root.join(format!(".{}.uninstalling-{}", game.game_uid, Uuid::new_v4().simple()));
    std::fs::rename(managed_path, &quarantine)
        .map_err(|error| format!("无法暂存游戏本体，可能仍有文件被占用：{error}"))?;

    if TaskService::is_cancelled(&state, task_id) {
        return match std::fs::rename(&quarantine, managed_path) {
            Ok(()) => Err("任务已取消".to_string()),
            Err(error) => Err(format!("任务已取消，但恢复游戏本体失败：{error}")),
        };
    }
    if let Err(error) = mark_game_uninstalled(app, &state, &game.game_uid) {
        let rollback = std::fs::rename(&quarantine, managed_path);
        return if let Err(rollback_error) = rollback {
            Err(format!("保存卸载状态失败：{error}；恢复游戏本体失败：{rollback_error}"))
        } else {
            Err(format!("保存卸载状态失败，已恢复游戏本体：{error}"))
        };
    }

    TaskService::update(&state, task_id, TaskStatus::Running, 80, "正在清理本体文件", None);
    let removed_bytes = directory_size(&quarantine).unwrap_or(0);
    if let Err(error) = std::fs::remove_dir_all(&quarantine) {
        crate::logging::error(format!("游戏本体已卸载，但清理暂存目录失败：{}：{error}", quarantine.display()));
        return Ok(serde_json::json!({
            "gameUid": game.game_uid,
            "alreadyMissing": false,
            "removedBytes": removed_bytes,
            "cleanupPending": true,
            "cleanupPath": quarantine,
        }));
    }
    Ok(serde_json::json!({
        "gameUid": game.game_uid,
        "alreadyMissing": false,
        "removedBytes": removed_bytes,
        "cleanupPending": false,
    }))
}

fn load_game_for_uninstall(state: &AppState, game_uid: &str) -> Result<crate::domain::Game, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "lock GameSaver store failed".to_string())?;
    GameLibraryService::find(&store, game_uid).ok_or_else(|| "游戏不存在".to_string())
}

fn validate_managed_game_path(games_root: &Path, game: &crate::domain::Game) -> Result<(), String> {
    let managed_path = Path::new(&game.managed_path);
    let expected = games_root.join(&game.game_uid);
    if !same_normalized_path(managed_path, &expected)
        || managed_path.file_name().and_then(|value| value.to_str()) != Some(game.game_uid.as_str())
    {
        return Err("受管游戏路径不在当前 GameSaver 游戏库中，已停止卸载".to_string());
    }
    Ok(())
}

fn same_normalized_path(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .eq_ignore_ascii_case(right.to_string_lossy().replace('/', "\\").trim_end_matches('\\'))
}

fn mark_game_uninstalled(app: &AppHandle, state: &AppState, game_uid: &str) -> Result<(), String> {
    let mut candidate = state
        .store
        .lock()
        .map_err(|_| "读取游戏记录失败".to_string())?
        .clone();
    let game = candidate
        .games
        .iter_mut()
        .find(|game| game.game_uid == game_uid)
        .ok_or_else(|| "游戏记录不存在".to_string())?;
    game.lifecycle = GameLifecycle::NeedsRepair;
    game.health = crate::domain::GameHealth::Broken;
    GameRepository::persist(app, &candidate).map_err(|error| format!("保存卸载状态失败：{error}"))?;
    *state
        .store
        .lock()
        .map_err(|_| "更新游戏记录失败".to_string())? = candidate;
    Ok(())
}

fn directory_size(root: &Path) -> Result<u64, String> {
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| format!("读取待删除本体失败：{error}"))?;
        if entry.file_type().is_file() {
            total = total.saturating_add(
                entry
                    .metadata()
                    .map_err(|error| format!("读取待删除本体大小失败：{error}"))?
                    .len(),
            );
        }
    }
    Ok(total)
}

fn delete_game_body_package_task(
    app: &AppHandle,
    task_id: &str,
    game: &crate::domain::Game,
    version: &crate::domain::GameBodyVersion,
    package_path: PathBuf,
) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        20,
        "正在更新本体版本记录",
        None,
    );
    let mut candidate = state
        .store
        .lock()
        .map_err(|_| "读取游戏本体包记录失败".to_string())?
        .clone();
    let Some(record) = candidate.body_versions.iter_mut().find(|item| {
        item.game_uid == game.game_uid
            && item.version_id == version.version_id
            && item.package_path.is_some()
    }) else {
        return Err("游戏本体包版本不存在".to_string());
    };
    let keeps_archive = !record.archive_path.trim().is_empty();
    if keeps_archive {
        record.package_path = None;
        record.sha256 = None;
        record.excluded_items.clear();
        record.upload_status = None;
    } else {
        candidate
            .body_versions
            .retain(|item| item.version_id != version.version_id);
    }
    GameRepository::persist(app, &candidate)
        .map_err(|error| format!("保存本体版本记录失败：{error}"))?;
    *state
        .store
        .lock()
        .map_err(|_| "更新游戏本体包记录失败".to_string())? = candidate;
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        70,
        "正在清理本地 ZIP 缓存",
        None,
    );
    if let Err(error) = std::fs::remove_file(&package_path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            return Err(format!("本体版本记录已更新，但清理 ZIP 缓存失败：{error}"));
        }
    }
    Ok(serde_json::json!({ "versionId": version.version_id, "keepsArchive": keeps_archive }))
}

fn package_game_body_task(
    app: &AppHandle,
    task_id: &str,
    game: &crate::domain::Game,
    protected_paths: &[String],
) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    let version_id = Uuid::new_v4().to_string();
    let cache_root = body_package_cache_root(app)?;
    let result = BodyPackageService::create_package_with_exclusions(
        Path::new(&game.managed_path),
        &cache_root,
        &game.game_uid,
        &version_id,
        &game.launch.executable_relative_path,
        protected_paths,
        |progress, message| {
            TaskService::update(
                &state,
                task_id,
                TaskStatus::Running,
                progress,
                message,
                None,
            )
        },
        || TaskService::is_cancelled(&state, task_id),
    )?;
    let body_version = crate::domain::GameBodyVersion {
        version_id: version_id.clone(),
        game_uid: game.game_uid.clone(),
        created_at: now_iso(),
        archive_path: String::new(),
        file_count: result.manifest.file_count,
        total_bytes: result.manifest.total_bytes,
        package_path: Some(result.package_path.to_string_lossy().to_string()),
        sha256: Some(result.sha256.clone()),
        excluded_items: result.manifest.excluded_items.clone(),
        upload_status: Some("local_only".to_string()),
        remote_path: None,
        remote_fs_id: None,
        remote_size: None,
    };
    let mut candidate = state
        .store
        .lock()
        .map_err(|_| "读取游戏本体包记录失败".to_string())?
        .clone();
    candidate.body_versions.push(body_version);
    if let Err(error) = GameRepository::persist(app, &candidate) {
        let _ = std::fs::remove_file(&result.package_path);
        return Err(format!("保存游戏本体包记录失败：{error}"));
    }
    *state
        .store
        .lock()
        .map_err(|_| "更新游戏本体包记录失败".to_string())? = candidate;
    Ok(serde_json::json!({
        "versionId": version_id,
        "packagePath": result.package_path,
        "sha256": result.sha256,
        "fileCount": result.manifest.file_count,
        "totalBytes": result.manifest.total_bytes,
        "excludedItems": result.manifest.excluded_items,
    }))
}

fn load_body_game(state: &AppState, game_uid: &str) -> Result<crate::domain::Game, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "lock GameSaver store failed".to_string())?;
    let game =
        GameLibraryService::find(&store, game_uid).ok_or_else(|| "游戏不存在".to_string())?;
    if !matches!(game.lifecycle, GameLifecycle::Active) {
        return Err("游戏尚未完成设置".to_string());
    }
    Ok(game)
}

fn load_body_package_context(
    state: &AppState,
    game_uid: &str,
    version_id: &str,
) -> Result<
    (
        crate::domain::Game,
        crate::domain::GameBodyVersion,
        Option<SaveProfile>,
        Option<SaveVersion>,
    ),
    String,
> {
    let store = state
        .store
        .lock()
        .map_err(|_| "lock GameSaver store failed".to_string())?;
    let game =
        GameLibraryService::find(&store, game_uid).ok_or_else(|| "游戏不存在".to_string())?;
    if !matches!(game.lifecycle, GameLifecycle::Active) {
        return Err("游戏尚未完成设置".to_string());
    }
    let version = store
        .body_versions
        .iter()
        .find(|item| {
            item.game_uid == game_uid
                && item.version_id == version_id
                && item.package_path.is_some()
        })
        .cloned()
        .ok_or_else(|| "游戏本体包版本不存在".to_string())?;
    let profile = game
        .save_profile_id
        .as_ref()
        .and_then(|profile_id| {
            store.save_profiles.iter().find(|item| {
                item.profile_id == *profile_id && item.game_uid == game_uid && item.enabled
            })
        })
        .cloned();
    let latest = game
        .latest_save_version_id
        .as_ref()
        .and_then(|save_id| {
            store
                .save_versions
                .iter()
                .find(|item| item.version_id == *save_id && item.game_uid == game_uid)
        })
        .cloned();
    Ok((game, version, profile, latest))
}

fn body_save_exclusion_paths(
    state: &AppState,
    game: &crate::domain::Game,
) -> Result<Vec<String>, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "读取存档保护范围失败".to_string())?;
    let Some(profile_id) = game.save_profile_id.as_deref() else {
        return Ok(Vec::new());
    };
    let Some(profile) = store.save_profiles.iter().find(|item| {
        item.profile_id == profile_id && item.game_uid == game.game_uid && item.enabled
    }) else {
        return Ok(Vec::new());
    };
    let managed_root = Path::new(&game.managed_path)
        .canonicalize()
        .map_err(|err| format!("解析受管游戏目录失败：{err}"))?;
    let mut paths = Vec::new();
    for scope in &profile.scopes {
        if !matches!(scope.root_type, crate::domain::SaveRootType::ManagedGame) {
            continue;
        }
        let Ok(scope_root) = Path::new(&scope.root_path).canonicalize() else {
            continue;
        };
        let Ok(base) = scope_root.strip_prefix(&managed_root) else {
            continue;
        };
        for relative in &scope.confirmed_files {
            paths.push(join_body_relative(base, relative)?);
        }
        for relative in &scope.include_directories {
            let path = join_body_relative(base, relative)?;
            if path == "." {
                return Err(
                    "存档保护范围覆盖整个游戏本体，请先缩小存档范围再创建本体包".to_string()
                );
            }
            paths.push(path);
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn join_body_relative(base: &Path, relative: &str) -> Result<String, String> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(format!("存档保护范围包含无效路径：{relative}"));
    }
    let combined = base.join(relative_path);
    let mut parts = Vec::new();
    for component in combined.components() {
        match component {
            std::path::Component::Normal(value) => parts.push(value.to_string_lossy().to_string()),
            std::path::Component::CurDir => {}
            _ => return Err(format!("存档保护范围包含无效路径：{relative}")),
        }
    }
    if parts.is_empty() {
        Ok(".".to_string())
    } else {
        Ok(parts.join("/"))
    }
}

fn body_package_cache_root(app: &AppHandle) -> Result<PathBuf, String> {
    app.state::<AppState>().body_packages_root()
}

fn auto_upload_enabled(app: &AppHandle) -> bool {
    let Ok(app_data_dir) = app.path().app_data_dir() else {
        return false;
    };
    BaiduConfigRepository::load(&app_data_dir)
        .ok()
        .flatten()
        .is_some_and(|config| config.auto_upload_body)
}

fn update_game_body_task(
    app: &AppHandle,
    task_id: &str,
    game: &crate::domain::Game,
    profile: &SaveProfile,
    latest: Option<&SaveVersion>,
    plan: crate::services::game_body_update_service::UpdatePlan,
) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    let games_root = Path::new(&game.managed_path)
        .parent()
        .ok_or_else(|| "解析游戏库目录失败".to_string())?
        .to_path_buf();
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
    let protected = match SaveRepository::commit(app, game, profile, latest, |progress, message| {
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
                if Path::new(&game.managed_path).is_dir() {
                    let _ = GameBodyUpdateService::clear_journal(&journal_path);
                }
                if let Some(version) = pending_protected.as_ref() {
                    crate::repositories::release_pending_objects(version);
                }
                let _ = std::fs::remove_dir_all(&staging);
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
    let mut candidate = match state.store.lock() {
        Ok(store) => store.clone(),
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
    if let Some(protected) = protected {
        candidate.save_versions.push(protected);
    }
    candidate.body_versions.push(body_version);
    let Some(game_record) = candidate
        .games
        .iter_mut()
        .find(|item| item.game_uid == game.game_uid)
    else {
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
    if let Err(error) = GameRepository::persist(app, &candidate) {
        let save_rollback = receipt.map(SaveRepository::rollback_restore);
        let body_rollback = GameBodyUpdateService::rollback(Path::new(&game.managed_path), &swap);
        if let Some(version) = pending_protected.as_ref() {
            crate::repositories::release_pending_objects(version);
        }
        if body_rollback.is_ok() {
            let _ = GameBodyUpdateService::clear_journal(&journal_path);
        }
        let mut errors = vec![format!("保存本体更新记录失败：{error}")];
        if let Some(Err(save_error)) = save_rollback {
            errors.push(format!("存档回滚失败：{save_error}"));
        }
        if let Err(body_error) = body_rollback {
            errors.push(format!("游戏本体回滚失败：{body_error}"));
        }
        return Err(errors.join("；"));
    }
    if let Some(receipt) = receipt {
        SaveRepository::finalize_restore(receipt);
    }
    *state
        .store
        .lock()
        .map_err(|_| "lock GameSaver store failed".to_string())? = candidate;
    if let Some(version) = pending_protected.as_ref() {
        crate::repositories::release_pending_objects(version);
    }
    let mut cleaned = state
        .store
        .lock()
        .map_err(|_| "读取游戏更新清理记录失败".to_string())?
        .clone();
    let archive_cleanup = GameBodyUpdateService::cleanup_archived_body_versions(
        &games_root,
        &mut cleaned.body_versions,
    );
    let old_body_cleanup_pending = archive_cleanup.is_err();
    if archive_cleanup.is_ok() {
        if let Err(error) = GameRepository::persist(app, &cleaned) {
            crate::logging::error(format!("旧游戏本体已清理，但更新记录清理失败：{error}"));
        } else {
            *state
                .store
                .lock()
                .map_err(|_| "更新游戏清理记录失败".to_string())? = cleaned;
        }
        let _ = GameBodyUpdateService::clear_journal(&journal_path);
    } else if let Err(error) = archive_cleanup {
        crate::logging::error(error);
    }
    Ok(
        serde_json::json!({ "fileCount": plan.file_count, "totalBytes": plan.total_bytes, "oldBodyCleanupPending": old_body_cleanup_pending }),
    )
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
    if body_rollback.is_ok() {
        let _ = GameBodyUpdateService::clear_journal(journal_path);
    }
    let mut errors = vec![error];
    if let Some(Err(save_error)) = save_rollback {
        errors.push(format!("存档回滚失败：{save_error}"));
    }
    if let Err(body_error) = body_rollback {
        errors.push(format!("游戏本体回滚失败：{body_error}"));
    }
    Err(errors.join("；"))
}

fn load_update_context(
    state: &AppState,
    game_uid: &str,
) -> Result<(crate::domain::Game, SaveProfile, Option<SaveVersion>), String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "lock GameSaver store failed".to_string())?;
    let game =
        GameLibraryService::find(&store, game_uid).ok_or_else(|| "游戏不存在".to_string())?;
    if !matches!(game.lifecycle, GameLifecycle::Active) {
        return Err("游戏尚未完成设置".to_string());
    }
    let profile = store
        .save_profiles
        .iter()
        .find(|profile| {
            game.save_profile_id.as_deref() == Some(profile.profile_id.as_str())
                && profile.game_uid == game_uid
                && profile.enabled
        })
        .cloned()
        .ok_or_else(|| "存档保护配置不存在".to_string())?;
    let latest = game
        .latest_save_version_id
        .as_ref()
        .and_then(|id| {
            store
                .save_versions
                .iter()
                .find(|version| &version.version_id == id)
        })
        .cloned();
    Ok((game, profile, latest))
}

fn reserve_update(state: &AppState, game_uid: &str) -> Result<(), String> {
    let mut operations = state
        .save_operations
        .lock()
        .map_err(|_| "lock save operation state failed".to_string())?;
    if operations.contains(game_uid) {
        return Err("该游戏已有本体或存档操作正在进行".to_string());
    }
    if state
        .running_games
        .lock()
        .map_err(|_| "lock running game state failed".to_string())?
        .contains_key(game_uid)
    {
        return Err("游戏运行中，暂时不能更新游戏本体".to_string());
    }
    operations.insert(game_uid.to_string());
    Ok(())
}

fn active_body_package_task(state: &AppState, game_uid: &str) -> Option<String> {
    state.tasks.lock().ok()?.values().find_map(|task| {
        if task.game_uid.as_deref() == Some(game_uid)
            && task.task_type == "package_game_body"
            && matches!(task.status, TaskStatus::Pending | TaskStatus::Running)
        {
            Some(task.task_id.clone())
        } else {
            None
        }
    })
}

fn release_update(state: &AppState, game_uid: &str) {
    if let Ok(mut operations) = state.save_operations.lock() {
        operations.remove(game_uid);
    }
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    let left = left
        .to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase();
    let right = right
        .to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase();
    left == right || left.starts_with(&(right.clone() + "\\")) || right.starts_with(&(left + "\\"))
}

fn now_iso() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}
