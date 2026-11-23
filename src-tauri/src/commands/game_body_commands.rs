use crate::{
    app_state::AppState,
    domain::{
        compare_created_at, is_safe_path_segment, path_utils::normalize_path, AppStore,
        GameBodyVersion, GameLifecycle, SaveProfile, SaveVersion, TaskCategory, TaskStatus,
    },
    repositories::{BaiduConfigRepository, GameRepository},
    services::{
        game_removal_service::validate_target, BodyPackageService, GameBodyUpdateService,
        GameLibraryService, TaskService,
    },
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
    versions.sort_by(|left, right| compare_created_at(&right.created_at, &left.created_at));
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
    reserve_update(&state, &game_uid)?;
    let context = (|| {
        let (game, profile, latest) = load_update_context(&state, &game_uid)?;
        GameBodyUpdateService::validate_managed_game(&state.games_root()?, &game)?;
        Ok((game, profile, latest))
    })();
    let (game, profile, latest) = match context {
        Ok(value) => value,
        Err(error) => {
            release_update(&state, &game_uid);
            return Err(error);
        }
    };
    let task_id = match TaskService::create(
        &state,
        "update_game_body",
        TaskCategory::BodyTransfer,
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
        let result = crate::services::game_body_update_service::run_update(
            &app_handle,
            &task_id_for_thread,
            &game,
            profile.as_ref(),
            latest.as_ref(),
            &source_path,
        );
        if crate::services::game_body_update_service::finish_update_task(
            &app_handle,
            &task_id_for_thread,
            &game_uid,
            result,
        ) {
            if auto_upload_enabled(&app_handle) {
                if let Err(error) =
                    package_game_body(app_handle.clone(), app_handle.state(), game_uid.clone())
                {
                    eprintln!("GameSaver 自动创建本体包失败：{error}");
                }
            }
        }
    });
    Ok(task_id)
}

