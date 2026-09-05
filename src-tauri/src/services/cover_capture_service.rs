use image::{imageops::FilterType, DynamicImage, ImageBuffer, ImageFormat, Rgba};
use serde::Serialize;
use std::{
    collections::HashSet,
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    sync::{mpsc, Mutex, OnceLock},
    thread,
    time::{Duration, Instant, SystemTime},
};
use tauri::{AppHandle, Emitter, Manager};
use uuid::Uuid;
use windows_sys::Win32::{
    Foundation::{GetLastError, RECT},
    Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT,
        DIB_RGB_COLORS, SRCCOPY,
    },
    UI::{
        Input::KeyboardAndMouse::{RegisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT},
        WindowsAndMessaging::{
            GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId, PeekMessageW, MSG,
            PM_REMOVE, WM_HOTKEY,
        },
    },
};

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(120);
const CAPTURE_RESULT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const CAPTURE_CLEANUP_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const HOTKEY_POLL_INTERVAL: Duration = Duration::from_millis(25);
const HOTKEY_ID: i32 = 0x4753;
const MAX_CAPTURE_EDGE: u32 = 1920;
const MAX_CAPTURE_PIXELS: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureArmView {
    pub capture_id: String,
    pub shortcut: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CoverCaptureReady {
    pub capture_id: String,
    pub game_uid: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CoverCaptureFailure {
    capture_id: String,
    game_uid: String,
    message: String,
}

struct PendingCapture {
    capture_id: String,
    game_uid: String,
    managed_path: PathBuf,
    armed_at: Instant,
    captured_at: Option<Instant>,
    image_path: Option<PathBuf>,
}

static ACTIVE_CAPTURE: OnceLock<Mutex<Option<PendingCapture>>> = OnceLock::new();
static HOTKEY_LISTENER_STATUS: OnceLock<Result<(), String>> = OnceLock::new();

pub struct CoverCaptureService;

impl CoverCaptureService {
    pub fn start_global_listener(app: AppHandle) -> Result<(), String> {
        HOTKEY_LISTENER_STATUS
            .get_or_init(|| {
                if let Err(error) = cleanup_old_captures() {
                    crate::logging::error(format!("启动时清理封面截图临时文件失败：{error}"));
                }
                let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
                thread::spawn(move || run_hotkey_loop(app, ready_sender));
                ready_receiver
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap_or_else(|_| Err("注册全局截图快捷键超时".to_string()))
            })
            .clone()
    }

    pub fn arm(game_uid: &str, managed_path: PathBuf) -> Result<CaptureArmView, String> {
        match HOTKEY_LISTENER_STATUS.get() {
            Some(Ok(())) => {}
            Some(Err(error)) => return Err(format!("全局截图快捷键不可用：{error}")),
            None => return Err("全局截图快捷键尚未初始化，请重启 GameSaver".to_string()),
        }
        crate::logging::info(format!(
            "开始封面截图：game_uid={game_uid} managed_path={}",
            managed_path.display()
        ));
        cleanup_old_captures()?;
        let capture_id = Uuid::new_v4().to_string();
        {
            let mut active = active_capture()
                .lock()
                .map_err(|_| "锁定封面截图状态失败".to_string())?;
            if active.is_some() {
                return Err("已有封面截图会话正在等待快捷键".to_string());
            }
            *active = Some(PendingCapture {
                capture_id: capture_id.clone(),
                game_uid: game_uid.to_string(),
                managed_path,
                armed_at: Instant::now(),
                captured_at: None,
                image_path: None,
            });
        }

        Ok(CaptureArmView {
            capture_id,
            shortcut: "Ctrl + Alt + S".to_string(),
        })
    }

    pub fn discard(capture_id: &str) {
        if let Some(path) = take_capture(capture_id).and_then(|session| session.image_path) {
            let _ = fs::remove_file(path);
        }
    }

    pub fn capture_path(capture_id: &str) -> Option<PathBuf> {
        let path = active_capture()
            .lock()
            .ok()?
            .as_ref()
            .filter(|session| session.capture_id == capture_id)
            .and_then(|session| session.image_path.clone())?;
        path.is_file().then_some(path)
    }
}

fn run_hotkey_loop(app: AppHandle, ready_sender: mpsc::SyncSender<Result<(), String>>) {
    unsafe {
        let mut message = std::mem::zeroed::<MSG>();
        let _ = PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE);
        if RegisterHotKey(
            std::ptr::null_mut(),
            HOTKEY_ID,
            MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
            b'S' as u32,
        ) == 0
        {
            let error = format!(
                "Ctrl + Alt + S 注册失败，可能与其他程序冲突（Windows 错误码：{}）",
                GetLastError()
            );
            crate::logging::error(&error);
            let _ = ready_sender.send(Err(error));
            return;
        }
        crate::logging::info("封面截图全局热键已注册：Ctrl + Alt + S");
        let _ = ready_sender.send(Ok(()));

        loop {
            let mut hotkey_triggered = false;
            while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                if message.message == WM_HOTKEY && message.wParam == HOTKEY_ID as usize {
                    hotkey_triggered = true;
                    break;
                }
            }
            if hotkey_triggered {
                handle_hotkey(&app);
            }
            expire_capture_if_needed(&app);
            thread::sleep(HOTKEY_POLL_INTERVAL);
        }
    }
}

