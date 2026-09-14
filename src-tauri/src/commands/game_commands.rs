use crate::{
    app_state::AppState,
    domain::{compare_created_at, CoverCrop, CoverPosition, GameCover, TaskCategory, TaskStatus},
    repositories::GameRepository,
    services::{CoverCaptureService, GameBodyUpdateService, GameLibraryService, TaskService},
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

    let updated_game = state.with_store_mut(|candidate| {
        let game = candidate
            .games
            .iter_mut()
            .find(|game| game.game_uid == game_uid)
            .ok_or_else(|| "游戏不存在".to_string())?;

        game.display_name = new_name.to_string();
        let updated_game = game.clone();

        GameRepository::persist(&app, candidate)?;
        Ok(updated_game)
    })?;

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
        let game_key = state.with_store_mut(|candidate| {
            let game = candidate
                .games
                .iter_mut()
                .find(|game| game.game_uid == game_uid)
                .ok_or_else(|| "游戏不存在".to_string())?;
            game.cover = Some(cover.clone());
            let game_key = game.game_key.clone();
            if let Err(error) = GameRepository::persist(app, candidate) {
                let _ = fs::remove_dir_all(&final_dir);
                return Err(format!("保存游戏封面记录失败：{error}"));
            }
            Ok(game_key)
        })?;
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

/// 把 `covers/<game_uid>/...` 形式的相对路径解析到库根之下。
///
/// 三重约束缺一不可：拒绝绝对路径、拒绝任何上级/根/前缀分量（`..` 会让
/// `root.join` 直接跨出库目录），并要求结果确实位于**该游戏自己的**封面目录下。
/// 协议层读取封面时同样要走这里，否则同一个不变量会出现两种强度。
pub(crate) fn safe_cover_path(root: &Path, game_uid: &str, value: &str) -> Result<PathBuf, String> {
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
    state.claim_operation(game_uid, "该游戏已有其他操作正在进行")
}

fn release_cover_operation(state: &AppState, game_uid: &str) {
    state.release_operation(&AppState::game_operation_key(game_uid));
}

/// 一项待删除的目录。`label` 只用于任务消息与失败项描述。
struct RemovalTarget {
    label: &'static str,
    path: PathBuf,
}

/// 删除一个目录或单个文件；**不存在视为成功**（本来就没有），其余错误按
/// `标签（路径）：原因` 收集起来交给调用方上报。
///
/// 返回 `None` 表示已删掉或本来就没有。
fn remove_if_present(label: &str, path: &Path, is_dir: bool) -> Option<String> {
    let result = if is_dir {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match result {
        Ok(()) => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => Some(format!("{label}（{}）：{error}", path.display())),
    }
}

/// 进度按「已完成步数 / 总步数」摊到 5~90，把 100 留给任务收尾。
fn progress_after(done: usize, total: usize) -> u8 {
    if total == 0 {
        return 90;
    }
    (5 + (done * 85) / total).min(90) as u8
}

/// 依次删除清单里的目录（外加可选的更新日志文件），返回**未能删除**的条目。
///
/// 每步通过 `on_step` 上报进度，让「正在删除游戏文件」这件事在任务列表里可见，
/// 而不是一个没有任何反馈的长阻塞。
fn remove_game_files(
    targets: &[RemovalTarget],
    journal: Option<&Path>,
    mut on_step: impl FnMut(u8, &str),
) -> Vec<String> {
    let total = targets.len() + usize::from(journal.is_some());
    let mut failures = Vec::new();
    let mut done = 0usize;
    for target in targets {
        on_step(
            progress_after(done, total),
            &format!("正在删除{}", target.label),
        );
        if let Some(failure) = remove_if_present(target.label, &target.path, true) {
            failures.push(failure);
        }
        done += 1;
    }
    if let Some(path) = journal {
        on_step(progress_after(done, total), "正在清理更新日志");
        if let Some(failure) = remove_if_present("更新日志", path, false) {
            failures.push(failure);
        }
    }
    failures
}

/// 算出要删的路径清单。**必须在游戏被摘除之前调用** —— 摘除之后 store 里就没有
/// `managed_path` 可读了。
fn collect_removal_targets(
    app: &AppHandle,
    state: &AppState,
    game_uid: &str,
) -> Result<(String, Vec<RemovalTarget>, Option<PathBuf>), String> {
    let (display_name, managed_path) = {
        let store = state
            .store
            .lock()
            .map_err(|_| "锁定游戏库数据失败".to_string())?;
        let game = store
            .games
            .iter()
            .find(|g| g.game_uid == game_uid)
            .ok_or_else(|| "游戏不存在".to_string())?;
        (game.display_name.clone(), PathBuf::from(&game.managed_path))
    };

    let mut targets = vec![RemovalTarget {
        label: "托管游戏目录",
        path: managed_path,
    }];
    let mut journal = None;
    if let Ok(games_root) = state.games_root() {
        targets.push(RemovalTarget {
            label: "存档版本目录",
            path: games_root.join(".versions").join(game_uid),
        });
        targets.push(RemovalTarget {
            label: "封面目录",
            path: games_root.join("covers").join(game_uid),
        });
        targets.push(RemovalTarget {
            label: "更新暂存目录",
            path: games_root.join(format!(".{game_uid}.updating")),
        });
        journal = Some(GameBodyUpdateService::journal_path(&games_root, game_uid));
    }
    if let Ok(app_data_dir) = app.path().app_data_dir() {
        targets.push(RemovalTarget {
            label: "本体包缓存",
            path: app_data_dir
                .join("cache")
                .join("body_packages")
                .join(game_uid),
        });
    }
    Ok((display_name, targets, journal))
}

/// 把游戏及其存档配置、版本、本体版本记录一起从 store 摘除并落盘。
///
/// 走 [`AppState::with_store_mut`] 的「读 → 改 → 持久化 → 提交」单次加锁契约，
/// 而不是先 `lock().clone()` 取快照、再 `lock()` 整体覆盖写回。
fn detach_game_from_store(app: &AppHandle, state: &AppState, game_uid: &str) -> Result<(), String> {
    state.with_store_mut(|candidate| {
        candidate.games.retain(|item| item.game_uid != game_uid);
        candidate
            .save_profiles
            .retain(|item| item.game_uid != game_uid);
        candidate
            .save_versions
            .retain(|item| item.game_uid != game_uid);
        candidate
            .body_versions
            .retain(|item| item.game_uid != game_uid);
        GameRepository::persist(app, candidate)
    })
}

/// 从库中移除游戏。
///
/// 分两段做，顺序不可颠倒：
///
/// 1. **同步段**（本函数体）：校验 → 认领整游戏独占 → 建任务 → 从 store 摘除并落盘。
///    走出这个函数时游戏已经不在库里，前端 `await` 之后刷新就能看到结果 —— 这也是它
///    保持同步命令的原因：同步命令的执行线程就是 Tauri 主线程（见 tauri-macros 的
///    `body_blocking`），所以这一段必须只剩毫秒级工作。
/// 2. **后台段**（下面的 `thread::spawn`）：删本体、存档版本、封面、更新暂存、本体包缓存。
///    托管本体可以非常大 —— 实测本机单个游戏 7.3 GB / 20,717 个文件、全库 35.52 GB /
///    29,485 个文件 —— 放在命令体里会让窗口消息循环与后续所有 IPC 一起停摆。
///
/// 删除失败**不再被 `let _ =` 吞掉**：失败项写进任务结果，任务消息里说明「有几处未能
/// 删除」。否则游戏已从库里消失、空间却还占着，没有任何地方会告诉用户。
/// 审计依据：`docs/command-blocking-audit-2026-09-14.md` 的 F1。
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

    // 认领「整游戏独占」：本体更新、封面处理、存档维护撞的是同一个 key。
    // 删除要持续到后台线程结束，所以这里不用 Drop 守卫，改由线程的两条出口显式释放。
    reserve_cover_operation(&state, &game_uid)?;

    let (display_name, targets, journal) = match collect_removal_targets(&app, &state, &game_uid) {
        Ok(value) => value,
        Err(error) => {
            release_cover_operation(&state, &game_uid);
            return Err(error);
        }
    };

    let task_id = match TaskService::create(
        &state,
        "remove_game_from_library",
        TaskCategory::Maintenance,
        Some(game_uid.clone()),
        &format!("正在从库中移除【{display_name}】"),
    ) {
        Ok(task_id) => task_id,
        Err(error) => {
            release_cover_operation(&state, &game_uid);
            return Err(error);
        }
    };

    // 先从 store 摘除：库里立刻看不到这个游戏，不必等文件删完。
    // 反过来的顺序（先删文件、后摘除）会在崩溃时留下「库里还在、本体已删一半」的游戏。
    if let Err(error) = detach_game_from_store(&app, &state, &game_uid) {
        TaskService::finish(
            &state,
            &task_id,
            TaskStatus::Failed,
            100,
            "从库中移除游戏失败",
            None,
            Some(error.clone()),
        );
        release_cover_operation(&state, &game_uid);
        return Err(error);
    }

    let app_handle = app.clone();
    let task_id_for_thread = task_id.clone();
    let game_uid_for_thread = game_uid.clone();
    std::thread::spawn(move || {
        let state = app_handle.state::<AppState>();
        TaskService::update(
            &state,
            &task_id_for_thread,
            TaskStatus::Running,
            5,
            "正在删除游戏文件",
            None,
        );
        let failures = remove_game_files(&targets, journal.as_deref(), |progress, label| {
            TaskService::update(
                &state,
                &task_id_for_thread,
                TaskStatus::Running,
                progress,
                label,
                None,
            );
        });
        release_cover_operation(&state, &game_uid_for_thread);
        let total = targets.len() + usize::from(journal.is_some());
        let removed = total - failures.len();
        if failures.is_empty() {
            TaskService::finish(
                &state,
                &task_id_for_thread,
                TaskStatus::Success,
                100,
                format!("已从库中移除【{display_name}】"),
                Some(serde_json::json!({ "removedPaths": removed })),
                None,
            );
        } else {
            // 主操作（从库里移除）已经成功，所以状态仍记 Success —— 与既有的
            // `delete_save_version`「保存版本已删除，但对象回收未完成」保持一致。
            // 失败点不能静默：任务卡片始终显示 `message`，所以把**是哪个目录**没删掉写进去；
            // 完整路径与原因进 result。Success 任务不渲染 `error` 那一行（TransferCenter
            // 只在 failed/interrupted 时显示），因此这里不传 error。
            let failed_labels = failures
                .iter()
                .map(|item| {
                    item.split_once('（')
                        .map(|(label, _)| label)
                        .unwrap_or(item.as_str())
                })
                .collect::<Vec<_>>()
                .join("、");
            TaskService::finish(
                &state,
                &task_id_for_thread,
                TaskStatus::Success,
                100,
                format!(
                    "已从库中移除【{display_name}】，但{failed_labels}未能删除，可能仍占用磁盘空间"
                ),
                Some(serde_json::json!({ "removedPaths": removed, "failedPaths": failures })),
                None,
            );
        }
    });

    Ok(())
}

/// 详情页版本列表的传输摘要。
///
/// 详情页只展示「几个文件 / 多大 / 什么时候」，从不读取单条文件记录；而
/// [`crate::domain::SaveVersion`] 的 `files` 承载的是每个版本的全部文件清单
/// （相对路径 + 对象哈希 + 大小），体积随「版本数 × 文件数」线性膨胀。把这些
/// 清单原样序列化给前端，会让 WebView 主线程付出与页面展示无关的 JSON 解析成本，
/// 因此这里只传计数。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveVersionSummary {
    pub version_id: String,
    pub created_at: String,
    pub total_bytes: u64,
    pub file_count: usize,
}

