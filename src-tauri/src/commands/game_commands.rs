use crate::{
    app_state::AppState,
    domain::{CoverCrop, CoverPosition, GameCover},
    repositories::GameRepository,
    services::{CoverCaptureService, GameBodyUpdateService, GameLibraryService},
};
use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};
use tauri::{AppHandle, Manager, State};
use uuid::Uuid;

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

    let mut candidate = state
        .store
        .lock()
        .map_err(|_| "读取游戏记录失败".to_string())?
        .clone();
    let game = candidate
        .games
        .iter_mut()
        .find(|game| game.game_uid == game_uid)
        .ok_or_else(|| "游戏不存在".to_string())?;

    game.display_name = new_name.to_string();
    let updated_game = game.clone();

    GameRepository::persist(&app, &candidate)?;
    *state
        .store
        .lock()
        .map_err(|_| "更新游戏记录失败".to_string())? = candidate;

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

const MAX_ORIGINAL_COVER_BYTES: usize = 32 * 1024 * 1024;
const MAX_DISPLAY_COVER_BYTES: usize = 8 * 1024 * 1024;

#[tauri::command]
pub fn save_game_cover(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
    original_bytes: Vec<u8>,
    display_bytes: Vec<u8>,
    original_extension: String,
    crop: CoverCrop,
    position: CoverPosition,
) -> Result<GameCover, String> {
    let game_uid = game_uid.trim().to_string();
    validate_component(&game_uid, "游戏标识")?;
    validate_cover_input(
        &original_bytes,
        &display_bytes,
        &original_extension,
        &crop,
        &position,
    )?;
    reserve_cover_operation(&state, &game_uid)?;
    let result = save_game_cover_files(
        &app,
        &state,
        &game_uid,
        &original_bytes,
        &display_bytes,
        &original_extension,
        crop,
        position,
    );
    release_cover_operation(&state, &game_uid);
    result
}

