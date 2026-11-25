use crate::{
    app_state::AppState,
    commands::{game_commands::safe_cover_path, run_blocking},
    domain::{
        compare_optional_created_at, is_safe_path_segment, AppStore, Game, GameBodyVersion, GameLifecycle,
        TaskCategory, TaskRetry, TaskStatus,
    },
    repositories::GameRepository,
    services::{
        BaiduConnectionStatus, BaiduNetdiskClient, BaiduQuota, BodyPackageService, CloudGamePage,
        CloudGameSummary, CloudManifestService, GameBodyInstallService, GameLibraryService,
        RemoteBodyPackageList, RemoteFile, TaskService,
    },
};
use crate::services::cloud_manifest_service::CloudBodyManifest;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager, State};
use uuid::Uuid;

const REMOTE_ROOT: &str = crate::services::baidu_netdisk_service::REMOTE_GAMES_ROOT;
const BODY_MANIFEST_PENDING_PREFIX: &str = "本体包已上传，但云端版本清单更新失败：";

/// 查询百度网盘连接状态。
///
/// 走 [`crate::commands::run_blocking`]：token 过期时这里会真的发起一次刷新请求
/// （客户端总超时 120 秒），不能在主线程上等。
#[tauri::command]
pub async fn get_baidu_status(app: AppHandle) -> Result<BaiduConnectionStatus, String> {
    run_blocking(move || get_baidu_status_blocking(app)).await
}

fn get_baidu_status_blocking(app: AppHandle) -> Result<BaiduConnectionStatus, String> {
    let app_data_dir = app_data_dir(&app)?;
    Ok(BaiduNetdiskClient::connection_status(&app_data_dir))
}

/// 查询网盘容量：`load_baidu_client` 可能触发 token 刷新，`quota()` 是一次网络往返。
#[tauri::command]
pub async fn get_baidu_quota(app: AppHandle) -> Result<BaiduQuota, String> {
    run_blocking(move || get_baidu_quota_blocking(app)).await
}

