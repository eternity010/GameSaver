use crate::{
    app_state::AppState,
    domain::{
        game::{CloudStatus, LaunchConfig},
        Game, GameBodyVersion, GameHealth, GameLifecycle, TaskRetry, TaskStatus,
    },
    repositories::{BaiduConfigRepository, GameRepository},
    services::{
        BaiduConnectionStatus, BaiduNetdiskClient, BaiduQuota, BodyPackageService, CloudGamePage,
        CloudGameSummary, CloudManifestService, GameLibraryService, RemoteBodyPackageList,
        RemoteFile, TaskService,
    },
};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager, State};
use uuid::Uuid;

const REMOTE_ROOT: &str = "/apps/GameSaver/games";

#[tauri::command]
pub fn get_baidu_status(app: AppHandle) -> Result<BaiduConnectionStatus, String> {
    let app_data_dir = app_data_dir(&app)?;
    let config = BaiduConfigRepository::load(&app_data_dir)?;
    Ok(BaiduNetdiskClient::connection_status_with_credentials(
        &app_data_dir,
        config.as_ref().map(|value| value.app_key.as_str()),
        config.as_ref().map(|value| value.secret_key.as_str()),
    ))
}

#[tauri::command]
pub fn get_baidu_quota(app: AppHandle) -> Result<BaiduQuota, String> {
    let client = load_baidu_client(&app)?;
    client.quota()
}

#[tauri::command]
pub fn get_cloud_game_cover(app: AppHandle, game_key: String) -> Result<Option<Vec<u8>>, String> {
    let game_key = game_key.trim();
    if game_key.is_empty() {
        return Ok(None);
    }
    let remote_dir = match remote_body_dir(game_key) {
        Ok(dir) => dir,
        Err(_) => return Ok(None),
    };
    let base_data_dir = app_data_dir(&app)?;
    let cache_root = base_data_dir.join("cloud-manifest-cache");
    let cover_file = cache_root
        .join(CloudManifestService::cache_folder_name(&remote_dir))
        .join("cover.jpg");
    if cover_file.is_file() {
        std::fs::read(cover_file)
            .map(Some)
            .map_err(|error| format!("读取云端封面缓存失败：{error}"))
    } else {
        Ok(None)
    }
}

#[tauri::command]
pub fn get_cloud_game_cover_path(
    app: AppHandle,
    game_key: String,
) -> Result<Option<String>, String> {
    let game_key = game_key.trim();
    if game_key.is_empty() {
        return Ok(None);
    }
    let remote_dir = match remote_body_dir(game_key) {
        Ok(dir) => dir,
        Err(_) => return Ok(None),
    };
    let base_data_dir = app_data_dir(&app)?;
    let cache_root = base_data_dir.join("cloud-manifest-cache");
    let cover_file = cache_root
        .join(CloudManifestService::cache_folder_name(&remote_dir))
        .join("cover.jpg");
    if cover_file.is_file() {
        Ok(Some(cover_file.to_string_lossy().to_string()))
    } else {
        Ok(None)
    }
}

#[tauri::command]
pub fn get_cloud_game_cover_paths(
    app: AppHandle,
) -> Result<std::collections::HashMap<String, String>, String> {
    let base_data_dir = app_data_dir(&app)?;
    let cache_root = base_data_dir.join("cloud-manifest-cache");
    let mut map = std::collections::HashMap::new();
    if !cache_root.is_dir() {
        return Ok(map);
    }
    let Ok(entries) = std::fs::read_dir(&cache_root) else {
        return Ok(map);
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let cover_file = dir.join("cover.jpg");
        if !cover_file.is_file() {
            continue;
        }
        let catalog_file = dir.join("game.json");
        let manifest_file = dir.join("manifest.json");
        let mut game_key = None;
        if catalog_file.is_file() {
            if let Ok(content) = std::fs::read_to_string(&catalog_file) {
                if let Ok(catalog) = serde_json::from_str::<serde_json::Value>(&content) {
                    game_key = catalog
                        .get("gameKey")
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string);
                }
            }
        }
        if game_key.is_none() && manifest_file.is_file() {
            if let Ok(content) = std::fs::read_to_string(&manifest_file) {
                if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&content) {
                    game_key = manifest
                        .get("gameKey")
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string);
                }
            }
        }
        if let Some(key) = game_key {
            map.insert(key, cover_file.to_string_lossy().to_string());
        }
    }
    Ok(map)
}