#[tauri::command]
pub fn arm_game_cover_capture(
    state: State<AppState>,
    game_uid: String,
) -> Result<crate::services::CaptureArmView, String> {
    let game_uid = game_uid.trim().to_string();
    validate_component(&game_uid, "游戏标识")?;
    let managed_path = {
        let store = state
            .store
            .lock()
            .map_err(|_| "读取游戏记录失败".to_string())?;
        GameLibraryService::find(&store, &game_uid)
            .ok_or_else(|| "游戏不存在".to_string())?
            .managed_path
    };
    if !Path::new(&managed_path).is_dir() {
        return Err("受管游戏目录不存在，无法截取封面".to_string());
    }
    if !state
        .running_games
        .lock()
        .map_err(|_| "读取游戏运行状态失败".to_string())?
        .contains_key(&game_uid)
    {
        return Err("请先启动游戏后再截取封面".to_string());
    }

    crate::logging::info(format!("确认进入封面截图模式：game_uid={game_uid}"));

    let capture = CoverCaptureService::arm(&game_uid, PathBuf::from(managed_path))?;
    crate::logging::info(format!(
        "封面截图会话已准备，等待前端最小化窗口：capture_id={}",
        capture.capture_id
    ));
    Ok(capture)
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

fn save_game_cover_files(
    app: &AppHandle,
    state: &AppState,
    game_uid: &str,
    original_bytes: &[u8],
    display_bytes: &[u8],
    original_extension: &str,
    crop: CoverCrop,
    position: CoverPosition,
) -> Result<GameCover, String> {
    let old_cover = {
        let store = state
            .store
            .lock()
            .map_err(|_| "读取游戏封面记录失败".to_string())?;
        let game =
            GameLibraryService::find(&store, game_uid).ok_or_else(|| "游戏不存在".to_string())?;
        game.cover
    };
    let root = state.library_root_path()?;
    let game_covers_root = root.join("covers").join(game_uid);
    fs::create_dir_all(&game_covers_root)
        .map_err(|error| format!("创建游戏封面目录失败：{error}"))?;
    let cover_id = Uuid::new_v4().simple().to_string();
    let staging = game_covers_root.join(format!(".staging-{cover_id}"));
    let final_dir = game_covers_root.join(&cover_id);
    let result = (|| -> Result<GameCover, String> {
        fs::create_dir_all(&staging).map_err(|error| format!("创建封面暂存目录失败：{error}"))?;
        let extension = normalize_extension(original_extension)?;
        write_synced_file(
            &staging.join(format!("original.{extension}")),
            original_bytes,
        )?;
        write_synced_file(&staging.join("display.jpg"), display_bytes)?;
        fs::rename(&staging, &final_dir)
            .map_err(|error| format!("提交游戏封面文件失败：{error}"))?;
        let cover = GameCover {
            original_path: relative_cover_path(
                game_uid,
                &cover_id,
                &format!("original.{extension}"),
            ),
            display_path: relative_cover_path(game_uid, &cover_id, "display.jpg"),
            crop,
            position,
        };
        let mut candidate = {
            let store = state
                .store
                .lock()
                .map_err(|_| "读取游戏记录失败".to_string())?;
            store.clone()
        };
        let game = candidate
            .games
            .iter_mut()
            .find(|game| game.game_uid == game_uid)
            .ok_or_else(|| "游戏不存在".to_string())?;
        game.cover = Some(cover.clone());
        let game_key = game.game_key.clone();
        if let Err(error) = GameRepository::persist(app, &candidate) {
            let _ = fs::remove_dir_all(&final_dir);
            return Err(format!("保存游戏封面记录失败：{error}"));
        }
        *state
            .store
            .lock()
            .map_err(|_| "更新游戏封面记录失败".to_string())? = candidate;
        cleanup_old_cover(&root, game_uid, old_cover.as_ref(), &cover);
        maybe_sync_cover_to_cloud_async(app.clone(), game_key, display_bytes.to_vec());
        Ok(cover)
    })();
    let _ = fs::remove_dir_all(&staging);
    result
}

fn maybe_sync_cover_to_cloud_async(app: AppHandle, game_key: String, display_bytes: Vec<u8>) {
    if game_key.trim().is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let Ok(client) = crate::commands::baidu_commands::load_baidu_client(&app) else {
            return;
        };
        let Ok(remote_dir) = crate::commands::baidu_commands::remote_body_dir(&game_key) else {
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
        let _ = crate::services::CloudManifestService::write_cover(
            &client,
            &remote_dir,
            &display_bytes,
            &temporary_root,
            Some(&cache_root),
        );
    });
}

fn validate_cover_input(
    original_bytes: &[u8],
    display_bytes: &[u8],
    original_extension: &str,
    crop: &CoverCrop,
    position: &CoverPosition,
) -> Result<(), String> {
    if original_bytes.is_empty() || original_bytes.len() > MAX_ORIGINAL_COVER_BYTES {
        return Err("原始封面不能为空且不能超过 32 MB".to_string());
    }
    if display_bytes.is_empty() || display_bytes.len() > MAX_DISPLAY_COVER_BYTES {
        return Err("展示封面不能为空且不能超过 8 MB".to_string());
    }
    let extension = normalize_extension(original_extension)?;
    let original_valid = match extension.as_str() {
        "jpg" => original_bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "png" => original_bytes.starts_with(&[0x89, b'P', b'N', b'G']),
        "webp" => original_bytes.starts_with(b"RIFF") && original_bytes.get(8..12) == Some(b"WEBP"),
        _ => false,
    };
    if !original_valid {
        return Err("原始封面格式与文件扩展名不匹配".to_string());
    }
    if !display_bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Err("展示封面必须是 JPEG 图片".to_string());
    }
    if crop.aspect_width != 16
        || crop.aspect_height != 9
        || crop.output_width != 1280
        || crop.output_height != 720
    {
        return Err("封面裁剪比例或输出尺寸无效".to_string());
    }
    if !(1000..=3000).contains(&position.zoom_milli)
        || position.offset_x_milli.abs() > 2_000_000
        || position.offset_y_milli.abs() > 2_000_000
    {
        return Err("封面裁剪位置无效".to_string());
    }
    Ok(())
}