#[tauri::command]
pub fn retry_game_body_update(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
) -> Result<String, String> {
    let game_uid = game_uid.trim().to_string();
    state.claim_game_update(&game_uid, true)?;
    let task_id = match TaskService::create(
        &state,
        "retry_game_body_update",
        TaskCategory::BodyTransfer,
        Some(game_uid.clone()),
        "准备恢复游戏更新事务",
    ) {
        Ok(id) => id,
        Err(error) => {
            release_update(&state, &game_uid);
            return Err(error);
        }
    };
    let worker_id = task_id.clone();
    std::thread::spawn(move || {
        let result =
            crate::services::game_body_update_service::retry_update(&app, &worker_id, &game_uid);
        crate::services::game_body_update_service::finish_update_task(
            &app, &worker_id, &game_uid, result,
        );
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
    let package_claim = match state.claim_body_package(&game_uid) {
        Ok(claim) => claim,
        Err(error) => {
            if let Some(task_id) = active_body_package_task(&state, &game_uid) {
                return Ok(task_id);
            }
            return Err(error);
        }
    };
    let context = (|| {
        let game = load_body_game(&state, &game_uid)?;
        let protected_paths = body_save_exclusion_paths(&state, &game)?;
        Ok::<_, String>((game, protected_paths))
    })();
    let (game, protected_paths) = match context {
        Ok(value) => value,
        Err(error) => {
            state.release_operation(&package_claim);
            return Err(error);
        }
    };
    let task_id = match TaskService::create(
        &state,
        "package_game_body",
        TaskCategory::BodyTransfer,
        Some(game_uid.clone()),
        "准备创建游戏本体包",
    ) {
        Ok(task_id) => task_id,
        Err(error) => {
            state.release_operation(&package_claim);
            return Err(error);
        }
    };
    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        let result =
            package_game_body_task(&app_handle, &task_id_for_thread, &game, &protected_paths);
        app_handle.state::<AppState>().release_operation(&package_claim);
        match result {
            Ok(summary) => {
                let version_id = summary
                    .get("versionId")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                TaskService::finish(
                    &app_handle.state(),
                    &task_id_for_thread,
                    TaskStatus::Success,
                    100,
                    "游戏本体包创建完成",
                    Some(summary),
                    None,
                );
                if let Some(version_id) = version_id.filter(|_| auto_upload_enabled(&app_handle)) {
                    if let Err(error) = crate::commands::baidu_commands::upload_game_body_package(
                        app_handle.clone(),
                        app_handle.state(),
                        game_uid.clone(),
                        version_id,
                    ) {
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
    reserve_update(&state, &game_uid)?;
    let context = (|| {
        let root = state.library_root_path()?;
        let store = state.store.lock().map_err(|_| "读取游戏记录失败")?;
        let version = local_package_for_deletion(&store, &game_uid, &version_id)?;
        Ok::<_, String>((version, root))
    })();
    let (version, root) = match context {
        Ok(value) => value,
        Err(error) => {
            release_update(&state, &game_uid);
            return Err(error);
        }
    };
    let task_id = match TaskService::create(
        &state,
        "delete_game_body_package",
        TaskCategory::BodyTransfer,
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
            &version,
            &root,
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
    super::game_commands::start_game_removal(
        app,
        &state,
        game_uid,
        Some(crate::services::RemovalKind::Uninstall),
    )
}

fn delete_game_body_package_task(
    app: &AppHandle,
    task_id: &str,
    version: &GameBodyVersion,
    root: &Path,
) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        20,
        "正在清理本地 ZIP 缓存",
        None,
    );
    let package_path = validated_local_package_path(root, version)?;
    if let Err(error) = std::fs::remove_file(&package_path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            return Err(format!("清理本地 ZIP 缓存失败，已保留记录：{error}"));
        }
    }
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        70,
        "正在更新本体版本记录",
        None,
    );
    let keeps_archive = state.with_store_mut(|candidate| {
        let Some(record) = candidate.body_versions.iter().find(|item| {
            item.game_uid == version.game_uid
                && item.version_id == version.version_id
                && item.package_path == version.package_path
        }) else {
            return Err("游戏本体包版本不存在".to_string());
        };
        let keeps_archive = !record.archive_path.trim().is_empty();
        crate::services::GameRemovalService::forget_local_packages(
            candidate,
            &version.game_uid,
            Some(&version.version_id),
        );
        GameRepository::persist(app, candidate)
            .map_err(|error| format!("保存本体版本记录失败：{error}"))?;
        Ok(keeps_archive)
    })?;
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
    if let Err(error) = state.with_store_mut(|candidate| {
        let current = candidate
            .games
            .iter_mut()
            .find(|current| current.game_uid == game.game_uid)
            .ok_or_else(|| "游戏已移除，无法登记本体包".to_string())?;
        current.record_pending_body_package(&version_id);
        candidate.body_versions.push(body_version);
        GameRepository::persist(app, candidate)
    }) {
        let _ = std::fs::remove_file(&result.package_path);
        return Err(format!("保存游戏本体包记录失败：{error}"));
    }
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

fn local_package_for_deletion(
    store: &AppStore,
    game_uid: &str,
    version_id: &str,
) -> Result<GameBodyVersion, String> {
    let game = store.games.iter().find(|game| game.game_uid == game_uid).ok_or("游戏不存在")?;
    if matches!(game.lifecycle, GameLifecycle::Removing) {
        return Err("该游戏正在删除".to_string());
    }
    store
        .body_versions
        .iter()
        .find(|item| {
            item.game_uid == game_uid
                && item.version_id == version_id
                && item.package_path.is_some()
        })
        .cloned()
        .ok_or_else(|| "游戏本体包版本不存在".to_string())
}

fn validated_local_package_path(root: &Path, version: &GameBodyVersion) -> Result<PathBuf, String> {
    let recorded = Path::new(version.package_path.as_deref().ok_or("没有本地 ZIP 缓存")?);
    let file_name = recorded
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| is_safe_path_segment(name))
        .ok_or("本地 ZIP 文件名无效")?;
    if !is_safe_path_segment(&version.game_uid)
        || !recorded.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
    {
        return Err("本地 ZIP 缓存记录无效".to_string());
    }
    let expected = root.join("body-packages").join(&version.game_uid).join(file_name);
    if normalize_path(recorded) != normalize_path(&expected) {
        return Err("本地 ZIP 不在该游戏的缓存目录内，已停止删除".to_string());
    }
    validate_target(root, &expected)?;
    Ok(expected)
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

fn load_update_context(
    state: &AppState,
    game_uid: &str,
) -> Result<(crate::domain::Game, Option<SaveProfile>, Option<SaveVersion>), String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "lock GameSaver store failed".to_string())?;
    update_context(&store, game_uid)
}

fn update_context(
    store: &AppStore,
    game_uid: &str,
) -> Result<(crate::domain::Game, Option<SaveProfile>, Option<SaveVersion>), String> {
    let game =
        GameLibraryService::find(store, game_uid).ok_or_else(|| "游戏不存在".to_string())?;
    if !matches!(game.lifecycle, GameLifecycle::Active | GameLifecycle::NeedsRepair) {
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
        .cloned();
    if profile.is_none() && !game.save_protection_skipped && GameLibraryService::is_installed(&game) {
        return Err("存档保护配置不存在".to_string());
    }
    let latest = profile.as_ref().and_then(|_| {
        let id = game.latest_save_version_id.as_deref()?;
        store
            .save_versions
            .iter()
            .find(|version| version.game_uid == game_uid && version.version_id == id)
            .cloned()
    });
    Ok((game, profile, latest))
}

fn reserve_update(state: &AppState, game_uid: &str) -> Result<(), String> {
    state.claim_game_update(game_uid, false)
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
    state.release_operation(&AppState::game_operation_key(game_uid));
}

fn now_iso() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{domain::Game, test_support::TempWorkspace};

    #[test]
    fn body_repair_uses_existing_game_record_even_without_save_profile() {
        let root = TempWorkspace::new("repair-missing-game");
        let game_path = root.join("managed-game");
        let mut game = Game::new_pending("Test", game_path.to_string_lossy(), "game.exe");
        let uid = game.game_uid.clone();
        let latest = SaveVersion::new(uid.clone(), "1".into(), Vec::new());
        game.latest_save_version_id = Some(latest.version_id.clone());
        let mut store = AppStore {
            games: vec![game],
            save_versions: vec![latest],
            ..AppStore::default()
        };
        assert!(update_context(&store, &uid).is_err());

        store.games[0].lifecycle = GameLifecycle::NeedsRepair;
        let (target, profile, latest) = update_context(&store, &uid).unwrap();
        assert_eq!(target.game_uid, uid);
        assert_eq!(target.game_key, store.games[0].game_key);
        assert!(profile.is_none() && latest.is_none());

        let profile = SaveProfile::new(uid.clone(), "hash".into(), Vec::new(), 100, "2".into());
        store.games[0].save_profile_id = Some(profile.profile_id.clone());
        store.save_profiles.push(profile);
        let (_, profile, latest) = update_context(&store, &uid).unwrap();
        assert!(profile.is_some());
        assert_eq!(latest.unwrap().version_id, store.save_versions[0].version_id);

        std::fs::create_dir(&game_path).unwrap();
        std::fs::write(game_path.join("game.exe"), b"fixture").unwrap();
        store.games[0].lifecycle = GameLifecycle::Active;
        store.save_profiles.clear();
        assert!(update_context(&store, &uid).is_err());
    }

    #[test]
    fn local_zip_deletion_accepts_an_uninstalled_game_and_uses_its_recorded_cache_path() {
        let root = TempWorkspace::new("local-zip-delete");
        let mut game = Game::new_pending("test", root.join("games/test").to_string_lossy(), "game.exe");
        game.lifecycle = GameLifecycle::NeedsRepair;
        let package = root.join("body-packages").join(&game.game_uid).join("install-transaction.zip");
        let uid = game.game_uid.clone();
        let mut store = AppStore {
            games: vec![game],
            body_versions: vec![GameBodyVersion {
                version_id: "v1".into(),
                game_uid: uid.clone(),
                created_at: "1".into(),
                archive_path: String::new(),
                file_count: 1,
                total_bytes: 10,
                package_path: Some(package.to_string_lossy().into_owned()),
                sha256: None,
                excluded_items: Vec::new(),
                upload_status: None,
                remote_path: None,
                remote_fs_id: None,
                remote_size: None,
            }],
            ..AppStore::default()
        };
        let mut version = local_package_for_deletion(&store, &uid, "v1").unwrap();
        assert_eq!(validated_local_package_path(&root, &version).unwrap(), package);
        version.package_path = Some(root.join("body-packages/other/other.zip").to_string_lossy().into_owned());
        assert!(validated_local_package_path(&root, &version).is_err());
        store.games[0].lifecycle = GameLifecycle::Removing;
        assert!(local_package_for_deletion(&store, &uid, "v1").is_err());
    }
}