#[tauri::command]
pub fn list_cloud_games(
    app: AppHandle,
    state: State<AppState>,
    page: Option<usize>,
    page_size: Option<usize>,
) -> Result<CloudGamePage, String> {
    let client = load_baidu_client(&app)?;
    let page = page.unwrap_or(1).max(1);
    let page_size = page_size.unwrap_or(9).clamp(1, 24);
    let start = page.saturating_sub(1).saturating_mul(page_size);
    let root_page = match client.list_page(REMOTE_ROOT, start, page_size) {
        Ok(files) => files,
        Err(error) if error.contains("(-9)") || error.contains("(-8)") => {
            return Ok(CloudGamePage {
                games: Vec::new(),
                page,
                page_size,
                has_more: false,
            })
        }
        Err(error) => return Err(error),
    };
    let local_store = state
        .store
        .lock()
        .map_err(|_| "读取本地游戏信息失败".to_string())?
        .clone();
    let local_games = local_store.games;
    let local_body_versions = local_store.body_versions;
    let base_data_dir = app_data_dir(&app)?;
    let temporary_root = base_data_dir.join("cloud-manifest-temp");
    let cache_root = base_data_dir.join("cloud-manifest-cache");

    let directories = root_page
        .files
        .into_iter()
        .filter(|file| file.is_dir)
        .filter_map(|root| {
            let remote_segment = root
                .path
                .strip_prefix(&format!("{REMOTE_ROOT}/"))
                .filter(|value| !value.is_empty() && !value.contains('/'))?;
            let remote_game_key = Game::derive_game_key(remote_segment);
            let directory = remote_body_dir(&remote_game_key).ok()?;
            Some((remote_game_key, directory))
        })
        .collect::<Vec<_>>();

    let summaries = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(directories.len());
        for (remote_game_key, directory) in directories {
            let client_ref = &client;
            let temporary_root_ref = &temporary_root;
            let cache_root_ref = &cache_root;
            let local_games_ref = &local_games;
            let local_body_versions_ref = &local_body_versions;

            handles.push(scope.spawn(move || -> Option<CloudGameSummary> {
                let files = match client_ref.list(&directory) {
                    Ok(files) => files,
                    Err(_) => return None,
                };
                let manifest = CloudManifestService::read(
                    client_ref,
                    &files,
                    &directory,
                    temporary_root_ref,
                    Some(cache_root_ref),
                )
                .ok()
                .flatten();
                let catalog = CloudManifestService::read_catalog(
                    client_ref,
                    &files,
                    &directory,
                    temporary_root_ref,
                    Some(cache_root_ref),
                )
                .ok()
                .flatten();
                let catalog_game_key = catalog
                    .as_ref()
                    .map(|value| value.game_key.trim())
                    .filter(|value| !value.is_empty());
                let manifest_game_key = manifest
                    .as_ref()
                    .map(|value| value.game_key.trim())
                    .filter(|value| !value.is_empty());
                if catalog.is_none() && manifest.is_none() {
                    return None;
                }
                if catalog_game_key.is_some_and(|key| key != remote_game_key)
                    || manifest_game_key.is_some_and(|key| key != remote_game_key)
                {
                    return None;
                }
                let has_cover = CloudManifestService::read_cover(
                    client_ref,
                    &files,
                    &directory,
                    temporary_root_ref,
                    Some(cache_root_ref),
                )
                .ok()
                .flatten()
                .is_some();
                let local = local_games_ref.iter().find(|game| {
                    matches!(game.lifecycle, GameLifecycle::Active)
                        && game.game_key == remote_game_key
                });
                let remote_game_uid = catalog
                    .as_ref()
                    .map(|value| value.game_uid.clone())
                    .or_else(|| manifest.as_ref().map(|value| value.game_uid.clone()))
                    .or_else(|| local.map(|game| game.game_uid.clone()))
                    .unwrap_or_default();
                let local_versions = local_body_versions_ref
                    .iter()
                    .filter(|version| local.is_some_and(|game| version.game_uid == game.game_uid))
                    .cloned()
                    .collect::<Vec<_>>();
                let packages =
                    CloudManifestService::project(&files, manifest.as_ref(), &local_versions)
                        .packages;
                let package = packages
                    .iter()
                    .max_by(|left, right| {
                        left.created_at
                            .cmp(&right.created_at)
                            .then_with(|| left.version_id.cmp(&right.version_id))
                    })
                    .cloned()?;
                let installed = local.is_some_and(|game| {
                    Path::new(&game.managed_path)
                        .join(&game.launch.executable_relative_path)
                        .is_file()
                });
                Some(CloudGameSummary {
                    game_key: remote_game_key.clone(),
                    game_uid: remote_game_uid,
                    display_name: catalog
                        .as_ref()
                        .map(|value| value.display_name.clone())
                        .or_else(|| local.map(|game| game.display_name.clone()))
                        .unwrap_or_else(|| remote_game_key.clone()),
                    executable_relative_path: catalog
                        .as_ref()
                        .map(|value| value.executable_relative_path.clone())
                        .or_else(|| local.map(|game| game.launch.executable_relative_path.clone())),
                    arguments: catalog
                        .as_ref()
                        .map(|value| value.arguments.clone())
                        .or_else(|| local.map(|game| game.launch.arguments.clone()))
                        .unwrap_or_default(),
                    working_directory_relative_path: catalog
                        .as_ref()
                        .and_then(|value| value.working_directory_relative_path.clone())
                        .or_else(|| {
                            local.and_then(|game| {
                                game.launch.working_directory_relative_path.clone()
                            })
                        }),
                    version_id: package.version_id,
                    package_path: package.path,
                    package_fs_id: package.fs_id,
                    package_size: package.size,
                    package_sha256: package.package_sha256,
                    file_count: package.file_count,
                    total_bytes: package.total_bytes,
                    created_at: package.created_at,
                    installed,
                    versions: packages,
                    has_cover,
                })
            }));
        }
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok().flatten())
            .collect::<Vec<_>>()
    });

    let mut result = summaries;
    result.sort_by(|left, right| {
        left.display_name
            .to_lowercase()
            .cmp(&right.display_name.to_lowercase())
    });
    Ok(CloudGamePage {
        games: result,
        page,
        page_size,
        has_more: root_page.has_more,
    })
}

#[tauri::command]
pub fn install_cloud_game(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
    game_key: Option<String>,
    remote_path: String,
    remote_fs_id: Option<u64>,
) -> Result<String, String> {
    let game_uid = game_uid.trim().to_string();
    let game_key = game_key
        .filter(|value| !value.trim().is_empty())
        .map(|value| Game::derive_game_key(&value));
    let remote_path = remote_path.trim().to_string();
    let remote_game_key = game_key
        .as_deref()
        .ok_or_else(|| "云端游戏缺少 gameKey，无法定位本体包".to_string())?;
    let directory = remote_body_dir(remote_game_key)?;
    validate_remote_package_path(&directory, &remote_path)?;
    reserve_transfer(&state, &game_uid)?;
    let task_id = match TaskService::create(
        &state,
        "install_cloud_game",
        Some(game_uid.clone()),
        "准备安装云端游戏",
    ) {
        Ok(task_id) => task_id,
        Err(error) => {
            release_transfer(&state, &game_uid);
            return Err(error);
        }
    };
    if let Err(error) = TaskService::set_retry(
        &state,
        &task_id,
        TaskRetry {
            operation: "install_cloud_game".to_string(),
            game_uid: game_uid.clone(),
            game_key: game_key.clone(),
            version_id: None,
            remote_path: Some(remote_path.clone()),
            remote_fs_id,
        },
    ) {
        release_transfer(&state, &game_uid);
        return Err(error);
    }
    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        let result = install_cloud_game_task(
            &app_handle,
            &task_id_for_thread,
            &game_uid,
            game_key.as_deref(),
            &remote_path,
            remote_fs_id,
        );
        release_transfer(&app_handle.state(), &game_uid);
        match result {
            Ok(summary) => TaskService::finish(
                &app_handle.state(),
                &task_id_for_thread,
                TaskStatus::Success,
                100,
                "游戏本体已安装",
                Some(summary),
                None,
            ),
            Err(error) if error == "任务已取消" => TaskService::finish(
                &app_handle.state(),
                &task_id_for_thread,
                TaskStatus::Cancelled,
                100,
                "已取消云端游戏安装",
                None,
                None,
            ),
            Err(error) => TaskService::finish(
                &app_handle.state(),
                &task_id_for_thread,
                TaskStatus::Failed,
                100,
                "云端游戏安装失败",
                None,
                Some(error),
            ),
        }
    });
    Ok(task_id)
}

