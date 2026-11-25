use crate::{
    app_state::AppState,
    domain::{CoverCrop, CoverPosition, GameCover},
    repositories::GameRepository,
    services::GameLibraryService,
};
use std::{
    collections::HashMap,
    fs,
    io::{Cursor, Write},
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
};
use tauri::{AppHandle, Manager};
use uuid::Uuid;

const MAX_ORIGINAL_COVER_BYTES: usize = 32 * 1024 * 1024;
const MAX_DISPLAY_COVER_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn install_cloud_cover(
    root: &Path,
    game_uid: &str,
    bytes: &[u8],
) -> Result<GameCover, String> {
    validate_component(game_uid, "游戏标识")?;
    let crop = CoverCrop {
        aspect_width: 16,
        aspect_height: 9,
        output_width: 1280,
        output_height: 720,
    };
    let position = CoverPosition {
        offset_x_milli: 0,
        offset_y_milli: 0,
        zoom_milli: 1000,
    };
    validate_cover_input(bytes, bytes, "jpg", &crop, &position)?;
    let cover_id = Uuid::new_v4().simple().to_string();
    let parent = root.join("covers").join(game_uid);
    validate_cover_target(root, &parent)?;
    let staging = parent.join(format!(".staging-{cover_id}"));
    let final_dir = parent.join(&cover_id);
    let result = (|| {
        fs::create_dir_all(&staging)
            .map_err(|error| format!("创建云端封面暂存目录失败：{error}"))?;
        validate_cover_target(root, &staging)?;
        write_synced_file(&staging.join("original.jpg"), bytes)?;
        write_synced_file(&staging.join("display.jpg"), bytes)?;
        fs::rename(&staging, &final_dir).map_err(|error| format!("提交云端封面失败：{error}"))?;
        Ok(GameCover {
            original_path: relative_cover_path(game_uid, &cover_id, "original.jpg"),
            display_path: relative_cover_path(game_uid, &cover_id, "display.jpg"),
            crop,
            position,
        })
    })();
    if result.is_err() && validate_cover_target(root, &staging).is_ok() {
        let _ = fs::remove_dir_all(staging);
    }
    result
}

struct CoverOperation<'a> {
    state: &'a AppState,
    key: String,
}

impl Drop for CoverOperation<'_> {
    fn drop(&mut self) {
        self.state.release_operation(&self.key);
    }
}

pub(crate) fn save_game_cover(
    app: &AppHandle,
    game_uid: &str,
    original_bytes: &[u8],
    display_bytes: &[u8],
    original_extension: &str,
    crop: CoverCrop,
    position: CoverPosition,
) -> Result<GameCover, String> {
    let game_uid = game_uid.trim();
    validate_component(game_uid, "游戏标识")?;
    let state = app.state::<AppState>();
    state.claim_operation(game_uid, "该游戏已有其他操作正在进行")?;
    let _claim = CoverOperation {
        state: &state,
        key: AppState::game_operation_key(game_uid),
    };
    validate_cover_input(
        original_bytes,
        display_bytes,
        original_extension,
        &crop,
        &position,
    )?;
    save_game_cover_files(
        app,
        &state,
        game_uid,
        original_bytes,
        display_bytes,
        original_extension,
        crop,
        position,
    )
}

pub(crate) fn save_game_cover_files(
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
    validate_cover_target(&root, &game_covers_root)?;
    fs::create_dir_all(&game_covers_root)
        .map_err(|error| format!("创建游戏封面目录失败：{error}"))?;
    validate_cover_target(&root, &game_covers_root)?;
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
                return Err(format!("保存游戏封面记录失败：{error}"));
            }
            Ok(game_key)
        })?;
        cleanup_old_cover(&root, game_uid, old_cover.as_ref(), &cover);
        maybe_sync_cover_to_cloud_async(app.clone(), game_key, display_bytes.to_vec());
        Ok(cover)
    })();
    if validate_cover_target(&root, &staging).is_ok() {
        let _ = fs::remove_dir_all(&staging);
    }
    if result.is_err() && validate_cover_target(&root, &final_dir).is_ok() {
        let _ = fs::remove_dir_all(&final_dir);
    }
    result
}

#[derive(Default)]
struct CoverSync {
    revision: AtomicU64,
    upload: Mutex<()>,
}

impl CoverSync {
    fn run_latest(&self, revision: u64, upload: impl FnOnce()) {
        let _upload = self
            .upload
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.revision.load(Ordering::SeqCst) == revision {
            upload();
        }
    }
}