fn handle_hotkey(app: &AppHandle) {
    let active = active_capture().lock().ok().and_then(|active| {
        active.as_ref().map(|session| {
            (
                session.capture_id.clone(),
                session.game_uid.clone(),
                session.image_path.is_some(),
            )
        })
    });
    let (capture_id, game_uid, has_image) = match active {
        Some(value) => value,
        None => match create_capture_for_foreground_game(app) {
            Ok(value) => value,
            Err(error) => {
                crate::logging::info(format!("全局截图快捷键未处理：{error}"));
                return;
            }
        },
    };
    if has_image {
        crate::logging::info(format!(
            "封面截图快捷键忽略：当前会话已有截图：capture_id={capture_id}"
        ));
        return;
    }

    crate::logging::info(format!("封面截图热键已触发：capture_id={capture_id}"));
    match capture_foreground_window(&capture_id) {
        Ok(()) => {
            crate::logging::info(format!("封面截图完成：capture_id={capture_id}"));
            show_main_window(app);
            let _ = app.emit(
                "cover-capture-ready",
                CoverCaptureReady {
                    capture_id,
                    game_uid,
                },
            );
        }
        Err(error) => {
            crate::logging::error(format!(
                "封面截图失败：capture_id={capture_id} error={error}"
            ));
            show_main_window(app);
            notify_failure(app, &capture_id, &game_uid, &error);
            clear_capture(&capture_id);
        }
    }
}

fn create_capture_for_foreground_game(app: &AppHandle) -> Result<(String, String, bool), String> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return Err("未找到前台游戏窗口".to_string());
    }
    let mut process_id = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut process_id);
    }
    let image_path = crate::services::process_service::get_process_image_path(process_id)
        .ok_or_else(|| "无法确认前台进程路径".to_string())?;
    let state = app.state::<crate::app_state::AppState>();
    let games = {
        let store = state
            .store
            .lock()
            .map_err(|_| "读取游戏库失败".to_string())?;
        crate::services::GameLibraryService::list(&store)
    };
    let running_game_uids = state
        .running_games
        .lock()
        .map_err(|_| "读取游戏运行状态失败".to_string())?
        .keys()
        .cloned()
        .collect::<HashSet<_>>();
    let game = games
        .into_iter()
        .find(|game| {
            running_game_uids.contains(&game.game_uid)
                && crate::services::process_service::is_process_in_directory(
                    &image_path,
                    Path::new(&game.managed_path),
                )
        })
        .ok_or_else(|| "当前前台窗口不属于正在运行的受管游戏".to_string())?;
    let capture_id = Uuid::new_v4().to_string();
    let mut active = active_capture()
        .lock()
        .map_err(|_| "锁定封面截图状态失败".to_string())?;
    if active.is_some() {
        return Err("已有封面截图会话正在等待处理".to_string());
    }
    *active = Some(PendingCapture {
        capture_id: capture_id.clone(),
        game_uid: game.game_uid.clone(),
        managed_path: PathBuf::from(game.managed_path),
        armed_at: Instant::now(),
        captured_at: None,
        image_path: None,
    });
    crate::logging::info(format!(
        "全局快捷键自动创建封面截图会话：game_uid={} capture_id={capture_id}",
        game.game_uid
    ));
    Ok((capture_id, game.game_uid, false))
}