#[tauri::command]
pub fn delete_remote_body_package(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
    game_key: Option<String>,
    remote_path: String,
    remote_fs_id: Option<u64>,
) -> Result<String, String> {
    let game_uid = game_uid.trim().to_string();
    let game_key = game_key
        .filter(|value| !value.trim().is_empty())
        .map(|value| Game::derive_game_key(&value));
    let remote_path = remote_path.trim().to_string();
    let local_game = state
        .store
        .lock()
        .map_err(|_| "读取本地游戏信息失败".to_string())?
        .games
        .iter()
        .find(|game| game.game_uid == game_uid)
        .cloned();
    let remote_game_key = game_key
        .or_else(|| local_game.as_ref().map(|game| game.game_key.clone()))
        .ok_or_else(|| "云端游戏缺少 gameKey，无法定位本体包".to_string())?;
    let directory = remote_body_dir(&remote_game_key)?;
    validate_remote_package_path(&directory, &remote_path)?;
    let transfer_key = local_game
        .as_ref()
        .map(|game| game.game_uid.clone())
        .unwrap_or_else(|| format!("remote:{remote_game_key}"));
    reserve_transfer(&state, &transfer_key)?;
    let task_id = match TaskService::create(
        &state,
        "delete_remote_body_package",
        Some(game_uid.clone()),
        "准备删除云端本体包",
    ) {
        Ok(task_id) => task_id,
        Err(error) => {
            release_transfer(&state, &transfer_key);
            return Err(error);
        }
    };
    if let Err(error) = TaskService::set_retry(
        &state,
        &task_id,
        TaskRetry {
            operation: "delete_remote_body_package".to_string(),
            game_uid: game_uid.clone(),
            game_key: Some(remote_game_key.clone()),
            version_id: None,
            remote_path: Some(remote_path.clone()),
            remote_fs_id,
        },
    ) {
        release_transfer(&state, &transfer_key);
        return Err(error);
    }
    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    let transfer_key_for_thread = transfer_key.clone();
    std::thread::spawn(move || {
        let result = delete_remote_body_task(
            &app_handle,
            &task_id_for_thread,
            local_game.as_ref(),
            &remote_game_key,
            &directory,
            &remote_path,
            remote_fs_id,
        );
        release_transfer(&app_handle.state(), &transfer_key_for_thread);
        match result {
            Ok(summary) => TaskService::finish(
                &app_handle.state(),
                &task_id_for_thread,
                TaskStatus::Success,
                100,
                "云端本体包已删除",
                Some(summary),
                None,
            ),
            Err(error) if error == "任务已取消" => TaskService::finish(
                &app_handle.state(),
                &task_id_for_thread,
                TaskStatus::Cancelled,
                100,
                "已取消云端本体包删除",
                None,
                None,
            ),
            Err(error) => TaskService::finish(
                &app_handle.state(),
                &task_id_for_thread,
                TaskStatus::Failed,
                100,
                "云端本体包删除失败",
                None,
                Some(error),
            ),
        }
    });
    Ok(task_id)
}

#[tauri::command]
pub fn list_remote_body_packages(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
) -> Result<RemoteBodyPackageList, String> {
    let game_uid = game_uid.trim().to_string();
    ensure_game(&state, &game_uid)?;
    let client = load_baidu_client(&app)?;
    let game = load_game(&state, &game_uid)?;
    let directory = remote_body_dir(&game.game_key)?;
    let files = match client.list(&directory) {
        Ok(files) => files,
        Err(error) if error.contains("(-9)") || error.contains("(-8)") => {
            let result = RemoteBodyPackageList {
                packages: Vec::new(),
                manifest_available: false,
                manifest_status: "missing".to_string(),
                manifest_updated_at: None,
                warnings: Vec::new(),
            };
            reconcile_local_body_versions(&app, &state, &game_uid, &result)?;
            return Ok(result);
        }
        Err(error) => return Err(error),
    };
    let base_data_dir = app_data_dir(&app)?;
    let temporary_root = base_data_dir.join("cloud-manifest-temp");
    let cache_root = base_data_dir.join("cloud-manifest-cache");
    let mut warnings = Vec::new();
    let manifest = match CloudManifestService::read(
        &client,
        &files,
        &directory,
        &temporary_root,
        Some(&cache_root),
    ) {
        Ok(manifest) => manifest,
        Err(error) => {
            warnings.push(format!("云端版本清单读取失败：{error}"));
            None
        }
    };
    let local_versions = state
        .store
        .lock()
        .map_err(|_| "读取本体版本记录失败".to_string())?
        .body_versions
        .iter()
        .filter(|version| version.game_uid == game_uid)
        .cloned()
        .collect::<Vec<_>>();
    let mut result = CloudManifestService::project(&files, manifest.as_ref(), &local_versions);
    result.warnings.extend(warnings);
    reconcile_local_body_versions(&app, &state, &game_uid, &result)?;
    Ok(result)
}

#[tauri::command]
pub fn repair_cloud_body_manifest(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
) -> Result<String, String> {
    let game_uid = game_uid.trim().to_string();
    let game = load_game(&state, &game_uid)?;
    let directory = remote_body_dir(&game.game_key)?;
    reserve_transfer(&state, &game_uid)?;
    let task_id = match TaskService::create(
        &state,
        "repair_cloud_body_manifest",
        Some(game_uid.clone()),
        "准备修复云端版本清单",
    ) {
        Ok(task_id) => task_id,
        Err(error) => {
            release_transfer(&state, &game_uid);
            return Err(error);
        }
    };
    if let Err(error) = TaskService::set_retry(
        &state,
        &task_id,
        TaskRetry {
            operation: "repair_cloud_body_manifest".to_string(),
            game_uid: game_uid.clone(),
            game_key: None,
            version_id: None,
            remote_path: None,
            remote_fs_id: None,
        },
    ) {
        release_transfer(&state, &game_uid);
        return Err(error);
    }
    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        let result =
            repair_cloud_body_manifest_task(&app_handle, &task_id_for_thread, &game, &directory);
        release_transfer(&app_handle.state(), &game_uid);
        match result {
            Ok(summary) => TaskService::finish(
                &app_handle.state(),
                &task_id_for_thread,
                TaskStatus::Success,
                100,
                "云端版本清单已修复",
                Some(summary),
                None,
            ),
            Err(error) if error == "任务已取消" => TaskService::finish(
                &app_handle.state(),
                &task_id_for_thread,
                TaskStatus::Cancelled,
                100,
                "已取消云端版本清单修复",
                None,
                None,
            ),
            Err(error) => TaskService::finish(
                &app_handle.state(),
                &task_id_for_thread,
                TaskStatus::Failed,
                100,
                "云端版本清单修复失败",
                None,
                Some(error),
            ),
        }
    });
    Ok(task_id)
}

