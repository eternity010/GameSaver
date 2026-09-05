use image::{imageops::FilterType, DynamicImage, ImageBuffer, ImageFormat, Rgba};
use serde::Serialize;
use std::{
    fs,
    io::Cursor,
    path::PathBuf,
    sync::{mpsc, Mutex, OnceLock},
    thread,
    time::{Duration, SystemTime},
};
use tauri::{AppHandle, Emitter, Manager};
use uuid::Uuid;
use windows_sys::Win32::{
    Foundation::RECT,
    Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT,
        DIB_RGB_COLORS, SRCCOPY,
    },
    UI::{
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
            VK_CONTROL, VK_MENU,
        },
        WindowsAndMessaging::{
            GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId, PeekMessageW, MSG,
            PM_REMOVE,
        },
    },
};

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(120);
const CAPTURE_CLEANUP_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const HOTKEY_ID: i32 = 0x4753;
const MAX_CAPTURE_EDGE: u32 = 1920;

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
    armed_at: std::time::Instant,
    image_path: Option<PathBuf>,
}

static ACTIVE_CAPTURE: OnceLock<Mutex<Option<PendingCapture>>> = OnceLock::new();

pub struct CoverCaptureService;

impl CoverCaptureService {
    pub fn arm(
        app: &AppHandle,
        game_uid: &str,
        managed_path: PathBuf,
    ) -> Result<CaptureArmView, String> {
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
                armed_at: std::time::Instant::now(),
                image_path: None,
            });
        }

        let app_for_thread = app.clone();
        let capture_id_for_thread = capture_id.clone();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        thread::spawn(move || run_hotkey_loop(app_for_thread, capture_id_for_thread, ready_sender));
        match ready_receiver.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(())) => Ok(CaptureArmView {
                capture_id,
                shortcut: "Ctrl + Alt + S".to_string(),
            }),
            Ok(Err(error)) => {
                crate::logging::error(format!("封面截图热键注册失败：{error}"));
                clear_capture(&capture_id);
                Err(error)
            }
            Err(_) => {
                crate::logging::error("封面截图热键注册超时".to_string());
                clear_capture(&capture_id);
                Err("注册封面截图快捷键超时".to_string())
            }
        }
    }

    pub fn discard(capture_id: &str) {
        let Ok(mut active) = active_capture().lock() else {
            return;
        };
        let Some(session) = active.as_ref() else {
            return;
        };
        if session.capture_id != capture_id {
            return;
        }
        if let Some(path) = &session.image_path {
            let _ = fs::remove_file(path);
        }
        *active = None;
    }

    pub fn capture_path(capture_id: &str) -> Option<PathBuf> {
        let active = active_capture().lock().ok()?;
        let session = active.as_ref()?;
        if session.capture_id != capture_id {
            return None;
        }
        session
            .image_path
            .as_ref()
            .filter(|path| path.is_file())
            .cloned()
    }
}

fn run_hotkey_loop(
    app: AppHandle,
    capture_id: String,
    ready_sender: mpsc::SyncSender<Result<(), String>>,
) {
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
            crate::logging::error("Ctrl + Alt + S 注册失败，可能与其他程序冲突".to_string());
            let _ = ready_sender.send(Err(
                "Ctrl + Alt + S 已被其他程序占用，请关闭冲突的快捷键后重试".to_string(),
            ));
            return;
        }
        crate::logging::info(format!("封面截图热键已注册：capture_id={capture_id}"));
        let _ = ready_sender.send(Ok(()));

        loop {
            let active = active_capture().lock().ok().and_then(|active| {
                active
                    .as_ref()
                    .filter(|session| session.capture_id == capture_id)
                    .map(|session| session.game_uid.clone())
            });
            let Some(game_uid) = active else {
                break;
            };

            while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {}
            let expired = active_capture()
                .lock()
                .ok()
                .map(|active| {
                    active
                        .as_ref()
                        .filter(|session| session.capture_id == capture_id)
                        .is_some_and(|session| session.armed_at.elapsed() > CAPTURE_TIMEOUT)
                })
                .unwrap_or(false);
            if expired {
                show_main_window(&app);
                notify_failure(
                    &app,
                    &capture_id,
                    &game_uid,
                    "封面截图会话已超时，请重新发起截图",
                );
                clear_capture(&capture_id);
                break;
            }

            let ctrl_pressed = GetAsyncKeyState(VK_CONTROL as i32) < 0;
            let alt_pressed = GetAsyncKeyState(VK_MENU as i32) < 0;
            let s_pressed = GetAsyncKeyState(b'S' as i32) < 0;
            if ctrl_pressed && alt_pressed && s_pressed {
                crate::logging::info(format!("封面截图按键状态已检测到：capture_id={capture_id}"));
                crate::logging::info(format!("封面截图热键已触发：capture_id={capture_id}"));
                match capture_foreground_window(&capture_id) {
                    Ok(()) => {
                        crate::logging::info(format!("封面截图完成：capture_id={capture_id}"));
                        show_main_window(&app);
                        let _ = app.emit(
                            "cover-capture-ready",
                            CoverCaptureReady {
                                capture_id: capture_id.clone(),
                                game_uid,
                            },
                        );
                    }
                    Err(error) => {
                        crate::logging::error(format!(
                            "封面截图失败：capture_id={capture_id} error={error}"
                        ));
                        show_main_window(&app);
                        notify_failure(&app, &capture_id, &game_uid, &error);
                        clear_capture(&capture_id);
                    }
                }
                break;
            }
        }
        let _ = UnregisterHotKey(std::ptr::null_mut(), HOTKEY_ID);
    }
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

    let path = capture_directory()?.join(format!("{capture_id}.png"));
    fs::write(&path, bytes).map_err(|error| format!("写入临时游戏截图失败：{error}"))?;
    let mut active = active_capture()
        .lock()
        .map_err(|_| "锁定封面截图状态失败".to_string())?;
    let session = active
        .as_mut()
        .filter(|session| session.capture_id == capture_id)
        .ok_or_else(|| "封面截图会话已失效".to_string())?;
    session.image_path = Some(path);
    Ok(())
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
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
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
    let Ok(mut active) = active_capture().lock() else {
        return;
    };
    if let Some(session) = active
        .as_ref()
        .filter(|session| session.capture_id == capture_id)
    {
        if let Some(path) = &session.image_path {
            let _ = fs::remove_file(path);
        }
        *active = None;
    }
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