fn normalize_extension(value: &str) -> Result<String, String> {
    match value
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => Ok("jpg".to_string()),
        "png" => Ok("png".to_string()),
        "webp" => Ok("webp".to_string()),
        _ => Err("只支持 JPG、PNG 或 WebP 封面".to_string()),
    }
}

fn validate_component(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || Path::new(value)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("{label}无效"));
    }
    Ok(())
}

fn relative_cover_path(game_uid: &str, cover_id: &str, file_name: &str) -> String {
    format!("covers/{game_uid}/{cover_id}/{file_name}")
}

fn safe_cover_path(root: &Path, game_uid: &str, value: &str) -> Result<PathBuf, String> {
    let relative = Path::new(value);
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err("游戏封面路径无效".to_string());
    }
    let prefix = PathBuf::from("covers").join(game_uid);
    if relative.strip_prefix(&prefix).is_err() {
        return Err("游戏封面路径超出受管目录".to_string());
    }
    Ok(root.join(relative))
}

fn write_synced_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temporary = path.with_file_name(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        Uuid::new_v4().simple()
    ));
    let result = (|| -> Result<(), String> {
        let mut file = fs::File::create(&temporary)
            .map_err(|error| format!("创建封面临时文件失败：{error}"))?;
        file.write_all(bytes)
            .map_err(|error| format!("写入封面文件失败：{error}"))?;
        file.sync_all()
            .map_err(|error| format!("刷新封面文件失败：{error}"))?;
        fs::rename(&temporary, path).map_err(|error| format!("提交封面文件失败：{error}"))
    })();
    let _ = fs::remove_file(&temporary);
    result
}

fn cleanup_old_cover(
    root: &Path,
    game_uid: &str,
    old_cover: Option<&GameCover>,
    new_cover: &GameCover,
) {
    let Some(old_cover) = old_cover else {
        return;
    };
    if old_cover.display_path == new_cover.display_path {
        return;
    }
    let Ok(old_path) = safe_cover_path(root, game_uid, &old_cover.display_path) else {
        return;
    };
    let Some(old_dir) = old_path.parent() else {
        return;
    };
    let _ = fs::remove_dir_all(old_dir);
}

fn reserve_cover_operation(state: &AppState, game_uid: &str) -> Result<(), String> {
    let mut operations = state
        .save_operations
        .lock()
        .map_err(|_| "锁定游戏操作状态失败".to_string())?;
    if !operations.insert(game_uid.to_string()) {
        return Err("该游戏已有其他操作正在进行".to_string());
    }
    Ok(())
}

fn release_cover_operation(state: &AppState, game_uid: &str) {
    if let Ok(mut operations) = state.save_operations.lock() {
        operations.remove(game_uid);
    }
}