#[tauri::command]
pub fn upload_game_body_package(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
    version_id: String,
) -> Result<String, String> {
    let game_uid = game_uid.trim().to_string();
    let version_id = version_id.trim().to_string();
    let (game, version) = load_body_version(&state, &game_uid, &version_id)?;
    let package_path = version
        .package_path
        .as_deref()
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from);
    reserve_transfer(&state, &game_uid)?;
    let task_id = match TaskService::create(
        &state,
        "upload_game_body_package",
        Some(game_uid.clone()),
        "准备上传游戏本体包",
    ) {
        Ok(task_id) => task_id,
        Err(error) => {
            release_transfer(&state, &game_uid);
            return Err(error);
        }
    };
    if let Err(error) = TaskService::set_retry(
        &state,
        &task_id,
        TaskRetry {
            operation: "upload_game_body_package".to_string(),
            game_uid: game_uid.clone(),
            game_key: None,
            version_id: Some(version_id.clone()),
            remote_path: None,
            remote_fs_id: None,
        },
    ) {
        release_transfer(&state, &game_uid);
        return Err(error);
    }
    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        let result = upload_body_task(
            &app_handle,
            &task_id_for_thread,
            &game,
            &version,
            package_path.as_deref(),
        );
        release_transfer(&app_handle.state(), &game_uid);
        match result {
            Ok(summary) => TaskService::finish(
                &app_handle.state(),
                &task_id_for_thread,
                TaskStatus::Success,
                100,
                "游戏本体包上传完成",
                Some(summary),
                None,
            ),
            Err(error) if error == "任务已取消" => {
                let _ = update_upload_record(
                    &app_handle,
                    &game_uid,
                    &version.version_id,
                    "local_only",
                    None,
                );
                TaskService::finish(
                    &app_handle.state(),
                    &task_id_for_thread,
                    TaskStatus::Cancelled,
                    100,
                    "已取消本体包上传",
                    None,
                    None,
                )
            }
            Err(error) => {
                let _ = update_upload_record(
                    &app_handle,
                    &game_uid,
                    &version.version_id,
                    "failed",
                    None,
                );
                TaskService::finish(
                    &app_handle.state(),
                    &task_id_for_thread,
                    TaskStatus::Failed,
                    100,
                    "游戏本体包上传失败",
                    None,
                    Some(error),
                )
            }
        }
    });
    Ok(task_id)
}

#[tauri::command]
pub fn download_game_body_package(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
    remote_path: String,
    remote_fs_id: Option<u64>,
) -> Result<String, String> {
    let game_uid = game_uid.trim().to_string();
    let remote_path = remote_path.trim().to_string();
    let game = load_game(&state, &game_uid)?;
    let directory = remote_body_dir(&game.game_key)?;
    validate_remote_package_path(&directory, &remote_path)?;
    reserve_transfer(&state, &game_uid)?;
    let task_id = match TaskService::create(
        &state,
        "download_game_body_package",
        Some(game_uid.clone()),
        "准备下载游戏本体包",
    ) {
        Ok(task_id) => task_id,
        Err(error) => {
            release_transfer(&state, &game_uid);
            return Err(error);
        }
    };
    if let Err(error) = TaskService::set_retry(
        &state,
        &task_id,
        TaskRetry {
            operation: "download_game_body_package".to_string(),
            game_uid: game_uid.clone(),
            game_key: None,
            version_id: None,
            remote_path: Some(remote_path.clone()),
            remote_fs_id,
        },
    ) {
        release_transfer(&state, &game_uid);
        return Err(error);
    }
    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        let result = download_body_task(
            &app_handle,
            &task_id_for_thread,
            &game,
            &directory,
            &remote_path,
            remote_fs_id,
        );
        release_transfer(&app_handle.state(), &game_uid);
        match result {
            Ok(summary) => TaskService::finish(
                &app_handle.state(),
                &task_id_for_thread,
                TaskStatus::Success,
                100,
                "游戏本体包下载完成",
                Some(summary),
                None,
            ),
            Err(error) if error == "任务已取消" => TaskService::finish(
                &app_handle.state(),
                &task_id_for_thread,
                TaskStatus::Cancelled,
                100,
                "已取消本体包下载",
                None,
                None,
            ),
            Err(error) => TaskService::finish(
                &app_handle.state(),
                &task_id_for_thread,
                TaskStatus::Failed,
                100,
                "游戏本体包下载失败",
                None,
                Some(error),
            ),
        }
    });
    Ok(task_id)
}

fn upload_body_task(
    app: &AppHandle,
    task_id: &str,
    game: &Game,
    version: &GameBodyVersion,
    package_path: Option<&Path>,
) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    update_upload_record(app, &game.game_uid, &version.version_id, "syncing", None)?;
    let package_path =
        package_path.ok_or_else(|| "该本体版本没有本地 ZIP，无法上传".to_string())?;
    if !package_path.is_file() {
        return Err("本地本体 ZIP 不存在，请先创建或下载本体包".to_string());
    }
    let package_size = std::fs::metadata(package_path)
        .map_err(|error| format!("读取本体 ZIP 大小失败：{error}"))?
        .len();
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        2,
        "正在校验本体 ZIP",
        None,
    );
    let quota = load_baidu_client(app)?.quota()?;
    if quota.free < package_size {
        return Err(format!(
            "百度网盘剩余空间不足：需要 {} MB，可用 {} MB",
            package_size / 1024 / 1024,
            quota.free / 1024 / 1024
        ));
    }
    BodyPackageService::validate_package_for_upload(
        package_path,
        &game.game_uid,
        &game.launch.executable_relative_path,
        version.sha256.as_deref(),
    )?;
    let client = load_baidu_client(app)?;
    let directory = remote_body_dir(&game.game_key)?;
    client.ensure_directory(&directory)?;
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        5,
        "正在连接百度网盘",
        None,
    );
    let remote_path = format!("{directory}/{}.zip", version.version_id);
    let remote = client.upload_file(package_path, &remote_path, |progress, message| {
        TaskService::update(
            &state,
            task_id,
            TaskStatus::Running,
            progress,
            message,
            None,
        );
        !TaskService::is_cancelled(&state, task_id)
    })?;
    update_upload_record(
        app,
        &game.game_uid,
        &version.version_id,
        "synced",
        Some(&remote),
    )?;
    sync_cloud_manifest(app, &state, &client, &game.game_uid)?;
    Ok(serde_json::json!({
        "versionId": version.version_id,
        "remotePath": remote.path,
        "remoteFsId": remote.fs_id,
        "size": remote.size,
    }))
}