static COVER_SYNCS: OnceLock<Mutex<HashMap<String, Arc<CoverSync>>>> = OnceLock::new();

fn cover_sync(game_key: &str) -> Arc<CoverSync> {
    let mut entries = COVER_SYNCS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    entries.entry(game_key.to_string()).or_default().clone()
}

fn maybe_sync_cover_to_cloud_async(app: AppHandle, game_key: String, display_bytes: Vec<u8>) {
    if game_key.trim().is_empty() {
        return;
    }
    let sync = cover_sync(&game_key);
    let revision = sync.revision.fetch_add(1, Ordering::SeqCst) + 1;
    let spawn = std::thread::Builder::new()
        .name("cover-sync".into())
        .spawn(move || {
            sync.run_latest(revision, || {
                let Ok(base_data_dir) = app.path().app_data_dir() else {
                    crate::logging::error("解析封面同步的数据目录失败");
                    return;
                };
                let Ok(client) =
                    crate::services::BaiduNetdiskClient::load_configured(&base_data_dir)
                else {
                    crate::logging::info("封面已保存在本地，百度网盘尚未就绪，未同步封面");
                    return;
                };
                let Ok(remote_dir) =
                    crate::services::BaiduNetdiskClient::remote_body_dir(&game_key)
                else {
                    return;
                };
                let remote_files = match client.list(&remote_dir) {
                    Ok(files) => files,
                    Err(error) => {
                        crate::logging::error(format!("读取云端游戏以同步封面失败：{error}"));
                        return;
                    }
                };
                let has_cloud_game = remote_files.iter().any(|file| {
                    file.path.ends_with("/manifest.json") || file.path.ends_with("/game.json")
                });
                if !has_cloud_game {
                    return;
                }
                let temporary_root = base_data_dir.join("cloud-manifest-temp");
                let cache_root = base_data_dir.join("cloud-manifest-cache");
                if let Err(error) = crate::services::CloudManifestService::write_cover(
                    &client,
                    &remote_dir,
                    &display_bytes,
                    &temporary_root,
                    Some(&cache_root),
                ) {
                    crate::logging::error(format!("封面已保存在本地，但云端同步失败：{error}"));
                }
                app.state::<AppState>().invalidate_cloud_store(false);
            })
        });
    if let Err(error) = spawn {
        crate::logging::error(format!("启动封面同步失败：{error}"));
    }
}

pub(crate) fn validate_cover_input(
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
        || !(-2_000_000..=2_000_000).contains(&position.offset_x_milli)
        || !(-2_000_000..=2_000_000).contains(&position.offset_y_milli)
    {
        return Err("封面裁剪位置无效".to_string());
    }
    validate_image(
        original_bytes,
        image::ImageFormat::from_extension(&extension).ok_or("封面图片格式无效")?,
        None,
    )?;
    validate_image(display_bytes, image::ImageFormat::Jpeg, Some((1280, 720)))?;
    Ok(())
}

fn validate_image(
    bytes: &[u8],
    format: image::ImageFormat,
    expected: Option<(u32, u32)>,
) -> Result<(), String> {
    let dimensions = image::ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|error| format!("封面图片损坏或不可解码：{error}"))?;
    if u64::from(dimensions.0) * u64::from(dimensions.1) > 64 * 1024 * 1024
        || expected.is_some_and(|size| size != dimensions)
    {
        return Err("封面图片实际尺寸无效或超过像素上限".into());
    }
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    reader
        .decode()
        .map_err(|error| format!("封面图片解码失败：{error}"))?;
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

pub(crate) fn validate_component(value: &str, label: &str) -> Result<(), String> {
    if !crate::domain::is_safe_path_segment(value)
        || Path::new(value).components().count() != 1
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
    validate_component(game_uid, "游戏标识")?;
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
    let parts = relative.components().collect::<Vec<_>>();
    if parts.len() != 4
        || parts
            .iter()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err("游戏封面路径结构无效".into());
    }
    let cover_id = parts[2].as_os_str().to_str().ok_or("封面版本标识无效")?;
    validate_component(cover_id, "封面版本标识")?;
    if cover_id.starts_with('.')
        || !matches!(
            parts[3].as_os_str().to_str(),
            Some("display.jpg" | "original.jpg" | "original.png" | "original.webp")
        )
    {
        return Err("游戏封面文件无效".into());
    }
    let path = root.join(relative);
    validate_cover_target(root, &path)?;
    Ok(path)
}

