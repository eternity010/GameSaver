use crate::{
    app_state::AppState,
    domain::{CoverCrop, CoverPosition, GameCover},
    repositories::GameRepository,
    services::GameLibraryService,
};
use std::{fs, io::Write, path::{Component, Path, PathBuf}};
use tauri::{AppHandle, State};
use uuid::Uuid;

#[tauri::command]
pub fn list_games(state: State<AppState>) -> Result<Vec<crate::domain::Game>, String> {
    let store = state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())?;
    Ok(GameLibraryService::list(&store))
}

#[tauri::command]
pub fn get_game(state: State<AppState>, game_uid: String) -> Result<Option<crate::domain::Game>, String> {
    let store = state.store.lock().map_err(|_| "lock GameSaver store failed".to_string())?;
    Ok(GameLibraryService::find(&store, game_uid.trim()))
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
    validate_cover_input(&original_bytes, &display_bytes, &original_extension, &crop, &position)?;
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
pub fn get_game_cover(state: State<AppState>, game_uid: String) -> Result<Option<Vec<u8>>, String> {
    let game_uid = game_uid.trim();
    validate_component(game_uid, "游戏标识")?;
    let cover = {
        let store = state.store.lock().map_err(|_| "读取游戏封面记录失败".to_string())?;
        GameLibraryService::find(&store, game_uid).and_then(|game| game.cover)
    };
    let Some(cover) = cover else { return Ok(None); };
    let root = state.library_root_path()?;
    let path = safe_cover_path(&root, game_uid, &cover.display_path)?;
    if !path.is_file() {
        return Ok(None);
    }
    fs::read(path).map(Some).map_err(|error| format!("读取游戏封面失败：{error}"))
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
        let store = state.store.lock().map_err(|_| "读取游戏封面记录失败".to_string())?;
        let game = GameLibraryService::find(&store, game_uid).ok_or_else(|| "游戏不存在".to_string())?;
        game.cover
    };
    let root = state.library_root_path()?;
    let game_covers_root = root.join("covers").join(game_uid);
    fs::create_dir_all(&game_covers_root).map_err(|error| format!("创建游戏封面目录失败：{error}"))?;
    let cover_id = Uuid::new_v4().simple().to_string();
    let staging = game_covers_root.join(format!(".staging-{cover_id}"));
    let final_dir = game_covers_root.join(&cover_id);
    let result = (|| -> Result<GameCover, String> {
        fs::create_dir_all(&staging).map_err(|error| format!("创建封面暂存目录失败：{error}"))?;
        let extension = normalize_extension(original_extension)?;
        write_synced_file(&staging.join(format!("original.{extension}")), original_bytes)?;
        write_synced_file(&staging.join("display.jpg"), display_bytes)?;
        fs::rename(&staging, &final_dir).map_err(|error| format!("提交游戏封面文件失败：{error}"))?;
        let cover = GameCover {
            original_path: relative_cover_path(game_uid, &cover_id, &format!("original.{extension}")),
            display_path: relative_cover_path(game_uid, &cover_id, "display.jpg"),
            crop,
            position,
        };
        let mut candidate = {
            let store = state.store.lock().map_err(|_| "读取游戏记录失败".to_string())?;
            store.clone()
        };
        let game = candidate.games.iter_mut().find(|game| game.game_uid == game_uid).ok_or_else(|| "游戏不存在".to_string())?;
        game.cover = Some(cover.clone());
        if let Err(error) = GameRepository::persist(app, &candidate) {
            let _ = fs::remove_dir_all(&final_dir);
            return Err(format!("保存游戏封面记录失败：{error}"));
        }
        *state.store.lock().map_err(|_| "更新游戏封面记录失败".to_string())? = candidate;
        cleanup_old_cover(&root, game_uid, old_cover.as_ref(), &cover);
        Ok(cover)
    })();
    let _ = fs::remove_dir_all(&staging);
    result
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
    if crop.aspect_width != 16 || crop.aspect_height != 9 || crop.output_width != 1280 || crop.output_height != 720 {
        return Err("封面裁剪比例或输出尺寸无效".to_string());
    }
    if !(1000..=3000).contains(&position.zoom_milli) || position.offset_x_milli.abs() > 2_000_000 || position.offset_y_milli.abs() > 2_000_000 {
        return Err("封面裁剪位置无效".to_string());
    }
    Ok(())
}

fn normalize_extension(value: &str) -> Result<String, String> {
    match value.trim().trim_start_matches('.').to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => Ok("jpg".to_string()),
        "png" => Ok("png".to_string()),
        "webp" => Ok("webp".to_string()),
        _ => Err("只支持 JPG、PNG 或 WebP 封面".to_string()),
    }
}

fn validate_component(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty() || Path::new(value).components().any(|component| !matches!(component, Component::Normal(_))) {
        return Err(format!("{label}无效"));
    }
    Ok(())
}

fn relative_cover_path(game_uid: &str, cover_id: &str, file_name: &str) -> String {
    format!("covers/{game_uid}/{cover_id}/{file_name}")
}

fn safe_cover_path(root: &Path, game_uid: &str, value: &str) -> Result<PathBuf, String> {
    let relative = Path::new(value);
    if relative.is_absolute() || relative.components().any(|component| matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_))) {
        return Err("游戏封面路径无效".to_string());
    }
    let prefix = PathBuf::from("covers").join(game_uid);
    if relative.strip_prefix(&prefix).is_err() {
        return Err("游戏封面路径超出受管目录".to_string());
    }
    Ok(root.join(relative))
}

fn write_synced_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temporary = path.with_file_name(format!(".{}.tmp-{}", path.file_name().unwrap_or_default().to_string_lossy(), Uuid::new_v4().simple()));
    let result = (|| -> Result<(), String> {
        let mut file = fs::File::create(&temporary).map_err(|error| format!("创建封面临时文件失败：{error}"))?;
        file.write_all(bytes).map_err(|error| format!("写入封面文件失败：{error}"))?;
        file.sync_all().map_err(|error| format!("刷新封面文件失败：{error}"))?;
        fs::rename(&temporary, path).map_err(|error| format!("提交封面文件失败：{error}"))
    })();
    let _ = fs::remove_file(&temporary);
    result
}

fn cleanup_old_cover(root: &Path, game_uid: &str, old_cover: Option<&GameCover>, new_cover: &GameCover) {
    let Some(old_cover) = old_cover else { return; };
    if old_cover.display_path == new_cover.display_path { return; }
    let Ok(old_path) = safe_cover_path(root, game_uid, &old_cover.display_path) else { return; };
    let Some(old_dir) = old_path.parent() else { return; };
    let _ = fs::remove_dir_all(old_dir);
}

fn reserve_cover_operation(state: &AppState, game_uid: &str) -> Result<(), String> {
    if state.running_games.lock().map_err(|_| "读取运行状态失败".to_string())?.contains_key(game_uid) {
        return Err("游戏运行中，暂时不能修改封面".to_string());
    }
    let mut operations = state.save_operations.lock().map_err(|_| "锁定游戏操作状态失败".to_string())?;
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