fn download_body_task(
    app: &AppHandle,
    task_id: &str,
    game: &Game,
    directory: &str,
    remote_path: &str,
    remote_fs_id: Option<u64>,
) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    let client = load_baidu_client(app)?;
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        3,
        "正在读取百度网盘本体版本",
        None,
    );
    let remote_files = client.list(directory)?;
    let remote = remote_files
        .iter()
        .find(|file| {
            file.path == remote_path && remote_fs_id.is_none_or(|fs_id| fs_id == file.fs_id)
        })
        .cloned()
        .ok_or_else(|| "百度网盘中没有找到这个本体包，可能已被删除".to_string())?;
    let base_data_dir = app_data_dir(app)?;
    let temporary_root = base_data_dir.join("cloud-manifest-temp");
    let manifest_cache_root = base_data_dir.join("cloud-manifest-cache");
    let cloud_manifest = CloudManifestService::read(
        &client,
        &remote_files,
        directory,
        &temporary_root,
        Some(&manifest_cache_root),
    )
    .map_err(|error| format!("云端版本清单校验失败，已停止下载：{error}"))?;
    let cloud_entry = cloud_manifest.as_ref().and_then(|manifest| {
        manifest.versions.iter().find(|version| {
            version.package_path == remote.path && version.package_fs_id == remote.fs_id
        })
    });
    if cloud_manifest.is_some() && cloud_entry.is_none() {
        return Err("云端本体包未登记在版本清单中，已停止下载".to_string());
    }
    let temporary_version_id = Uuid::new_v4().to_string();
    let cache_root = body_package_cache_root(app)?;
    let temporary_path = BodyPackageService::package_path(
        &cache_root,
        &game.game_uid,
        &format!(".download-{temporary_version_id}"),
    );
    let downloaded_sha256 =
        client.download_file(&remote, &temporary_path, |progress, message| {
            TaskService::update(
                &state,
                task_id,
                TaskStatus::Running,
                progress,
                message,
                None,
            );
            !TaskService::is_cancelled(&state, task_id)
        })?;
    let manifest = match BodyPackageService::validate_package_with_known_hash(
        &temporary_path,
        &game.game_uid,
        &game.launch.executable_relative_path,
        cloud_entry.and_then(|entry| entry.package_sha256.as_deref()),
        Some(&downloaded_sha256),
    ) {
        Ok(manifest) => manifest,
        Err(error) => {
            let _ = std::fs::remove_file(&temporary_path);
            return Err(format!("下载的本体包校验失败：{error}"));
        }
    };
    let version_id = manifest.version_id.clone();
    if cloud_entry.is_some_and(|entry| entry.version_id != version_id) {
        let _ = std::fs::remove_file(&temporary_path);
        return Err("下载的本体包版本与云端版本清单不一致".to_string());
    }
    if version_id.trim().is_empty()
        || version_id.contains('/')
        || version_id.contains('\\')
        || version_id == "."
        || version_id == ".."
    {
        let _ = std::fs::remove_file(&temporary_path);
        return Err("下载的本体包版本标识无效".to_string());
    }
    let package_path = BodyPackageService::package_path(&cache_root, &game.game_uid, &version_id);
    if package_path.exists() {
        let _ = std::fs::remove_file(&temporary_path);
        return Err("该本体版本已下载到本地，无需重复下载".to_string());
    }
    std::fs::rename(&temporary_path, &package_path).map_err(|err| {
        let _ = std::fs::remove_file(&temporary_path);
        format!("提交下载的本体包失败：{err}")
    })?;
    let package_sha256 = downloaded_sha256;
    let body_version = GameBodyVersion {
        version_id: version_id.clone(),
        game_uid: game.game_uid.clone(),
        created_at: now_iso(),
        archive_path: String::new(),
        file_count: manifest.file_count,
        total_bytes: manifest.total_bytes,
        package_path: Some(package_path.to_string_lossy().to_string()),
        sha256: Some(package_sha256),
        excluded_items: manifest.excluded_items,
        upload_status: Some("synced".to_string()),
        remote_path: Some(remote.path),
        remote_fs_id: Some(remote.fs_id),
        remote_size: Some(remote.size),
    };
    let mut candidate = state
        .store
        .lock()
        .map_err(|_| "读取本体版本记录失败".to_string())?
        .clone();
    candidate
        .body_versions
        .retain(|item| !(item.game_uid == game.game_uid && item.version_id == version_id));
    candidate.body_versions.push(body_version);
    if let Err(error) = GameRepository::persist(app, &candidate) {
        let _ = std::fs::remove_file(&package_path);
        return Err(format!("保存下载的本体版本失败：{error}"));
    }
    *state
        .store
        .lock()
        .map_err(|_| "更新本体版本记录失败".to_string())? = candidate;
    Ok(
        serde_json::json!({ "versionId": version_id, "remotePath": remote_path, "fileCount": manifest.file_count }),
    )
}

fn delete_remote_body_task(
    app: &AppHandle,
    task_id: &str,
    local_game: Option<&Game>,
    game_key: &str,
    directory: &str,
    remote_path: &str,
    remote_fs_id: Option<u64>,
) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        10,
        "正在删除云端本体包",
        None,
    );
    if TaskService::is_cancelled(&state, task_id) {
        return Err("任务已取消".to_string());
    }
    let client = load_baidu_client(app)?;
    let remote = client
        .list(directory)?
        .into_iter()
        .find(|file| {
            file.path == remote_path
                && !file.is_dir
                && remote_fs_id.is_none_or(|fs_id| fs_id == file.fs_id)
        })
        .ok_or_else(|| "百度网盘中没有找到这个本体包，可能已被删除".to_string())?;
    client.delete_file(&remote.path)?;
    if let Some(game) = local_game {
        clear_upload_record(app, &game.game_uid, &remote.path, remote.fs_id)?;
        sync_cloud_manifest(app, &state, &client, &game.game_uid)?;
    } else {
        rebuild_remote_manifest(app, &client, directory, game_key)?;
    }
    Ok(
        serde_json::json!({ "remotePath": remote.path, "remoteFsId": remote.fs_id, "size": remote.size }),
    )
}

fn rebuild_remote_manifest(
    app: &AppHandle,
    client: &BaiduNetdiskClient,
    directory: &str,
    game_key: &str,
) -> Result<(), String> {
    let remote_files = client.list(directory)?;
    let base_data_dir = app_data_dir(app)?;
    let temporary_root = base_data_dir.join("cloud-manifest-temp");
    let cache_root = base_data_dir.join("cloud-manifest-cache");
    let existing = CloudManifestService::read(
        client,
        &remote_files,
        directory,
        &temporary_root,
        Some(&cache_root),
    )?;
    let game_uid = existing
        .as_ref()
        .map(|manifest| manifest.game_uid.clone())
        .or_else(|| {
            CloudManifestService::read_catalog(
                client,
                &remote_files,
                directory,
                &temporary_root,
                Some(&cache_root),
            )
            .ok()
            .flatten()
            .map(|catalog| catalog.game_uid)
        })
        .ok_or_else(|| "云端游戏缺少可用的版本清单，无法更新删除结果".to_string())?;
    CloudManifestService::rebuild(
        client,
        directory,
        game_key,
        &game_uid,
        &remote_files,
        &[],
        &temporary_root,
    )?;
    Ok(())
}

fn clear_upload_record(
    app: &AppHandle,
    game_uid: &str,
    remote_path: &str,
    remote_fs_id: u64,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut candidate = state
        .store
        .lock()
        .map_err(|_| "读取本体版本记录失败".to_string())?
        .clone();
    for version in candidate.body_versions.iter_mut().filter(|version| {
        version.game_uid == game_uid
            && version.remote_path.as_deref() == Some(remote_path)
            && version.remote_fs_id == Some(remote_fs_id)
    }) {
        version.upload_status = Some("local_only".to_string());
        version.remote_path = None;
        version.remote_fs_id = None;
        version.remote_size = None;
    }
    GameRepository::persist(app, &candidate)?;
    *state
        .store
        .lock()
        .map_err(|_| "更新本体版本记录失败".to_string())? = candidate;
    Ok(())
}