fn expire_capture_if_needed(app: &AppHandle) {
    let expired = active_capture().lock().ok().and_then(|active| {
        active.as_ref().and_then(|session| {
            let result_expired = session
                .captured_at
                .is_some_and(|captured_at| captured_at.elapsed() > CAPTURE_RESULT_TIMEOUT);
            let waiting_expired =
                session.image_path.is_none() && session.armed_at.elapsed() > CAPTURE_TIMEOUT;
            (result_expired || waiting_expired).then(|| {
                (
                    session.capture_id.clone(),
                    session.game_uid.clone(),
                    result_expired,
                )
            })
        })
    });
    let Some((capture_id, game_uid, result_expired)) = expired else {
        return;
    };
    if result_expired {
        crate::logging::info(format!(
            "未处理的封面截图结果已清理：capture_id={capture_id}"
        ));
    } else {
        crate::logging::info(format!("封面截图会话超时：capture_id={capture_id}"));
        show_main_window(app);
        notify_failure(
            app,
            &capture_id,
            &game_uid,
            "封面截图会话已超时，请重新发起截图",
        );
    }
    clear_capture(&capture_id);
}

fn capture_foreground_window(capture_id: &str) -> Result<(), String> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return Err("未找到可截取的前台窗口".to_string());
    }
    let mut process_id = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut process_id);
    }
    let managed_path = active_capture()
        .lock()
        .map_err(|_| "锁定封面截图状态失败".to_string())?
        .as_ref()
        .filter(|session| session.capture_id == capture_id)
        .map(|session| session.managed_path.clone())
        .ok_or_else(|| "封面截图会话已失效".to_string())?;
    let image_path = crate::services::process_service::get_process_image_path(process_id)
        .ok_or_else(|| "无法确认当前前台窗口所属进程".to_string())?;
    crate::logging::info(format!(
        "封面截图前台进程：pid={process_id} path={} capture_id={capture_id}",
        image_path.display()
    ));
    if !crate::services::process_service::is_process_in_directory(&image_path, &managed_path) {
        return Err("当前前台窗口不属于该游戏，请切回游戏后重试".to_string());
    }
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
        return Err("无法读取游戏窗口范围".to_string());
    }
    let width = (rect.right - rect.left).max(0) as u32;
    let height = (rect.bottom - rect.top).max(0) as u32;
    if width < 32 || height < 32 {
        return Err("当前窗口尺寸过小，无法截取封面".to_string());
    }
    capture_buffer_len(width, height)?;

    let bgra = unsafe { capture_screen_region(rect.left, rect.top, width, height)? };
    let image = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(width, height, bgra)
        .ok_or_else(|| "转换游戏截图像素失败".to_string())?;
    let image = if width.max(height) > MAX_CAPTURE_EDGE {
        let scale = MAX_CAPTURE_EDGE as f64 / width.max(height) as f64;
        let target_width = (width as f64 * scale).round().max(1.0) as u32;
        let target_height = (height as f64 * scale).round().max(1.0) as u32;
        image::imageops::resize(&image, target_width, target_height, FilterType::Triangle)
    } else {
        image
    };
    let mut bytes = Vec::new();
    DynamicImage::ImageRgba8(image)
        .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
        .map_err(|error| format!("编码游戏截图失败：{error}"))?;
    if bytes.len() > 24 * 1024 * 1024 {
        return Err("游戏截图过大，请降低游戏分辨率后重试".to_string());
    }

    let directory = capture_directory()?;
    let path = directory.join(format!("{capture_id}.png"));
    let temporary_path = directory.join(format!("{capture_id}.png.tmp"));
    fs::write(&temporary_path, bytes).map_err(|error| format!("写入临时游戏截图失败：{error}"))?;
    if let Err(error) = fs::rename(&temporary_path, &path) {
        let _ = fs::remove_file(&temporary_path);
        return Err(format!("提交临时游戏截图失败：{error}"));
    }
    let attached = active_capture()
        .lock()
        .map_err(|_| "锁定封面截图状态失败".to_string())?
        .as_mut()
        .filter(|session| session.capture_id == capture_id)
        .map(|session| {
            session.image_path = Some(path.clone());
            session.captured_at = Some(Instant::now());
        })
        .is_some();
    if !attached {
        let _ = fs::remove_file(&path);
        return Err("封面截图会话已失效".to_string());
    }
    Ok(())
}

