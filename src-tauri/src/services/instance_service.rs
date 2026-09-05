use std::sync::{Mutex, OnceLock};

const ADMIN_RELAUNCH_ARGUMENT_PREFIX: &str = "--gamesaver-admin-relaunch=";

static INSTANCE_MUTEX_HANDLE: OnceLock<Mutex<Option<isize>>> = OnceLock::new();

pub struct InstanceService;

impl InstanceService {
    pub fn acquire_or_focus_existing() -> Result<bool, String> {
        #[cfg(target_os = "windows")]
        {
            acquire_windows_instance()
        }

        #[cfg(not(target_os = "windows"))]
        {
            Ok(true)
        }
    }

    pub fn admin_relaunch_argument() -> String {
        format!("{ADMIN_RELAUNCH_ARGUMENT_PREFIX}{}", std::process::id())
    }
}

fn ignored_relaunch_pid() -> Option<u32> {
    std::env::args().find_map(|argument| {
        argument
            .strip_prefix(ADMIN_RELAUNCH_ARGUMENT_PREFIX)
            .and_then(|value| value.parse::<u32>().ok())
    })
}

#[cfg(target_os = "windows")]
fn acquire_windows_instance() -> Result<bool, String> {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt, ptr::null};
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    let ignored_pid = ignored_relaunch_pid();
    if let Some(process_id) = ignored_pid {
        wait_for_relaunch_source(process_id);
    }
    if let Some(existing_pid) = find_existing_process(ignored_pid)? {
        focus_process_window(existing_pid);
        return Ok(false);
    }

    let mutex_name = OsStr::new("Local\\GameSaverNext.SingleInstance")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let handle: HANDLE = unsafe { CreateMutexW(null(), 0, mutex_name.as_ptr()) };
    if handle.is_null() {
        return Err(format!("创建 GameSaver 单实例锁失败：错误 {}", unsafe {
            GetLastError()
        }));
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe { CloseHandle(handle) };
        if let Some(existing_pid) = find_existing_process(ignored_pid)? {
            focus_process_window(existing_pid);
        }
        return Ok(false);
    }

    *INSTANCE_MUTEX_HANDLE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| "保存 GameSaver 单实例锁失败".to_string())? = Some(handle as isize);
    Ok(true)
}

#[cfg(target_os = "windows")]
fn wait_for_relaunch_source(process_id: u32) {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        Storage::FileSystem::SYNCHRONIZE,
        System::Threading::{OpenProcess, WaitForSingleObject},
    };

    let process = unsafe { OpenProcess(SYNCHRONIZE, 0, process_id) };
    if process.is_null() {
        return;
    }
    unsafe {
        WaitForSingleObject(process, 10_000);
        CloseHandle(process);
    }
}

#[cfg(target_os = "windows")]
fn find_existing_process(ignored_pid: Option<u32>) -> Result<Option<u32>, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
                TH32CS_SNAPPROCESS,
            },
            Threading::GetCurrentProcessId,
        },
    };

    let executable_name = std::env::current_exe()
        .map_err(|error| format!("读取当前程序路径失败：{error}"))?
        .file_name()
        .ok_or_else(|| "当前程序路径缺少文件名".to_string())?
        .encode_wide()
        .collect::<Vec<_>>();
    let current_pid = unsafe { GetCurrentProcessId() };
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err("读取运行中程序列表失败".to_string());
    }

    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..PROCESSENTRY32W::default()
    };
    let mut found = None;
    let mut has_entry = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while has_entry {
        let name_end = entry
            .szExeFile
            .iter()
            .position(|character| *character == 0)
            .unwrap_or(entry.szExeFile.len());
        let same_name = entry.szExeFile[..name_end].eq_ignore_ascii_case(&executable_name);
        if same_name
            && entry.th32ProcessID != current_pid
            && Some(entry.th32ProcessID) != ignored_pid
        {
            found = Some(entry.th32ProcessID);
            break;
        }
        has_entry = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    Ok(found)
}

#[cfg(target_os = "windows")]
fn focus_process_window(process_id: u32) {
    use windows_sys::{
        core::BOOL,
        Win32::{
            Foundation::{HWND, LPARAM},
            UI::WindowsAndMessaging::{
                EnumWindows, GetWindowThreadProcessId, IsWindowVisible, SetForegroundWindow,
                ShowWindow, SW_RESTORE,
            },
        },
    };

    struct SearchContext {
        process_id: u32,
        window: HWND,
    }

    unsafe extern "system" fn visit_window(window: HWND, parameter: LPARAM) -> BOOL {
        let context = &mut *(parameter as *mut SearchContext);
        let mut window_process_id = 0;
        GetWindowThreadProcessId(window, &mut window_process_id);
        if window_process_id == context.process_id && IsWindowVisible(window) != 0 {
            context.window = window;
            return 0;
        }
        1
    }

    let mut context = SearchContext {
        process_id,
        window: std::ptr::null_mut(),
    };
    unsafe {
        EnumWindows(
            Some(visit_window),
            (&mut context as *mut SearchContext) as LPARAM,
        );
        if !context.window.is_null() {
            ShowWindow(context.window, SW_RESTORE);
            SetForegroundWindow(context.window);
        }
    }
}

trait WideAsciiCaseEq {
    fn eq_ignore_ascii_case(&self, other: &[u16]) -> bool;
}

impl WideAsciiCaseEq for [u16] {
    fn eq_ignore_ascii_case(&self, other: &[u16]) -> bool {
        self.len() == other.len()
            && self.iter().zip(other).all(|(left, right)| {
                let left = u16::from((*left as u8).to_ascii_lowercase());
                let right = u16::from((*right as u8).to_ascii_lowercase());
                left == right
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{WideAsciiCaseEq, ADMIN_RELAUNCH_ARGUMENT_PREFIX};

    #[test]
    fn executable_names_compare_without_ascii_case() {
        let left = "GameSaver_Next.exe".encode_utf16().collect::<Vec<_>>();
        let right = "gamesaver_next.exe".encode_utf16().collect::<Vec<_>>();
        assert!(left.eq_ignore_ascii_case(&right));
    }

    #[test]
    fn admin_relaunch_argument_has_parseable_process_id() {
        let argument = super::InstanceService::admin_relaunch_argument();
        assert!(argument.starts_with(ADMIN_RELAUNCH_ARGUMENT_PREFIX));
        assert!(argument[ADMIN_RELAUNCH_ARGUMENT_PREFIX.len()..]
            .parse::<u32>()
            .is_ok());
    }
}