fn update_upload_record(
    app: &AppHandle,
    game_uid: &str,
    version_id: &str,
    status: &str,
    remote: Option<&RemoteFile>,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut candidate = state
        .store
        .lock()
        .map_err(|_| "读取本体版本记录失败".to_string())?
        .clone();
    let version = candidate
        .body_versions
        .iter_mut()
        .find(|item| item.game_uid == game_uid && item.version_id == version_id)
        .ok_or_else(|| "本体版本记录不存在".to_string())?;
    version.upload_status = Some(status.to_string());
    if let Some(remote) = remote {
        version.remote_path = Some(remote.path.clone());
        version.remote_fs_id = Some(remote.fs_id);
        version.remote_size = Some(remote.size);
    }
    GameRepository::persist(app, &candidate)?;
    *state
        .store
        .lock()
        .map_err(|_| "更新本体版本记录失败".to_string())? = candidate;
    Ok(())
}

fn sync_cloud_manifest(
    app: &AppHandle,
    state: &AppState,
    client: &BaiduNetdiskClient,
    game_uid: &str,
) -> Result<(), String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "读取本体版本记录失败".to_string())?
        .clone();
    let game_key = store
        .games
        .iter()
        .find(|game| game.game_uid == game_uid)
        .map(|game| game.game_key.clone())
        .ok_or_else(|| "游戏不存在，无法生成云端版本清单".to_string())?;
    let remote_dir = remote_body_dir(&game_key)?;
    client.ensure_directory(&remote_dir)?;
    let remote_files = client.list(&remote_dir)?;
    let versions = store.body_versions;
    let temporary_root = app_data_dir(app)?.join("cloud-manifest-temp");
    CloudManifestService::rebuild(
        client,
        &remote_dir,
        &game_key,
        game_uid,
        &remote_files,
        &versions,
        &temporary_root,
    )?;
    sync_cloud_catalog(app, state, client, game_uid, &remote_dir)
}

fn repair_cloud_body_manifest_task(
    app: &AppHandle,
    task_id: &str,
    game: &Game,
    directory: &str,
) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        5,
        "正在读取云端本体包",
        None,
    );
    let client = load_baidu_client(app)?;
    client.ensure_directory(directory)?;
    let remote_files = match client.list(directory) {
        Ok(files) => files,
        Err(error) if error.contains("(-9)") || error.contains("(-8)") => Vec::new(),
        Err(error) => return Err(error),
    };
    if TaskService::is_cancelled(&state, task_id) {
        return Err("任务已取消".to_string());
    }
    let local_versions = state
        .store
        .lock()
        .map_err(|_| "读取本体版本记录失败".to_string())?
        .body_versions
        .iter()
        .filter(|version| version.game_uid == game.game_uid)
        .cloned()
        .collect::<Vec<_>>();
    let package_count = remote_files
        .iter()
        .filter(|file| !file.is_dir && file.path.to_ascii_lowercase().ends_with(".zip"))
        .count();
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        35,
        format!("正在整理 {package_count} 个云端本体包"),
        None,
    );
    let manifest = CloudManifestService::rebuild(
        &client,
        directory,
        &game.game_key,
        &game.game_uid,
        &remote_files,
        &local_versions,
        &app_data_dir(app)?.join("cloud-manifest-temp"),
    )?;
    sync_cloud_catalog(app, &state, &client, &game.game_uid, directory)?;
    Ok(serde_json::json!({
        "versionCount": manifest.versions.len(),
        "remoteFileCount": remote_files.len(),
    }))
}

fn sync_cloud_catalog(
    app: &AppHandle,
    state: &AppState,
    client: &BaiduNetdiskClient,
    game_uid: &str,
    remote_dir: &str,
) -> Result<(), String> {
    let game = state
        .store
        .lock()
        .map_err(|_| "读取游戏信息失败".to_string())?
        .games
        .iter()
        .find(|game| game.game_uid == game_uid)
        .cloned()
        .ok_or_else(|| "游戏不存在，无法同步云端游戏信息".to_string())?;
    let base_data_dir = app_data_dir(app)?;
    let temporary_root = base_data_dir.join("cloud-manifest-temp");
    let cache_root = base_data_dir.join("cloud-manifest-cache");
    CloudManifestService::write_catalog(
        client,
        remote_dir,
        &CloudManifestService::catalog_from_game(&game),
        &temporary_root,
    )?;
    if let Some(cover) = &game.cover {
        if let Ok(root) = state.library_root_path() {
            let display_path = root.join(&cover.display_path);
            if display_path.is_file() {
                if let Ok(bytes) = std::fs::read(&display_path) {
                    let _ = CloudManifestService::write_cover(
                        client,
                        remote_dir,
                        &bytes,
                        &temporary_root,
                        Some(&cache_root),
                    );
                }
            }
        }
    }
    Ok(())
}