impl From<&crate::domain::SaveVersion> for SaveVersionSummary {
    fn from(version: &crate::domain::SaveVersion) -> Self {
        Self {
            version_id: version.version_id.clone(),
            created_at: version.created_at.clone(),
            total_bytes: version.total_bytes,
            file_count: version.files.len(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameDetailView {
    pub precheck: crate::services::LaunchPrecheck,
    pub versions: Vec<SaveVersionSummary>,
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
        .map(SaveVersionSummary::from)
        .collect::<Vec<_>>();
    versions.sort_by(|left, right| compare_created_at(&right.created_at, &left.created_at));

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
    body_versions.sort_by(|left, right| compare_created_at(&right.created_at, &left.created_at));

    let body_version_views = body_versions
        .into_iter()
        .map(
            |version| crate::commands::game_body_commands::GameBodyVersionView {
                package_size: version
                    .package_path
                    .as_deref()
                    .and_then(|path| std::fs::metadata(path).ok())
                    .map(|metadata| metadata.len()),
                version,
            },
        )
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

    fn detail_view_game() -> Game {
        Game {
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
            added_at: None,
        }
    }

    #[test]
    fn query_game_detail_view_returns_aggregated_details() {
        let mut store = AppStore::default();
        let running = HashMap::new();
        store.games.push(detail_view_game());

        let detail = query_game_detail_view(&store, &running, "test-uid-1").unwrap();
        assert_eq!(detail.precheck.game_uid, "test-uid-1");
        assert_eq!(detail.versions.len(), 0);
        assert_eq!(detail.body_versions.len(), 0);
        assert!(detail.runtime.is_none());
        assert!(detail.save_profile.is_none());
    }

    #[test]
    fn detail_view_versions_carry_counts_but_not_file_lists() {
        use crate::domain::{SaveFileEntry, SaveRootType, SaveVersion};

        let entry = |name: &str| SaveFileEntry {
            root_type: SaveRootType::SavedGames,
            root_path: None,
            relative_path: name.to_string(),
            object_hash: Some("a".repeat(64)),
            size: 2048,
            deleted: false,
            mtime_ms: None,
        };

        let mut store = AppStore::default();
        let running = HashMap::new();
        store.games.push(detail_view_game());
        store.save_versions.push(SaveVersion {
            version_id: "v1".to_string(),
            game_uid: "test-uid-1".to_string(),
            created_at: "1700000000".to_string(),
            total_bytes: 4096,
            files: vec![entry("slot1.sav"), entry("slot2.sav")],
        });

        let detail = query_game_detail_view(&store, &running, "test-uid-1").unwrap();
        assert_eq!(detail.versions.len(), 1);
        assert_eq!(detail.versions[0].file_count, 2);
        assert_eq!(detail.versions[0].total_bytes, 4096);

        // 详情页只消费计数，文件清单不得进入传输载荷。
        let version = serde_json::to_value(&detail).unwrap();
        assert!(
            version["versions"][0].get("files").is_none(),
            "详情页不应再序列化文件清单"
        );
        assert_eq!(version["versions"][0]["fileCount"], 2);
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

/// 从库中移除游戏时那套文件删除的守卫。
///
/// 这些测试守的是上一版最要命的那个性质：**失败被 `let _ =` 吞掉**。
/// 一旦有人把 `remove_if_present` 改回忽略错误、或者把失败清单丢掉，
/// `undeletable_paths_are_reported_instead_of_swallowed` 会立刻失败。
#[cfg(test)]
mod removal_tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("gamesaver-remove-{name}-{}", Uuid::new_v4()))
    }

    fn managed_target(path: &Path) -> RemovalTarget {
        RemovalTarget {
            label: "托管游戏目录",
            path: path.to_path_buf(),
        }
    }

    #[test]
    fn removes_directory_trees_and_reports_progress() {
        let root = temp_path("tree");
        fs::create_dir_all(root.join("nested/deep")).expect("建临时目录");
        fs::write(root.join("nested/deep/file.bin"), b"content").expect("写临时文件");

        let mut progress = Vec::new();
        let failures = remove_game_files(&[managed_target(&root)], None, |value, label| {
            progress.push((value, label.to_string()));
        });

        assert!(failures.is_empty(), "不该有失败项：{failures:?}");
        assert!(!root.exists(), "目录树应当已被删除");
        assert_eq!(progress.len(), 1, "每删一项应上报一次进度");
        assert!(
            (5..=90).contains(&progress[0].0),
            "进度应落在 5~90（100 留给任务收尾）：{:?}",
            progress[0]
        );
    }

    #[test]
    fn missing_paths_are_treated_as_success() {
        let root = temp_path("missing");
        let journal = root.join("no-such-journal.json");
        let failures = remove_game_files(&[managed_target(&root)], Some(&journal), |_, _| {});
        assert!(
            failures.is_empty(),
            "本来就不存在的路径不该算失败（否则每次移除都会报假警）：{failures:?}"
        );
    }

    #[test]
    fn undeletable_paths_are_reported_instead_of_swallowed() {
        // 拿一个「文件」当目录删：remove_dir_all 必然失败。这正是原先 `let _ =` 会吞掉的情况，
        // 结果是游戏已从库里消失、空间还占着，而没有任何地方告诉用户。
        let file = temp_path("file");
        fs::write(&file, b"x").expect("写临时文件");

        let failures = remove_game_files(&[managed_target(&file)], None, |_, _| {});

        assert_eq!(failures.len(), 1, "删除失败必须被报告：{failures:?}");
        assert!(
            failures[0].contains("托管游戏目录"),
            "失败项应带上标签便于定位：{failures:?}"
        );
        let _ = fs::remove_file(&file);
    }

    #[test]
    fn progress_stays_within_bounds_for_any_step_count() {
        for total in 0..8usize {
            for done in 0..=total {
                let value = progress_after(done, total);
                assert!(
                    (5..=90).contains(&value),
                    "total={total} done={done} 时进度越界：{value}"
                );
            }
        }
    }
}