fn get_baidu_quota_blocking(app: AppHandle) -> Result<BaiduQuota, String> {
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
pub async fn get_cloud_game_cover_path(
    app: AppHandle,
    game_key: String,
    remote_cover: Option<RemoteFile>,
) -> Result<Option<String>, String> {
    run_blocking(move || get_cloud_game_cover_path_blocking(app, game_key, remote_cover)).await
}

fn get_cloud_game_cover_path_blocking(
    app: AppHandle,
    game_key: String,
    remote_cover: Option<RemoteFile>,
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
    if let Some(remote_cover) = remote_cover {
        CloudManifestService::validate_store_cover(&remote_dir, &remote_cover)?;
        if CloudManifestService::load_cached_cover(&cache_root, &remote_dir, &remote_cover)
            .is_none()
        {
            let started = std::time::Instant::now();
            let client = load_baidu_client(&app)?;
            CloudManifestService::read_cover(
                &client,
                std::slice::from_ref(&remote_cover),
                &remote_dir,
                &base_data_dir.join("cloud-manifest-temp"),
                Some(&cache_root),
            )?;
            if CloudManifestService::load_cached_cover(&cache_root, &remote_dir, &remote_cover)
                .is_none()
            {
                return Err("保存商店封面缓存失败，请刷新后重试".to_string());
            }
            crate::logging::info(format!(
                "商店封面加载：{}，{} ms",
                game_key,
                started.elapsed().as_millis()
            ));
        }
    }
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

/// 列出云端游戏。
///
/// `client.list(REMOTE_ROOT)` 会拉取整个远端根目录、再在本地分页，是这个文件里最重的
/// 一次网络往返。`state` 改在阻塞闭包里从 `app` 取。
#[tauri::command]
pub async fn list_cloud_games(
    app: AppHandle,
    page: Option<usize>,
    page_size: Option<usize>,
    search: Option<String>,
    force: Option<bool>,
) -> Result<CloudGamePage, String> {
    run_blocking(move || {
        list_cloud_games_blocking(app, page, page_size, search, force.unwrap_or(false))
    })
    .await
}

fn list_cloud_games_blocking(
    app: AppHandle,
    page: Option<usize>,
    page_size: Option<usize>,
    search: Option<String>,
    force: bool,
) -> Result<CloudGamePage, String> {
    let started = std::time::Instant::now();
    let state = app.state::<AppState>();
    let client = load_baidu_client(&app)?;
    let authorization = client.store_cache_scope()?;
    let (games, cached, remaining) = state
        .cloud_store_cache
        .get_or_load(authorization, force, || read_cloud_store(&app, &state, &client))?;
    client.store_cache_scope()?;
    let mut result = CloudGamePage::from_games(
        games,
        page.unwrap_or(1),
        page_size.unwrap_or(9),
        search.as_deref(),
    );
    result.cache_ttl_ms = Some(remaining.as_millis() as u64);
    let local_games = state
        .store
        .lock()
        .map_err(|_| "读取本地游戏信息失败".to_string())?
        .games
        .clone();
    for summary in &mut result.games {
        summary.installed = local_games.iter().any(|game| {
            game.lifecycle == GameLifecycle::Active
                && game.game_key == summary.game_key
                && Path::new(&game.managed_path)
                    .join(&game.launch.executable_relative_path)
                    .is_file()
        });
    }
    crate::logging::info(format!(
        "商店分页加载：第 {} / {} 页，{} 个游戏，搜索后 {} 个，完整列表缓存命中 {}，总计 {} ms",
        result.page, result.total_pages, result.games.len(), result.total_count,
        cached, started.elapsed().as_millis()
    ));
    Ok(result)
}

fn read_cloud_store(
    app: &AppHandle,
    state: &AppState,
    client: &BaiduNetdiskClient,
) -> Result<Vec<CloudGameSummary>, String> {
    let root_started = std::time::Instant::now();
    let all_files = match client.list(REMOTE_ROOT) {
        Ok(files) => files,
        Err(error) if error.contains("(-9)") || error.contains("(-8)") => {
            return Ok(Vec::new());
        }
        Err(error) => return Err(error),
    };
    let root_elapsed = root_started.elapsed();
    let local_store = state
        .store
        .lock()
        .map_err(|_| "读取本地游戏信息失败".to_string())?
        .clone();
    let local_games = local_store.games;
    let local_body_versions = local_store.body_versions;
    let base_data_dir = app_data_dir(app)?;
    let temporary_root = base_data_dir.join("cloud-manifest-temp");
    let cache_root = base_data_dir.join("cloud-manifest-cache");

    let directories = all_files
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

    let cards_started = std::time::Instant::now();
    let mut summaries = Vec::with_capacity(directories.len());
    for batch in directories.chunks(9) {
        let batch_summaries = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(batch.len());
            for (remote_game_key, directory) in batch.iter().cloned() {
                let client_ref = client;
                let temporary_root_ref = &temporary_root;
                let cache_root_ref = &cache_root;
                let local_games_ref = &local_games;
                let local_body_versions_ref = &local_body_versions;

                handles.push(
                    scope.spawn(move || -> Result<Option<CloudGameSummary>, String> {
                        let files = match client_ref.list(&directory) {
                            Ok(files) => files,
                            Err(error) if error.contains("(-9)") || error.contains("(-8)") => {
                                return Ok(None);
                            }
                            Err(error) => {
                                return Err(format!(
                                    "读取云端游戏 {remote_game_key} 失败：{error}"
                                ));
                            }
                        };
                        if !files.iter().any(|file| {
                            !file.is_dir && file.path.to_ascii_lowercase().ends_with(".zip")
                        }) {
                            return Ok(None);
                        }
                        let (manifest, catalog) = CloudManifestService::read_store_metadata(
                            client_ref,
                            &files,
                            &directory,
                            temporary_root_ref,
                            cache_root_ref,
                        );
                        let catalog_game_key = catalog
                            .as_ref()
                            .map(|value| value.game_key.trim())
                            .filter(|value| !value.is_empty());
                        let manifest_game_key = manifest
                            .as_ref()
                            .map(|value| value.game_key.trim())
                            .filter(|value| !value.is_empty());
                        if catalog.is_none() && manifest.is_none() {
                            return Ok(None);
                        }
                        if catalog_game_key.is_some_and(|key| key != remote_game_key)
                            || manifest_game_key.is_some_and(|key| key != remote_game_key)
                        {
                            return Ok(None);
                        }
                        let cover_file = files
                            .iter()
                            .find(|file| {
                                file.path == CloudManifestService::cover_path(&directory)
                                    && !file.is_dir
                            })
                            .cloned();
                        let has_cover = cover_file.is_some();
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
                            .filter(|version| {
                                local.is_some_and(|game| version.game_uid == game.game_uid)
                            })
                            .cloned()
                            .collect::<Vec<_>>();
                        let packages = CloudManifestService::project(
                            &files,
                            manifest.as_ref(),
                            &local_versions,
                        )
                        .packages;
                        let Some(package) = packages
                            .iter()
                            .max_by(|left, right| {
                                compare_optional_created_at(
                                    left.created_at.as_deref(),
                                    right.created_at.as_deref(),
                                )
                                .then_with(|| left.version_id.cmp(&right.version_id))
                            })
                            .cloned()
                        else {
                            return Ok(None);
                        };
                        Ok(Some(CloudGameSummary {
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
                                .or_else(|| {
                                    local.map(|game| game.launch.executable_relative_path.clone())
                                }),
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
                            installed: false,
                            versions: packages,
                            has_cover,
                            cover_file,
                        }))
                    }),
                );
            }
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| "读取商店游戏的线程异常退出".to_string())?
                })
                .collect::<Result<Vec<_>, String>>()
        })?;
        summaries.extend(batch_summaries.into_iter().flatten());
    }
    crate::logging::info(format!(
        "商店完整列表读取：目录 {} 个，有效游戏 {} 个，根目录 {} ms，卡片读取 {} ms；封面独立加载",
        directories.len(),
        summaries.len(),
        root_elapsed.as_millis(),
        cards_started.elapsed().as_millis()
    ));

    Ok(summaries)
}