fn install_cloud_game_task(
    app: &AppHandle,
    task_id: &str,
    game_uid: &str,
    game_key: Option<&str>,
    remote_path: &str,
    remote_fs_id: Option<u64>,
) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    let app_data_dir = app_data_dir(app)?;
    let client = load_baidu_client(app)?;
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        3,
        "正在读取云端游戏信息",
        None,
    );
    let remote_game_key =
        game_key.ok_or_else(|| "云端游戏缺少 gameKey，无法定位本体包".to_string())?;
    let directory = remote_body_dir(remote_game_key)?;
    let remote_files = client.list(&directory)?;
    let remote = remote_files
        .iter()
        .find(|file| {
            file.path == remote_path
                && !file.is_dir
                && remote_fs_id.is_none_or(|fs_id| fs_id == file.fs_id)
        })
        .cloned()
        .ok_or_else(|| "百度网盘中没有找到这个游戏本体包，可能已被删除".to_string())?;
    let temporary_root = app_data_dir.join("cloud-manifest-temp");
    let cache_root = app_data_dir.join("cloud-manifest-cache");
    let catalog = CloudManifestService::read_catalog(
        &client,
        &remote_files,
        &directory,
        &temporary_root,
        Some(&cache_root),
    )?
    .ok_or_else(|| "云端游戏缺少启动信息，请在本地重新上传一次游戏本体包".to_string())?;
    let manifest = CloudManifestService::read(
        &client,
        &remote_files,
        &directory,
        &temporary_root,
        Some(&cache_root),
    )
    .ok()
    .flatten();
    let existing = state
        .store
        .lock()
        .map_err(|_| "读取本地游戏信息失败".to_string())?
        .games
        .iter()
        .find(|game| game_key.is_some_and(|key| game.game_key == key) || game.game_uid == game_uid)
        .cloned();
    let local_uid = existing
        .as_ref()
        .map(|game| game.game_uid.clone())
        .unwrap_or_else(|| game_uid.to_string());
    let local_versions = state
        .store
        .lock()
        .map_err(|_| "读取本地本体版本信息失败".to_string())?
        .body_versions
        .iter()
        .filter(|version| version.game_uid == local_uid)
        .cloned()
        .collect::<Vec<_>>();
    let package = CloudManifestService::project(&remote_files, manifest.as_ref(), &local_versions)
        .packages
        .into_iter()
        .find(|package| package.path == remote.path && package.fs_id == remote.fs_id);
    let expected_sha256 = package
        .as_ref()
        .and_then(|package| package.package_sha256.as_deref());
    let games_root = state.games_root()?;
    let managed_path = existing
        .as_ref()
        .map(|game| PathBuf::from(&game.managed_path))
        .unwrap_or_else(|| games_root.join(&local_uid));
    if existing.as_ref().is_some_and(|game| {
        Path::new(&game.managed_path)
            .join(&game.launch.executable_relative_path)
            .is_file()
    }) {
        return Err("这个游戏已经安装，无需重复下载".to_string());
    }
    let version_id = package
        .as_ref()
        .map(|package| package.version_id.clone())
        .unwrap_or_else(|| remote_file_name_without_extension(remote_path));
    let cache_root = state.body_packages_root()?;
    let package_path = BodyPackageService::package_path(&cache_root, &local_uid, &version_id);
    if package_path.is_file() {
        let _ = std::fs::remove_file(&package_path);
    }
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        5,
        "正在下载游戏本体",
        None,
    );
    let downloaded_sha256 = client.download_file(&remote, &package_path, |progress, message| {
        TaskService::update(
            &state,
            task_id,
            TaskStatus::Running,
            5 + progress.saturating_mul(3) / 4,
            message,
            None,
        );
        !TaskService::is_cancelled(&state, task_id)
    })?;
    if TaskService::is_cancelled(&state, task_id) {
        let _ = std::fs::remove_file(&package_path);
        return Err("任务已取消".to_string());
    }
    let staging = games_root.join(format!(".{local_uid}.cloud-installing"));
    if staging.exists() {
        return Err("已有未完成的云端游戏安装暂存目录，请重启应用后重试".to_string());
    }
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        82,
        "正在校验并安装游戏本体",
        None,
    );
    let body_manifest = BodyPackageService::extract_package_with_known_hash(
        &package_path,
        &staging,
        &local_uid,
        &catalog.executable_relative_path,
        expected_sha256,
        Some(&downloaded_sha256),
        |progress, message| {
            TaskService::update(
                &state,
                task_id,
                TaskStatus::Running,
                82 + progress / 6,
                message,
                None,
            )
        },
        || TaskService::is_cancelled(&state, task_id),
    )?;
    if TaskService::is_cancelled(&state, task_id) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err("任务已取消".to_string());
    }
    std::fs::create_dir_all(&games_root).map_err(|error| format!("创建游戏库目录失败：{error}"))?;
    if managed_path.exists() {
        return Err("游戏受管目录已存在，未覆盖现有文件".to_string());
    }
    std::fs::rename(&staging, &managed_path)
        .map_err(|error| format!("提交云端游戏安装失败：{error}"))?;
    let mut candidate = state
        .store
        .lock()
        .map_err(|_| "读取游戏记录失败".to_string())?
        .clone();
    let mut game = existing.unwrap_or_else(|| {
        let mut game = Game::new_pending(
            &catalog.display_name,
            managed_path.to_string_lossy(),
            &catalog.executable_relative_path,
        );
        game.game_uid = local_uid.to_string();
        game
    });
    if game.game_key.trim().is_empty() {
        game.game_key = Game::derive_game_key(&catalog.game_key);
    }
    game.display_name = catalog.display_name;
    game.managed_path = managed_path.to_string_lossy().to_string();
    game.lifecycle = GameLifecycle::Active;
    game.health = GameHealth::NeedsAttention;
    game.cloud_status = CloudStatus::Synced;
    game.launch = LaunchConfig {
        executable_relative_path: catalog.executable_relative_path,
        arguments: catalog.arguments,
        working_directory_relative_path: catalog.working_directory_relative_path,
    };
    if game.cover.is_none() {
        let manifest_cache_root = app_data_dir.join("cloud-manifest-cache");
        if let Ok(Some(cover_bytes)) = CloudManifestService::read_cover(
            &client,
            &remote_files,
            &directory,
            &temporary_root,
            Some(&manifest_cache_root),
        ) {
            if let Ok(library_root) = state.library_root_path() {
                let cover_id = Uuid::new_v4().simple().to_string();
                let cover_dir = library_root.join("covers").join(&local_uid).join(&cover_id);
                if std::fs::create_dir_all(&cover_dir).is_ok() {
                    let _ = std::fs::write(cover_dir.join("display.jpg"), &cover_bytes);
                    let _ = std::fs::write(cover_dir.join("original.jpg"), &cover_bytes);
                    game.cover = Some(crate::domain::game::GameCover {
                        original_path: format!("covers/{local_uid}/{cover_id}/original.jpg"),
                        display_path: format!("covers/{local_uid}/{cover_id}/display.jpg"),
                        crop: crate::domain::game::CoverCrop {
                            aspect_width: 16,
                            aspect_height: 9,
                            output_width: 1280,
                            output_height: 720,
                        },
                        position: crate::domain::game::CoverPosition {
                            zoom_milli: 1000,
                            offset_x_milli: 0,
                            offset_y_milli: 0,
                        },
                    });
                }
            }
        }
    }
    if let Some(existing_game) = candidate
        .games
        .iter_mut()
        .find(|item| item.game_uid == local_uid)
    {
        *existing_game = game.clone();
    } else {
        GameLibraryService::register_pending(&mut candidate, game.clone())?;
    }
    candidate
        .body_versions
        .retain(|version| !(version.game_uid == local_uid && version.version_id == version_id));
    candidate.body_versions.push(GameBodyVersion {
        version_id: version_id.clone(),
        game_uid: local_uid.to_string(),
        created_at: package
            .as_ref()
            .and_then(|package| package.created_at.clone())
            .unwrap_or_else(now_iso),
        archive_path: String::new(),
        file_count: body_manifest.file_count,
        total_bytes: body_manifest.total_bytes,
        package_path: Some(package_path.to_string_lossy().to_string()),
        sha256: Some(downloaded_sha256),
        excluded_items: body_manifest.excluded_items,
        upload_status: Some("synced".to_string()),
        remote_path: Some(remote.path.clone()),
        remote_fs_id: Some(remote.fs_id),
        remote_size: Some(remote.size),
    });
    if let Err(error) = GameRepository::persist(app, &candidate) {
        let _ = std::fs::remove_dir_all(&managed_path);
        return Err(format!("保存已安装游戏记录失败：{error}"));
    }
    *state
        .store
        .lock()
        .map_err(|_| "更新游戏记录失败".to_string())? = candidate;
    Ok(serde_json::json!({
        "gameUid": local_uid,
        "versionId": version_id,
        "managedPath": managed_path,
        "fileCount": body_manifest.file_count,
        "totalBytes": body_manifest.total_bytes,
    }))
}