fn capture_buffer_len(width: u32, height: u32) -> Result<usize, String> {
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| "游戏窗口尺寸异常，无法分配截图缓冲区".to_string())?;
    if pixels > MAX_CAPTURE_PIXELS {
        return Err("游戏窗口尺寸过大，无法安全截取封面".to_string());
    }
    pixels
        .checked_mul(4)
        .ok_or_else(|| "游戏窗口尺寸异常，无法分配截图缓冲区".to_string())
}

unsafe fn capture_screen_region(
    left: i32,
    top: i32,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, String> {
    let screen = GetDC(std::ptr::null_mut());
    if screen.is_null() {
        return Err("无法访问屏幕截图设备".to_string());
    }
    let memory = CreateCompatibleDC(screen);
    if memory.is_null() {
        let _ = ReleaseDC(std::ptr::null_mut(), screen);
        return Err("无法创建截图缓冲区".to_string());
    }
    let bitmap = CreateCompatibleBitmap(screen, width as i32, height as i32);
    if bitmap.is_null() {
        let _ = DeleteDC(memory);
        let _ = ReleaseDC(std::ptr::null_mut(), screen);
        return Err("无法创建截图位图".to_string());
    }
    let previous = SelectObject(memory, bitmap);
    let copied = BitBlt(
        memory,
        0,
        0,
        width as i32,
        height as i32,
        screen,
        left,
        top,
        SRCCOPY | CAPTUREBLT,
    ) != 0;
    let mut pixels = vec![0u8; capture_buffer_len(width, height)?];
    let mut bitmap_info = std::mem::zeroed::<BITMAPINFO>();
    bitmap_info.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width as i32,
        biHeight: -(height as i32),
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        ..std::mem::zeroed()
    };
    let copied_pixels = if copied {
        GetDIBits(
            memory,
            bitmap,
            0,
            height,
            pixels.as_mut_ptr().cast(),
            &mut bitmap_info,
            DIB_RGB_COLORS,
        ) == height as i32
    } else {
        false
    };
    let _ = SelectObject(memory, previous);
    let _ = DeleteObject(bitmap);
    let _ = DeleteDC(memory);
    let _ = ReleaseDC(std::ptr::null_mut(), screen);
    if !copied_pixels {
        return Err("无法读取游戏画面，可能受到独占全屏或保护内容限制".to_string());
    }
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        pixel[3] = 255;
    }
    Ok(pixels)
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        if let Err(error) = window.show() {
            crate::logging::error(format!("恢复主窗口失败：{error}"));
        }
        if let Err(error) = window.set_focus() {
            crate::logging::error(format!("聚焦主窗口失败：{error}"));
        }
    }
}

fn notify_failure(app: &AppHandle, capture_id: &str, game_uid: &str, message: &str) {
    let _ = app.emit(
        "cover-capture-failed",
        CoverCaptureFailure {
            capture_id: capture_id.to_string(),
            game_uid: game_uid.to_string(),
            message: message.to_string(),
        },
    );
}

fn clear_capture(capture_id: &str) {
    if let Some(path) = take_capture(capture_id).and_then(|session| session.image_path) {
        let _ = fs::remove_file(path);
    }
}

fn take_capture(capture_id: &str) -> Option<PendingCapture> {
    let mut active = active_capture().lock().ok()?;
    active
        .as_ref()
        .is_some_and(|session| session.capture_id == capture_id)
        .then(|| active.take())
        .flatten()
}

fn active_capture() -> &'static Mutex<Option<PendingCapture>> {
    ACTIVE_CAPTURE.get_or_init(|| Mutex::new(None))
}

fn capture_directory() -> Result<PathBuf, String> {
    let directory = std::env::temp_dir().join("gamesaver-next-cover-captures");
    fs::create_dir_all(&directory).map_err(|error| format!("创建封面截图临时目录失败：{error}"))?;
    Ok(directory)
}

fn cleanup_old_captures() -> Result<(), String> {
    let directory = capture_directory()?;
    let now = SystemTime::now();
    for entry in
        fs::read_dir(directory).map_err(|error| format!("读取封面截图临时目录失败：{error}"))?
    {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let stale = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > CAPTURE_CLEANUP_AGE);
        if stale && path.is_file() {
            let _ = fs::remove_file(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{capture_buffer_len, MAX_CAPTURE_PIXELS};

    #[test]
    fn capture_buffer_rejects_unreasonably_large_windows() {
        assert!(capture_buffer_len(7680, 4320).is_ok());
        assert!(capture_buffer_len(MAX_CAPTURE_PIXELS as u32, 2).is_err());
    }
}