fn validate_cover_target(root: &Path, path: &Path) -> Result<(), String> {
    let relative = path.strip_prefix(root).map_err(|_| "封面路径超出游戏库")?;
    let mut current = root.to_path_buf();
    for part in relative.components() {
        if !matches!(part, Component::Normal(_)) {
            return Err("封面路径包含不安全的目录跳转".into());
        }
        current.push(part.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                #[cfg(windows)]
                let linked = {
                    use std::os::windows::fs::MetadataExt;
                    metadata.file_attributes() & 0x400 != 0
                };
                #[cfg(not(windows))]
                let linked = metadata.file_type().is_symlink();
                if linked {
                    return Err("封面路径包含符号链接或重解析目录".into());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("检查封面路径失败：{error}")),
        }
    }
    Ok(())
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
    let Ok(new_path) = safe_cover_path(root, game_uid, &new_cover.display_path) else {
        return;
    };
    if new_path.parent() == Some(old_dir) {
        return;
    }
    if let Err(error) = fs::remove_dir_all(old_dir) {
        if error.kind() != std::io::ErrorKind::NotFound {
            crate::logging::error(format!("清理旧封面失败，已保留新封面：{error}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempWorkspace;
    use std::sync::mpsc;
    use std::time::Duration;

    fn crop() -> CoverCrop {
        CoverCrop {
            aspect_width: 16,
            aspect_height: 9,
            output_width: 1280,
            output_height: 720,
        }
    }
    fn position() -> CoverPosition {
        CoverPosition {
            offset_x_milli: 0,
            offset_y_milli: 0,
            zoom_milli: 1000,
        }
    }
    fn cover(path: &str) -> GameCover {
        GameCover {
            original_path: String::new(),
            display_path: path.into(),
            crop: crop(),
            position: position(),
        }
    }
    fn image_bytes(format: image::ImageFormat, width: u32, height: u32) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image::RgbImage::new(width, height))
            .write_to(&mut cursor, format)
            .unwrap();
        cursor.into_inner()
    }
    fn write(root: &Path, relative: &str) -> PathBuf {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"cover").unwrap();
        path
    }

    #[test]
    fn cover_accepts_real_supported_images_and_expected_display_dimensions() {
        let display = image_bytes(image::ImageFormat::Jpeg, 1280, 720);
        for (format, extension) in [
            (image::ImageFormat::Png, "png"),
            (image::ImageFormat::Jpeg, "jpg"),
            (image::ImageFormat::WebP, "webp"),
        ] {
            let original = image_bytes(format, 32, 24);
            assert!(
                validate_cover_input(&original, &display, extension, &crop(), &position()).is_ok()
            );
        }
    }

    #[test]
    fn cover_rejects_magic_only_or_wrong_actual_display_size() {
        assert!(validate_cover_input(
            &[0x89, b'P', b'N', b'G'],
            &[0xff, 0xd8, 0xff],
            "png",
            &crop(),
            &position()
        )
        .is_err());
        let original = image_bytes(image::ImageFormat::Png, 32, 24);
        let display = image_bytes(image::ImageFormat::Jpeg, 640, 360);
        assert!(validate_cover_input(&original, &display, "png", &crop(), &position()).is_err());
    }

    #[test]
    fn cloud_cover_install_commits_both_files_and_rejects_invalid_input() {
        let workspace = TempWorkspace::new("cover-cloud-install");
        assert!(install_cloud_cover(&workspace, "game-a", &[0xff, 0xd8, 0xff]).is_err());
        assert!(!workspace.join("covers").exists());
        let image = image_bytes(image::ImageFormat::Jpeg, 1280, 720);
        let cover = install_cloud_cover(&workspace, "game-a", &image).unwrap();
        assert_eq!(
            fs::read(safe_cover_path(&workspace, "game-a", &cover.display_path).unwrap()).unwrap(),
            image
        );
        assert_eq!(
            fs::read(safe_cover_path(&workspace, "game-a", &cover.original_path).unwrap()).unwrap(),
            image
        );
        assert_eq!(
            fs::read_dir(workspace.join("covers/game-a"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn cover_rejects_extreme_signed_offsets_without_overflow() {
        for value in [i32::MIN, i32::MAX] {
            let mut invalid = position();
            invalid.offset_x_milli = value;
            assert!(validate_cover_input(
                &[0x89, b'P', b'N', b'G'],
                &[0xff, 0xd8, 0xff],
                "png",
                &crop(),
                &invalid
            )
            .is_err());
            invalid.offset_x_milli = 0;
            invalid.offset_y_milli = value;
            assert!(validate_cover_input(
                &[0x89, b'P', b'N', b'G'],
                &[0xff, 0xd8, 0xff],
                "png",
                &crop(),
                &invalid
            )
            .is_err());
        }
    }

    #[test]
    fn cover_rejects_nested_identifiers_and_nonfile_paths() {
        let workspace = TempWorkspace::new("cover-paths");
        assert!(validate_component("first/second", "id").is_err());
        assert!(validate_component("first\\second", "id").is_err());
        for path in [
            "covers/game-a",
            "covers/game-a/display.jpg",
            "covers/game-a/old/sub/display.jpg",
            "covers/game-b/old/display.jpg",
            "covers/game-a/../display.jpg",
            "covers/game-a/.staging-old/display.jpg",
        ] {
            assert!(
                safe_cover_path(&workspace, "game-a", path).is_err(),
                "{path}"
            );
        }
        assert!(safe_cover_path(&workspace, "game-a", "covers/game-a/old/display.jpg").is_ok());
    }

    #[test]
    fn cover_cleanup_never_deletes_game_or_shared_cover_roots() {
        let workspace = TempWorkspace::new("cover-cleanup");
        let new = write(&workspace, "covers/game-a/new/display.jpg");
        let other = write(&workspace, "covers/game-b/current/display.jpg");
        for old in [
            "covers/game-a",
            "covers/game-a/display.jpg",
            "covers/game-a/new/original.png",
        ] {
            cleanup_old_cover(
                &workspace,
                "game-a",
                Some(&cover(old)),
                &cover("covers/game-a/new/display.jpg"),
            );
            assert!(new.is_file() && other.is_file());
        }
        let old = write(&workspace, "covers/game-a/old/display.jpg");
        cleanup_old_cover(
            &workspace,
            "game-a",
            Some(&cover("covers/game-a/old/display.jpg")),
            &cover("covers/game-a/new/display.jpg"),
        );
        assert!(!old.exists());
        assert!(new.is_file() && other.is_file());
    }

    #[cfg(windows)]
    #[test]
    fn cover_rejects_junctions_before_read_write_or_cleanup() {
        let workspace = TempWorkspace::new("cover-junction");
        let library = workspace.join("library");
        let external = workspace.join("external");
        let outside = write(&external, "old/display.jpg");
        fs::create_dir_all(library.join("covers")).unwrap();
        let junction = library.join("covers").join("game-a");
        let result = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&external)
            .output()
            .unwrap();
        assert!(result.status.success());
        let read = safe_cover_path(&library, "game-a", "covers/game-a/old/display.jpg");
        let write = validate_cover_target(&library, &junction.join("new"));
        cleanup_old_cover(
            &library,
            "game-a",
            Some(&cover("covers/game-a/old/display.jpg")),
            &cover("covers/game-a/new/display.jpg"),
        );
        let retained = outside.is_file();
        fs::remove_dir(&junction).unwrap();
        assert!(read.is_err() && write.is_err() && retained);
    }

    #[test]
    fn cover_upload_coalesces_obsolete_revisions() {
        let sync = CoverSync::default();
        let old = sync.revision.fetch_add(1, Ordering::SeqCst) + 1;
        let latest = sync.revision.fetch_add(1, Ordering::SeqCst) + 1;
        sync.run_latest(old, || panic!("obsolete upload must be skipped"));
        let mut uploaded = false;
        sync.run_latest(latest, || uploaded = true);
        assert!(uploaded);
    }

    #[test]
    fn cover_upload_serializes_new_image_after_inflight_old_image() {
        let sync = Arc::new(CoverSync::default());
        let events = Arc::new(Mutex::new(Vec::new()));
        let (started_sender, started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let old = sync.revision.fetch_add(1, Ordering::SeqCst) + 1;
        let old_sync = sync.clone();
        let old_events = events.clone();
        let older = std::thread::spawn(move || {
            old_sync.run_latest(old, || {
                started_sender.send(()).unwrap();
                release_receiver
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap();
                old_events.lock().unwrap().push("old");
            })
        });
        started_receiver
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        let latest = sync.revision.fetch_add(1, Ordering::SeqCst) + 1;
        let new_sync = sync.clone();
        let new_events = events.clone();
        let newer = std::thread::spawn(move || {
            new_sync.run_latest(latest, || new_events.lock().unwrap().push("new"))
        });
        release_sender.send(()).unwrap();
        older.join().unwrap();
        newer.join().unwrap();
        assert_eq!(*events.lock().unwrap(), ["old", "new"]);
    }
}