fn reconcile_local_body_versions(
    app: &AppHandle,
    state: &AppState,
    game_uid: &str,
    remote: &RemoteBodyPackageList,
) -> Result<(), String> {
    let mut candidate = state
        .store
        .lock()
        .map_err(|_| "读取本体版本记录失败".to_string())?
        .clone();
    let mut changed = false;
    for version in candidate
        .body_versions
        .iter_mut()
        .filter(|version| version.game_uid == game_uid)
    {
        let matched = remote.packages.iter().find(|package| {
            package.version_id == version.version_id
                || version.remote_path.as_deref() == Some(package.path.as_str())
        });
        match matched {
            Some(package) => {
                if version.remote_path.as_deref() != Some(package.path.as_str()) {
                    version.remote_path = Some(package.path.clone());
                    changed = true;
                }
                if version.remote_fs_id != Some(package.fs_id) {
                    version.remote_fs_id = Some(package.fs_id);
                    changed = true;
                }
                if version.remote_size != Some(package.size) {
                    version.remote_size = Some(package.size);
                    changed = true;
                }
                let next_status = match package.sync_state.as_str() {
                    "mismatch" => "failed",
                    "synced" => "synced",
                    _ => "manifest_pending",
                };
                if version.upload_status.as_deref() != Some(next_status) {
                    version.upload_status = Some(next_status.to_string());
                    changed = true;
                }
            }
            None if version.remote_path.is_some()
                || version.upload_status.as_deref() == Some("synced") =>
            {
                version.upload_status = Some("local_only".to_string());
                version.remote_path = None;
                version.remote_fs_id = None;
                version.remote_size = None;
                changed = true;
            }
            None => {}
        }
    }
    if !changed {
        return Ok(());
    }
    GameRepository::persist(app, &candidate)?;
    *state
        .store
        .lock()
        .map_err(|_| "更新本体版本记录失败".to_string())? = candidate;
    Ok(())
}

fn load_body_version(
    state: &AppState,
    game_uid: &str,
    version_id: &str,
) -> Result<(Game, GameBodyVersion), String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "lock GameSaver store failed".to_string())?;
    let game = store
        .games
        .iter()
        .find(|game| game.game_uid == game_uid)
        .cloned()
        .ok_or_else(|| "游戏不存在".to_string())?;
    if !matches!(game.lifecycle, crate::domain::GameLifecycle::Active) {
        return Err("游戏尚未完成设置，不能传输本体包".to_string());
    }
    let version = store
        .body_versions
        .iter()
        .find(|version| version.game_uid == game_uid && version.version_id == version_id)
        .cloned()
        .ok_or_else(|| "本体版本不存在".to_string())?;
    Ok((game, version))
}

fn load_game(state: &AppState, game_uid: &str) -> Result<Game, String> {
    let game = state
        .store
        .lock()
        .map_err(|_| "lock GameSaver store failed".to_string())?
        .games
        .iter()
        .find(|game| game.game_uid == game_uid)
        .cloned()
        .ok_or_else(|| "游戏不存在".to_string())?;
    if !matches!(game.lifecycle, crate::domain::GameLifecycle::Active) {
        return Err("游戏尚未完成设置，不能传输本体包".to_string());
    }
    Ok(game)
}

fn ensure_game(state: &AppState, game_uid: &str) -> Result<(), String> {
    load_game(state, game_uid).map(|_| ())
}

fn reserve_transfer(state: &AppState, game_uid: &str) -> Result<(), String> {
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
        return Err("游戏运行中，暂时不能传输游戏本体".to_string());
    }
    operations.insert(game_uid.to_string());
    Ok(())
}

fn release_transfer(state: &AppState, game_uid: &str) {
    if let Ok(mut operations) = state.save_operations.lock() {
        operations.remove(game_uid);
    }
}

pub(crate) fn load_baidu_client(app: &AppHandle) -> Result<BaiduNetdiskClient, String> {
    let app_data_dir = app_data_dir(app)?;
    let config = BaiduConfigRepository::load(&app_data_dir)?;
    match config {
        Some(config) => BaiduNetdiskClient::load_from_app_data_with_credentials(
            &app_data_dir,
            Some(&config.app_key),
            Some(&config.secret_key),
        ),
        None => BaiduNetdiskClient::load_from_app_data(&app_data_dir),
    }
}

pub(crate) fn app_data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|err| format!("解析 GameSaver 数据目录失败：{err}"))
}

fn body_package_cache_root(app: &AppHandle) -> Result<PathBuf, String> {
    app.state::<AppState>().body_packages_root()
}

pub(crate) fn remote_body_dir(game_key: &str) -> Result<String, String> {
    let game_key = game_key.trim();
    if game_key.is_empty()
        || game_key == "."
        || game_key == ".."
        || game_key.contains('/')
        || game_key.contains('\\')
        || game_key.chars().any(char::is_control)
    {
        return Err("gameKey 包含不支持的远程路径字符".to_string());
    }
    Ok(format!("{REMOTE_ROOT}/{game_key}/body"))
}

fn validate_remote_package_path(directory: &str, path: &str) -> Result<(), String> {
    let prefix = format!("{directory}/");
    let name = path
        .strip_prefix(&prefix)
        .ok_or_else(|| "远程本体包路径不属于当前游戏".to_string())?;
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name == "."
        || name == ".."
        || !name.to_ascii_lowercase().ends_with(".zip")
    {
        return Err("远程本体包路径无效".to_string());
    }
    Ok(())
}

fn remote_file_name_without_extension(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("cloud-game")
        .to_string()
}

fn now_iso() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use super::{remote_body_dir, validate_remote_package_path};

    #[test]
    fn remote_body_directory_uses_game_key_verbatim() {
        assert_eq!(
            remote_body_dir("monster black market").unwrap(),
            "/apps/GameSaver/games/monster black market/body"
        );
        assert_eq!(
            remote_body_dir("肉遊びver1.0.7").unwrap(),
            "/apps/GameSaver/games/肉遊びver1.0.7/body"
        );
    }

    #[test]
    fn remote_body_directory_rejects_path_segments() {
        for value in ["", ".", "..", "game/name", r"game\name", "game\nname"] {
            assert!(
                remote_body_dir(value).is_err(),
                "accepted unsafe gameKey: {value:?}"
            );
        }
    }

    #[test]
    fn remote_package_path_must_stay_in_game_key_directory() {
        let directory = remote_body_dir("monster black market").unwrap();
        assert!(validate_remote_package_path(
            &directory,
            "/apps/GameSaver/games/monster black market/body/v1.zip"
        )
        .is_ok());
        assert!(validate_remote_package_path(
            &directory,
            "/apps/GameSaver/games/other game/body/v1.zip"
        )
        .is_err());
    }
}