#[tauri::command]
pub fn install_cloud_game(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
    game_key: Option<String>,
    remote_path: String,
    remote_fs_id: Option<u64>,
    replace_existing: Option<bool>,
) -> Result<String, String> {
    let game_uid = game_uid.trim().to_string();
    let game_key = game_key
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_string());
    let remote_path = remote_path.trim().to_string();
    let remote_game_key = game_key
        .as_deref()
        .ok_or_else(|| "云端游戏缺少 gameKey，无法定位本体包".to_string())?;
    let directory = remote_body_dir(remote_game_key)?;
    validate_remote_package_path(&directory, &remote_path)?;
    let games_root = state.games_root()?;
    let target = {
        let store = state
            .store
            .lock()
            .map_err(|_| "读取本地游戏信息失败".to_string())?;
        GameBodyInstallService::resolve_target(
            &store,
            &games_root,
            &game_uid,
            remote_game_key,
            replace_existing.unwrap_or(false),
        )?
    };
    let install_key = format!("install:{}", target.game_key);
    state.claim_operation(&install_key, "该云端游戏已有安装任务正在进行")?;
    if let Err(error) = reserve_transfer(&state, &target.game_uid) {
        state.release_operation(&install_key);
        return Err(error);
    }
    let task_id = match TaskService::create(
        &state,
        "install_cloud_game",
        TaskCategory::BodyTransfer,
        Some(target.game_uid.clone()),
        "准备安装云端游戏",
    ) {
        Ok(task_id) => task_id,
        Err(error) => {
            release_transfer(&state, &target.game_uid);
            state.release_operation(&install_key);
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
        release_transfer(&state, &target.game_uid);
        state.release_operation(&install_key);
        TaskService::finish(
            &state,
            &task_id,
            TaskStatus::Failed,
            0,
            "创建云端安装任务失败",
            None,
            Some(error.clone()),
        );
        return Err(error);
    }
    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    std::thread::spawn(move || {
        let result = install_cloud_game_task(
            &app_handle,
            &task_id_for_thread,
            &game_uid,
            &target,
            &remote_path,
            remote_fs_id,
        );
        let recovery_pending = app_handle
            .state::<AppState>()
            .games_root()
            .map(|root| {
                root.join(format!(".{}.cloud-install.json", target.game_uid))
                    .exists()
            })
            .unwrap_or(true);
        if recovery_pending {
            crate::logging::warn(format!(
                "游戏 {} 的安装事务尚未清理，已保留操作锁，请重启应用恢复",
                target.game_uid
            ));
        } else {
            release_transfer(&app_handle.state(), &target.game_uid);
        }
        app_handle
            .state::<AppState>()
            .release_operation(&install_key);
        match result {
            Ok(summary) => {
                let message = if recovery_pending {
                    "游戏已安装，清理尚未完成，请重启应用恢复后继续操作"
                } else {
                    "游戏本体已安装"
                };
                TaskService::finish(
                    &app_handle.state(),
                    &task_id_for_thread,
                    TaskStatus::Success,
                    100,
                    message,
                    Some(summary),
                    None,
                )
            }
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
        TaskCategory::BodyTransfer,
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
pub async fn list_remote_body_packages(
    app: AppHandle,
    game_uid: String,
) -> Result<RemoteBodyPackageList, String> {
    run_blocking(move || list_remote_body_packages_blocking(app, game_uid)).await
}

fn list_remote_body_packages_blocking(
    app: AppHandle,
    game_uid: String,
) -> Result<RemoteBodyPackageList, String> {
    let state = app.state::<AppState>();
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
        TaskCategory::BodyTransfer,
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
    let package_claim = state.claim_body_package(&game_uid)?;
    let (game, version) = match load_body_version(&state, &game_uid, &version_id) {
        Ok(context) => context,
        Err(error) => {
            state.release_operation(&package_claim);
            return Err(error);
        }
    };
    let package_path = version
        .package_path
        .as_deref()
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from);
    let task_id = match TaskService::create(
        &state,
        "upload_game_body_package",
        TaskCategory::BodyTransfer,
        Some(game_uid.clone()),
        "准备上传游戏本体包",
    ) {
        Ok(task_id) => task_id,
        Err(error) => {
            state.release_operation(&package_claim);
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
        state.release_operation(&package_claim);
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
        match result {
            Ok(summary) => {
                app_handle
                    .state::<AppState>()
                    .release_operation(&package_claim);
                TaskService::finish(
                    &app_handle.state(),
                    &task_id_for_thread,
                    TaskStatus::Success,
                    100,
                    "游戏本体包上传完成",
                    Some(summary),
                    None,
                )
            }
            Err(error) if error == "任务已取消" => {
                let _ = update_upload_record(
                    &app_handle,
                    &game_uid,
                    &version.version_id,
                    "local_only",
                    None,
                );
                app_handle
                    .state::<AppState>()
                    .release_operation(&package_claim);
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
                let manifest_pending = error.starts_with(BODY_MANIFEST_PENDING_PREFIX);
                let _ = update_upload_record(
                    &app_handle,
                    &game_uid,
                    &version.version_id,
                    if manifest_pending {
                        "manifest_pending"
                    } else {
                        "failed"
                    },
                    None,
                );
                if manifest_pending {
                    let _ = TaskService::set_retry(
                        &app_handle.state(),
                        &task_id_for_thread,
                        TaskRetry {
                            operation: "repair_cloud_body_manifest".to_string(),
                            game_uid: game_uid.clone(),
                            game_key: None,
                            version_id: None,
                            remote_path: None,
                            remote_fs_id: None,
                        },
                    );
                }
                app_handle
                    .state::<AppState>()
                    .release_operation(&package_claim);
                TaskService::finish(
                    &app_handle.state(),
                    &task_id_for_thread,
                    TaskStatus::Failed,
                    100,
                    if manifest_pending {
                        "本体包已上传，云端版本清单待修复"
                    } else {
                        "游戏本体包上传失败"
                    },
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
        TaskCategory::BodyTransfer,
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
    BodyPackageService::validate_package_structure_for_upload(
        package_path,
        version,
        &game.launch.executable_relative_path,
    )?;
    let client = load_baidu_client(app)?;
    let directory = remote_body_dir(&game.game_key)?;
    client.ensure_directory(&directory)?;
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        2,
        "正在校验本体包并准备上传",
        None,
    );
    let remote_path = format!("{directory}/{}.zip", version.version_id);
    let remote = client.upload_file(
        package_path,
        &remote_path,
        version.sha256.as_deref(),
        |progress, message| {
            TaskService::update(
                &state,
                task_id,
                TaskStatus::Running,
                progress.min(90),
                message,
                None,
            );
            !TaskService::is_cancelled(&state, task_id)
        },
    )?;
    update_upload_record(
        app,
        &game.game_uid,
        &version.version_id,
        "synced",
        Some(&remote),
    )?;
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        92,
        "本体包已上传，正在更新云端版本清单",
        None,
    );
    sync_cloud_manifest(app, &state, &client, &game.game_uid)
        .and_then(|manifest| persist_body_publication(app, &state, &game.game_uid, &manifest))
        .map_err(|error| format!("{BODY_MANIFEST_PENDING_PREFIX}{error}"))?;
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        98,
        "云端版本清单已更新",
        None,
    );
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
    )?;
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
    // 判定收敛到领域层的 `is_safe_path_segment`：这里原先是一份内联的等价检查（空/`/`/`\`/
    // `.`/`..`），与 `install_cloud_game_task` 那侧构成「一处查、一处漏」的重复判定。收在这里
    // 之后两侧强度一致，且比内联那份更严（多拒控制字符，并先 `trim`）。
    // 保留显式检查而不是只靠下面的 `?`：这条路径要顺手删掉已下载的临时文件。
    if !is_safe_path_segment(&version_id) {
        let _ = std::fs::remove_file(&temporary_path);
        return Err("下载的本体包版本标识无效".to_string());
    }
    let package_path = BodyPackageService::package_path(&cache_root, &game.game_uid, &version_id)?;
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
    state.with_store_mut(|candidate| {
        candidate
            .body_versions
            .retain(|item| !(item.game_uid == game.game_uid && item.version_id == version_id));
        candidate.body_versions.push(body_version);
        if let Err(error) = GameRepository::persist(app, candidate) {
            let _ = std::fs::remove_file(&package_path);
            return Err(format!("保存下载的本体版本失败：{error}"));
        }
        Ok(())
    })?;
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
    state.with_store_mut(|candidate| {
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
        GameRepository::persist(app, candidate)?;
        Ok(())
    })
}

fn update_upload_record(
    app: &AppHandle,
    game_uid: &str,
    version_id: &str,
    status: &str,
    remote: Option<&RemoteFile>,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    state.with_store_mut(|candidate| {
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
        GameRepository::persist(app, candidate)?;
        Ok(())
    })
}

fn sync_cloud_manifest(
    app: &AppHandle,
    state: &AppState,
    client: &BaiduNetdiskClient,
    game_uid: &str,
) -> Result<CloudBodyManifest, String> {
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
    let manifest = CloudManifestService::rebuild(
        client,
        &remote_dir,
        &game_key,
        game_uid,
        &remote_files,
        &versions,
        &temporary_root,
    )?;
    sync_cloud_catalog(app, state, client, game_uid, &remote_dir)?;
    Ok(manifest)
}

fn persist_body_publication(
    app: &AppHandle,
    state: &AppState,
    game_uid: &str,
    manifest: &CloudBodyManifest,
) -> Result<(), String> {
    state.with_store_mut(|candidate| {
        if record_body_publication(candidate, game_uid, manifest) {
            GameRepository::persist(app, candidate)?;
        }
        Ok(())
    })
}

fn record_body_publication(store: &mut AppStore, game_uid: &str, manifest: &CloudBodyManifest) -> bool {
    let Some(game) = store.games.iter_mut().find(|game| {
        game.game_uid == game_uid && game.game_key == manifest.game_key
            && game.pending_body_upload.is_some()
    }) else {
        return false;
    };
    for entry in manifest.versions.iter().filter(|entry| entry.file_count > 0) {
        let published = store.body_versions.iter_mut().find(|version| {
            version.game_uid == game_uid
                && version.version_id == entry.version_id
                && version.remote_path.as_deref() == Some(entry.package_path.as_str())
                && version.remote_fs_id == Some(entry.package_fs_id)
                && version.remote_size == Some(entry.package_size)
                && version.sha256.is_some()
                && version.sha256 == entry.package_sha256
        });
        if let Some(version) = published {
            if game.complete_body_upload(&version.version_id) {
                version.upload_status = Some("synced".to_string());
                return true;
            }
        }
    }
    false
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
    persist_body_publication(app, &state, &game.game_uid, &manifest)?;
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
    if let (Some(cover), Ok(root)) = (&game.cover, state.library_root_path()) {
        // 与封面读取共用同一份守卫：`..` 或越出 `covers/<uid>` 一律当作没有封面。
        // 这里若照旧裸 `join`，库目录之外的文件会被当成封面传到云端。
        if let Ok(display_path) = safe_cover_path(&root, game_uid, &cover.display_path) {
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
    target: &crate::services::game_body_install_service::CloudInstallTarget,
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
    let remote_game_key = target.game_key.as_str();
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
    if catalog.game_uid != game_uid || catalog.game_key != remote_game_key {
        return Err("云端游戏信息已变化，请刷新商店后重试".to_string());
    }
    let manifest = CloudManifestService::read(
        &client,
        &remote_files,
        &directory,
        &temporary_root,
        Some(&cache_root),
    )
    .map_err(|error| format!("云端版本清单读取失败，已停止安装：{error}"))?
    .ok_or_else(|| "云端版本清单不存在，请先修复清单或重新上传本体包".to_string())?;
    let package = GameBodyInstallService::package_entry(&manifest, &remote, remote_game_key)?;
    let expected_sha256 = package
        .package_sha256
        .as_deref()
        .ok_or_else(|| "云端本体包缺少 SHA-256".to_string())?;
    let local_uid = target.game_uid.clone();
    let games_root = state.games_root()?;
    let managed_path = target.managed_path.clone();
    let version_id = package.version_id.clone();
    let cache_root = state.body_packages_root()?;
    crate::services::disk_space::ensure_available_space(
        &cache_root,
        remote.size.saturating_add(package.total_bytes),
    )?;
    if TaskService::is_cancelled(&state, task_id) {
        return Err("任务已取消".to_string());
    }
    let transaction_id = Uuid::new_v4().to_string();
    let package_path =
        GameBodyInstallService::install_package_path(&cache_root, &local_uid, &transaction_id)?;
    let journal_path = games_root.join(format!(".{local_uid}.cloud-install.json"));
    let mut package_guard = CloudInstallPackageGuard {
        path: Some(package_path.clone()),
        journal_path: journal_path.clone(),
    };
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
            5 + ((u16::from(progress) * 74) / 100) as u8,
            message,
            None,
        );
        !TaskService::is_cancelled(&state, task_id)
    })?;
    if TaskService::is_cancelled(&state, task_id) {
        let _ = std::fs::remove_file(&package_path);
        return Err("任务已取消".to_string());
    }
    let staging = games_root.join(format!(
        "{}{local_uid}{}",
        crate::services::game_body_package_service::CLOUD_INSTALL_STAGING_PREFIX,
        crate::services::game_body_package_service::CLOUD_INSTALL_STAGING_SUFFIX
    ));
    if staging.exists() {
        return Err(
            "已有未完成的云端游戏安装暂存目录；若确认没有安装任务在进行，重启应用会自动清理它。"
                .to_string(),
        );
    }
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        82,
        "正在校验并安装游戏本体",
        None,
    );
    // 暂存目录在提交（下面的 `fs::rename`）之前一直由这个守卫负责收拾：解包失败、应用
    // 崩溃都会留下它，而留下的后果是该游戏**永久装不上**。提交成功时置 `None` 解除守卫，
    // 目录已变成受管目录、不能再删。
    let mut staging_guard = CloudInstallStagingGuard {
        path: Some(staging.clone()),
    };
    let body_manifest = BodyPackageService::extract_cloud_package(
        &package_path,
        &staging,
        &version_id,
        &catalog.executable_relative_path,
        expected_sha256,
        &downloaded_sha256,
        |progress, message| {
            TaskService::update(
                &state,
                task_id,
                TaskStatus::Running,
                82 + ((u16::from(progress) * 13) / 100) as u8,
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
    if body_manifest.file_count != package.file_count
        || body_manifest.total_bytes != package.total_bytes
    {
        return Err("本体包内容与云端版本清单不一致".to_string());
    }
    let profiles = state
        .store
        .lock()
        .map_err(|_| "读取存档保护范围失败".to_string())?
        .save_profiles
        .iter()
        .filter(|profile| profile.game_uid == local_uid)
        .cloned()
        .collect::<Vec<_>>();
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        96,
        "正在保留当前本地存档",
        None,
    );
    GameBodyInstallService::preserve_managed_saves(target, &profiles, &staging, || {
        TaskService::is_cancelled(&state, task_id)
    })?;
    // 游戏对象与封面在此组装：封面要走一次网络下载，放到 store 锁外完成，
    // 避免安装期间长时间占住锁、阻塞其他读写者。
    let mut game = GameBodyInstallService::installed_game(target, &catalog);
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
                match crate::services::game_cover_service::install_cloud_cover(
                    &library_root,
                    &local_uid,
                    &cover_bytes,
                ) {
                    Ok(cover) => game.cover = Some(cover),
                    Err(error) => {
                        crate::logging::error(format!("游戏安装继续，但云端封面未保存：{error}"))
                    }
                }
            }
        }
    }
    let body_version = GameBodyVersion {
        version_id: version_id.clone(),
        game_uid: local_uid.to_string(),
        created_at: package.created_at.clone(),
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
    };
    if TaskService::is_cancelled(&state, task_id) {
        return Err("任务已取消".to_string());
    }
    let mut previous_package = None;
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        99,
        "正在提交游戏安装",
        None,
    );
    let cleanup_pending = GameBodyInstallService::commit_install(
        &games_root,
        &cache_root,
        target,
        &transaction_id,
        &version_id,
        || {
            state.with_store_mut(|candidate| {
                if candidate
                    .games
                    .iter()
                    .any(|item| item.game_key == target.game_key && item.game_uid != local_uid)
                {
                    return Err("本地游戏关联已变化，已停止安装".to_string());
                }
                if let Some(existing_game) = candidate
                    .games
                    .iter_mut()
                    .find(|item| item.game_uid == local_uid)
                {
                    if target.existing.is_none()
                        || existing_game.game_key != target.game_key
                        || crate::domain::path_utils::normalize_path(Path::new(
                            &existing_game.managed_path,
                        )) != crate::domain::path_utils::normalize_path(&managed_path)
                    {
                        return Err("本地游戏记录已变化，已停止安装".to_string());
                    }
                    existing_game.lifecycle = game.lifecycle.clone();
                    existing_game.health = game.health.clone();
                    existing_game.cloud_status = game.cloud_status.clone();
                    existing_game.pending_body_upload = None;
                    existing_game.launch = game.launch.clone();
                    if existing_game.cover.is_none() {
                        existing_game.cover = game.cover.clone();
                    }
                } else {
                    if target.existing.is_some() {
                        return Err("本地游戏已被移除，已停止安装".to_string());
                    }
                    GameLibraryService::register_pending(candidate, game.clone())?;
                }
                previous_package = candidate
                    .body_versions
                    .iter()
                    .find(|version| {
                        version.game_uid == local_uid && version.version_id == version_id
                    })
                    .and_then(|version| version.package_path.clone());
                candidate.body_versions.retain(|version| {
                    !(version.game_uid == local_uid && version.version_id == version_id)
                });
                candidate.body_versions.push(body_version);
                GameRepository::persist(app, candidate)
                    .map_err(|error| format!("保存已安装游戏记录失败：{error}"))
            })
        },
    )?;
    staging_guard.path = None;
    package_guard.path = None;
    if let Some(previous) = previous_package {
        let previous = PathBuf::from(previous);
        let current = crate::domain::path_utils::normalize_path(&previous);
        let referenced = state
            .store
            .lock()
            .map(|store| {
                store.body_versions.iter().any(|version| {
                    version.package_path.as_ref().is_some_and(|path| {
                        crate::domain::path_utils::normalize_path(Path::new(path)) == current
                    })
                })
            })
            .unwrap_or(true);
        if !referenced
            && previous.parent().is_some_and(|parent| {
                crate::domain::path_utils::normalize_path(parent)
                    == crate::domain::path_utils::normalize_path(&cache_root.join(&local_uid))
            })
        {
            if let Err(error) = std::fs::remove_file(&previous) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    crate::logging::warn(format!("安装完成，旧 ZIP 缓存待清理：{error}"));
                }
            }
        }
    }
    Ok(serde_json::json!({
        "gameUid": local_uid,
        "versionId": version_id,
        "managedPath": managed_path,
        "fileCount": body_manifest.file_count,
        "totalBytes": body_manifest.total_bytes,
        "cleanupPending": cleanup_pending,
    }))
}