#[tauri::command]
pub fn remove_game_from_library(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
) -> Result<(), String> {
    let game_uid = game_uid.trim().to_string();
    if game_uid.is_empty() {
        return Err("游戏 ID 不能为空".to_string());
    }

    if let Ok(running) = state.running_games.lock() {
        if running.contains_key(&game_uid) {
            return Err("游戏正在运行中，无法从库中删除".to_string());
        }
    }

    reserve_cover_operation(&state, &game_uid)?;
    struct OperationGuard<'a> {
        state: &'a AppState,
        game_uid: String,
    }
    impl<'a> Drop for OperationGuard<'a> {
        fn drop(&mut self) {
            release_cover_operation(self.state, &self.game_uid);
        }
    }
    let _guard = OperationGuard {
        state: &state,
        game_uid: game_uid.clone(),
    };

    let (game, games_root_opt, app_data_dir_opt) = {
        let store = state
            .store
            .lock()
            .map_err(|_| "锁定游戏库数据失败".to_string())?;
        let game = store
            .games
            .iter()
            .find(|g| g.game_uid == game_uid)
            .cloned()
            .ok_or_else(|| "游戏不存在".to_string())?;
        (
            game,
            state.games_root().ok(),
            app.path().app_data_dir().ok(),
        )
    };

    // 1. Clean up managed directory if it exists
    let managed_path = PathBuf::from(&game.managed_path);
    if managed_path.exists() {
        let _ = fs::remove_dir_all(&managed_path);
    }

    // 2. Clean up versions, covers, update journal, staging
    if let Some(ref games_root) = games_root_opt {
        let versions_dir = games_root.join(".versions").join(&game_uid);
        if versions_dir.exists() {
            let _ = fs::remove_dir_all(&versions_dir);
        }
        let covers_dir = games_root.join("covers").join(&game_uid);
        if covers_dir.exists() {
            let _ = fs::remove_dir_all(&covers_dir);
        }
        let staging = games_root.join(format!(".{game_uid}.updating"));
        if staging.exists() {
            let _ = fs::remove_dir_all(&staging);
        }
        let journal = GameBodyUpdateService::journal_path(games_root, &game_uid);
        let _ = GameBodyUpdateService::clear_journal(&journal);
    }

    // 3. Clean up cached body packages if any
    if let Some(ref app_data_dir) = app_data_dir_opt {
        let package_cache = app_data_dir.join("cache").join("body_packages").join(&game_uid);
        if package_cache.exists() {
            let _ = fs::remove_dir_all(&package_cache);
        }
    }

    // 4. Update store and persist
    let mut store = state
        .store
        .lock()
        .map_err(|_| "锁定游戏库数据失败".to_string())?;
    let mut candidate = store.clone();
    candidate.games.retain(|item| item.game_uid != game_uid);
    candidate.save_profiles.retain(|item| item.game_uid != game_uid);
    candidate.save_versions.retain(|item| item.game_uid != game_uid);
    candidate.body_versions.retain(|item| item.game_uid != game_uid);

    GameRepository::persist(&app, &candidate)?;
    *store = candidate;

    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameDetailView {
    pub precheck: crate::services::LaunchPrecheck,
    pub versions: Vec<crate::domain::SaveVersion>,
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

    let mut versions = store
        .save_versions
        .iter()
        .filter(|version| version.game_uid == game_uid)
        .cloned()
        .collect::<Vec<_>>();
    versions.sort_by(|left, right| right.created_at.cmp(&left.created_at));

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
    body_versions.sort_by(|left, right| right.created_at.cmp(&left.created_at));

    let body_version_views = body_versions
        .into_iter()
        .map(|version| crate::commands::game_body_commands::GameBodyVersionView {
            package_size: version
                .package_path
                .as_deref()
                .and_then(|path| std::fs::metadata(path).ok())
                .map(|metadata| metadata.len()),
            version,
        })
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

    #[test]
    fn query_game_detail_view_returns_aggregated_details() {
        let mut store = AppStore::default();
        let running = HashMap::new();
        let game = Game {
            game_uid: "test-uid-1".to_string(),
            game_key: "test-key".to_string(),
            display_name: "Test Game".to_string(),
            managed_path: "C:\\Games\\Test".to_string(),
            lifecycle: GameLifecycle::Active,
            health: GameHealth::Ready,
            cloud_status: CloudStatus::Disabled,
            save_profile_id: None,
            launch: LaunchConfig {
                executable_relative_path: "test.exe".to_string(),
                arguments: vec![],
                working_directory_relative_path: None,
            },
            cover: None,
            last_played_at: None,
            latest_save_version_id: None,
        };
        store.games.push(game);

        let detail = query_game_detail_view(&store, &running, "test-uid-1").unwrap();
        assert_eq!(detail.precheck.game_uid, "test-uid-1");
        assert_eq!(detail.versions.len(), 0);
        assert_eq!(detail.body_versions.len(), 0);
        assert!(detail.runtime.is_none());
        assert!(detail.save_profile.is_none());
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