/// 提交前失败就删掉云端安装的暂存目录。
///
/// 设成 `Option` 而不是「bool + 路径」：`Some` 即「还归我管」，提交成功后置 `None` 就地
/// 解除，不需要另一个标志位去解释路径已经不该删了。
struct CloudInstallStagingGuard {
    path: Option<PathBuf>,
}

impl Drop for CloudInstallStagingGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let journal = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix('.'))
                .and_then(|name| name.strip_suffix(".cloud-installing"))
                .and_then(|uid| {
                    path.parent()
                        .map(|root| root.join(format!(".{uid}.cloud-install.json")))
                });
            if journal.is_some_and(|journal| journal.exists()) {
                return;
            }
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

struct CloudInstallPackageGuard {
    path: Option<PathBuf>,
    journal_path: PathBuf,
}

impl Drop for CloudInstallPackageGuard {
    fn drop(&mut self) {
        if !self.journal_path.exists() {
            if let Some(path) = &self.path {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

fn reconcile_local_body_versions(
    app: &AppHandle,
    state: &AppState,
    game_uid: &str,
    remote: &RemoteBodyPackageList,
) -> Result<(), String> {
    state.with_store_mut(|candidate| {
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
        GameRepository::persist(app, candidate)?;
        Ok(())
    })
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
    state.claim_operation(game_uid, "该游戏已有本体或存档操作正在进行")?;
    let running = state
        .running_games
        .lock()
        .map(|games| games.contains_key(game_uid))
        .map_err(|_| "lock running game state failed".to_string());
    match running {
        Ok(false) => Ok(()),
        other => {
            state.release_operation(&AppState::game_operation_key(game_uid));
            Err(other
                .err()
                .unwrap_or_else(|| "游戏运行中，暂时不能传输游戏本体".to_string()))
        }
    }
}

fn release_transfer(state: &AppState, game_uid: &str) {
    state.release_operation(&AppState::game_operation_key(game_uid));
}

pub(crate) fn load_baidu_client(app: &AppHandle) -> Result<BaiduNetdiskClient, String> {
    BaiduNetdiskClient::load_configured(&app_data_dir(app)?)
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
    // 判定收敛到领域层的 `is_safe_path_segment`：这里原先是一段内联的等价检查，
    // 与云账号清单校验里那份 `is_valid_game_key` 重复。
    BaiduNetdiskClient::remote_body_dir(game_key)
}

pub(crate) fn validate_remote_package_path(directory: &str, path: &str) -> Result<(), String> {
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

fn now_iso() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        is_safe_path_segment, remote_body_dir, reserve_transfer, validate_remote_package_path,
        CloudInstallPackageGuard, CloudInstallStagingGuard, Game,
    };

    #[test]
    fn upload_reminder_requires_store_manifest_to_contain_the_current_zip() {
        use super::{record_body_publication, AppStore, CloudBodyManifest, GameBodyVersion};
        use crate::services::cloud_manifest_service::CloudBodyManifestVersion;

        let mut game = Game::new_pending("Reminder game", "games/test", "game.exe");
        let uid = game.game_uid.clone();
        let game_key = game.game_key.clone();
        let old_entry = CloudBodyManifestVersion {
            version_id: "old".into(),
            created_at: "1".into(),
            package_path: "/games/test/body/old.zip".into(),
            package_fs_id: 10,
            package_size: 50,
            package_sha256: Some("a".repeat(64)),
            file_count: 1,
            total_bytes: 50,
        };
        let current_entry = CloudBodyManifestVersion {
            version_id: "current".into(),
            package_path: "/games/test/body/current.zip".into(),
            package_fs_id: 11,
            ..old_entry.clone()
        };
        let versions = [&old_entry, &current_entry].map(|entry| GameBodyVersion {
            version_id: entry.version_id.clone(),
            game_uid: uid.clone(),
            created_at: entry.created_at.clone(),
            archive_path: String::new(),
            file_count: entry.file_count,
            total_bytes: entry.total_bytes,
            package_path: None,
            sha256: entry.package_sha256.clone(),
            excluded_items: Vec::new(),
            upload_status: Some("manifest_pending".into()),
            remote_path: Some(entry.package_path.clone()),
            remote_fs_id: Some(entry.package_fs_id),
            remote_size: Some(entry.package_size),
        });
        game.mark_body_upload_pending();
        game.record_pending_body_package("current");
        let mut store = AppStore {
            games: vec![game],
            body_versions: versions.into(),
            ..Default::default()
        };
        let mut manifest = CloudBodyManifest {
            format_version: 1,
            game_key,
            game_uid: uid.clone(),
            updated_at: "2".into(),
            versions: vec![old_entry],
        };
        assert!(!record_body_publication(&mut store, &uid, &manifest));
        assert!(store.games[0].pending_body_upload.is_some());
        manifest.versions.push(CloudBodyManifestVersion {
            package_fs_id: 12,
            ..current_entry.clone()
        });
        assert!(!record_body_publication(&mut store, &uid, &manifest));
        manifest.versions[1] = current_entry;
        assert!(record_body_publication(&mut store, &uid, &manifest));
        assert!(store.games[0].pending_body_upload.is_none());
        assert_eq!(store.body_versions[1].upload_status.as_deref(), Some("synced"));
    }

    #[test]
    fn transfer_reservation_respects_cloud_save_operations() {
        let root = crate::test_support::TempWorkspace::new("cloud-install-operation-lock");
        let state = crate::app_state::AppState::new(
            crate::domain::AppStore::default(),
            root.to_path_buf(),
            std::collections::HashMap::new(),
            root.join("tasks.json"),
        );
        let key = state
            .claim_cloud_operation("local-game")
            .expect("cloud save lock");
        assert!(reserve_transfer(&state, "local-game").is_err());
        let operations = state.save_operations.lock().expect("operations");
        assert!(operations.contains(&key));
        assert!(!operations.contains("local-game"));
    }

    #[test]
    fn transfer_reservation_releases_its_claim_for_a_running_game() {
        let root = crate::test_support::TempWorkspace::new("cloud-install-running-lock");
        let state = crate::app_state::AppState::new(
            crate::domain::AppStore::default(),
            root.to_path_buf(),
            std::collections::HashMap::new(),
            root.join("tasks.json"),
        );
        state.running_games.lock().expect("running games").insert(
            "local-game".to_string(),
            crate::domain::GameRuntime {
                game_uid: "local-game".to_string(),
                status: crate::domain::GameRuntimeStatus::Running,
                pid: Some(1),
                started_at: None,
                task_id: None,
            },
        );
        assert!(reserve_transfer(&state, "local-game").is_err());
        assert!(state.save_operations.lock().expect("operations").is_empty());
    }

    #[test]
    fn unresolved_install_journals_preserve_owned_recovery_artifacts() {
        let root = crate::test_support::TempWorkspace::new("cloud-install-recovery-guards");
        let staging = root.join(".local-game.cloud-installing");
        let journal_path = root.join(".local-game.cloud-install.json");
        let package = root.join("downloaded.zip");
        std::fs::create_dir_all(&staging).expect("staging");
        std::fs::write(&journal_path, b"pending transaction").expect("journal");
        std::fs::write(&package, b"zip").expect("package");
        drop(CloudInstallStagingGuard {
            path: Some(staging.clone()),
        });
        drop(CloudInstallPackageGuard {
            path: Some(package.clone()),
            journal_path,
        });
        assert!(staging.is_dir());
        assert!(package.is_file());
    }

    #[test]
    fn failed_download_install_removes_its_uncommitted_zip() {
        let root = crate::test_support::TempWorkspace::new("cloud-install-zip-guard");
        let package = root.join("downloaded.zip");
        std::fs::write(&package, b"zip").expect("package");
        drop(CloudInstallPackageGuard {
            path: Some(package.clone()),
            journal_path: root.join("missing-journal.json"),
        });
        assert!(!package.exists());
    }

    /// 提交前失败：守卫必须把暂存目录删掉，否则该游戏会被那条「已存在就拒绝」的判据卡死。
    #[test]
    fn staging_guard_removes_the_directory_it_still_owns() {
        let dir = std::env::temp_dir().join(format!(
            "gamesaver-guard-armed-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).expect("create staging");
        std::fs::write(dir.join("game.exe"), b"half-extracted").expect("write partial file");

        drop(CloudInstallStagingGuard {
            path: Some(dir.clone()),
        });

        assert!(!dir.exists());
    }

    /// 提交成功后必须解除守卫。这条是这套逻辑最危险的失效方式：写错就会把**刚装好的
    /// 游戏本体**当成暂存目录删掉，而用户看到的是「安装成功但游戏不见了」。
    #[test]
    fn staging_guard_keeps_a_committed_directory() {
        let dir = std::env::temp_dir().join(format!(
            "gamesaver-guard-disarmed-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).expect("create committed directory");
        std::fs::write(dir.join("game.exe"), b"real").expect("write real game");
        let mut guard = CloudInstallStagingGuard {
            path: Some(dir.clone()),
        };

        // 模拟 `fs::rename` 提交成功后解除守卫。
        guard.path = None;
        drop(guard);

        assert!(dir.join("game.exe").is_file());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

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

    /// 钉住这轮修复的**根因**：`derive_game_key` 只做空白折叠与小写，**不处理路径分隔符**，
    /// 所以「由显示名现推一个 game_key」在云端场景下不可靠 —— 显示名是远端 JSON 里的任意
    /// 字符串，而推导结果会被拼进远程路径。
    #[test]
    fn a_display_name_is_not_a_safe_source_for_a_game_key() {
        for display_name in ["Pokemon Red/Blue", "..", ".", r"a\b", "   ", ""] {
            let derived = Game::derive_game_key(display_name);
            assert!(
                !is_safe_path_segment(&derived),
                "{display_name:?} 推出的 {derived:?} 不该被当成可用的 game_key"
            );
        }
        // 反向守卫：正常显示名推出来的键必须仍然可用（含空格、非 ASCII），别把校验做过头。
        assert!(is_safe_path_segment(&Game::derive_game_key(
            "Black Market v1.23"
        )));
        assert!(is_safe_path_segment(&Game::derive_game_key(
            "トビ姫 - inter breed tobihime_cn"
        )));
    }
}
