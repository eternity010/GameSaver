use std::path::{Path, PathBuf};

#[cfg(target_os = "windows")]
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
    System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    },
    System::Threading::{
        GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
        WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    },
};

#[cfg(target_os = "windows")]
const SYNCHRONIZE: u32 = 0x00100000;

#[allow(dead_code)]
pub struct ProcessService;

#[cfg(target_os = "windows")]
pub struct TrackedProcessHandle {
    #[allow(dead_code)]
    pub pid: u32,
    handle: HANDLE,
}

#[cfg(target_os = "windows")]
impl TrackedProcessHandle {
    pub fn open(pid: u32) -> Option<Self> {
        if pid == 0 {
            return None;
        }
        let handle =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            None
        } else {
            Some(Self { pid, handle })
        }
    }

    pub fn is_alive(&self) -> bool {
        const WAIT_TIMEOUT: u32 = 258;
        const STILL_ACTIVE: u32 = 259;

        unsafe {
            let wait_res = WaitForSingleObject(self.handle, 0);
            if wait_res != WAIT_TIMEOUT {
                return false;
            }
            let mut exit_code = 0u32;
            let ok = GetExitCodeProcess(self.handle, &mut exit_code);
            ok != 0 && exit_code == STILL_ACTIVE
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for TrackedProcessHandle {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }
}

// Windows 句柄是**进程范围**的内核对象，不属于创建它的线程。把句柄的独占所有权交给
// 另一个线程（`Send` 的语义）是安全的 —— 跨重启接手的会话正是这么用的：在启动线程里
// 认领游戏进程，再把等待交给会话线程。
//
// 没有一并引入 `Sync`：句柄只被独占使用，不会被多线程共享。
#[cfg(target_os = "windows")]
unsafe impl Send for TrackedProcessHandle {}

#[cfg(not(target_os = "windows"))]
pub struct TrackedProcessHandle {
    pub pid: u32,
}

#[cfg(not(target_os = "windows"))]
impl TrackedProcessHandle {
    pub fn open(pid: u32) -> Option<Self> {
        Some(Self { pid })
    }

    pub fn is_alive(&self) -> bool {
        false
    }
}

pub fn is_process_running(pid: u32) -> bool {
    TrackedProcessHandle::open(pid)
        .map(|h| h.is_alive())
        .unwrap_or(false)
}

/// 强制终止一个进程。
///
/// 与 [`TrackedProcessHandle`] 分开实现是刻意的：那个句柄只申请查询权限，被
/// [`is_process_running`] 用来探测**任意** PID。若把 `PROCESS_TERMINATE` 并进
/// 它的打开权限，一部分受限进程会从「能读到退出状态」退化成「打开失败」，
/// 于是被静默判定成「不在运行」——取消启动任务的收尾会因此漏掉真正的子进程。
///
/// 本函数按 PID 重新打开句柄，理论上存在 PID 复用风险；但调用方（游戏会话的
/// 进程跟踪）始终持有 [`TrackedProcessHandle`]，句柄未关闭时 Windows 不会回收
/// 该 PID，因此该风险不成立。
///
/// 拒绝终止自身与 PID 0（System Idle Process）：前者是自杀，后者必然失败。
#[cfg(target_os = "windows")]
pub fn terminate_process(pid: u32) -> bool {
    if pid == 0 || pid == std::process::id() {
        return false;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            return false;
        }
        let terminated = TerminateProcess(handle, 1) != 0;
        CloseHandle(handle);
        terminated
    }
}

#[cfg(not(target_os = "windows"))]
pub fn terminate_process(_pid: u32) -> bool {
    false
}

pub fn is_ignored_process_name(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    let file_name = Path::new(&lower)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&lower);

    file_name.starts_with("unitycrashhandler")
        || file_name.starts_with("crashreportclient")
        || file_name.starts_with("werfault")
        || file_name.starts_with("bugsplat")
        || file_name == "errorreport.exe"
        || file_name == "dxsetup.exe"
        || file_name.starts_with("vcredist")
        || file_name.starts_with("vc_redist")
}

pub fn is_process_in_directory(image_path: &Path, directory: &Path) -> bool {
    let norm_image = normalize_path(image_path);
    let norm_dir = normalize_path(directory);
    if norm_image.is_empty() || norm_dir.is_empty() {
        return false;
    }

    if norm_image.len() <= norm_dir.len() {
        return false;
    }

    if norm_image.starts_with(&norm_dir) {
        let remainder = &norm_image[norm_dir.len()..];
        return remainder.starts_with('/') || remainder.starts_with('\\');
    }
    false
}

fn normalize_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let trimmed = raw.trim().trim_start_matches(r"\\?\");
    let mut normalized = trimmed.replace('\\', "/").to_ascii_lowercase();
    while normalized.ends_with('/') {
        normalized.pop();
    }
    normalized
}

#[cfg(target_os = "windows")]
pub fn get_process_image_path(pid: u32) -> Option<PathBuf> {
    if pid == 0 {
        return None;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut buffer = [0u16; 1024];
        let mut size = buffer.len() as u32;
        let success = QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size);
        CloseHandle(handle);
        if success != 0 && size > 0 {
            let path_str = String::from_utf16_lossy(&buffer[..size as usize]);
            Some(PathBuf::from(path_str))
        } else {
            None
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub fn get_process_image_path(_pid: u32) -> Option<PathBuf> {
    None
}

#[cfg(target_os = "windows")]
pub fn find_processes_in_directory(directory: &Path) -> Vec<u32> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Vec::new();
    }

    let mut result = Vec::new();
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..unsafe { std::mem::zeroed() }
    };

    let mut has_entry = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while has_entry {
        let pid = entry.th32ProcessID;
        if pid > 4 {
            if let Some(image_path) = get_process_image_path(pid) {
                if is_process_in_directory(&image_path, directory) {
                    let file_name = image_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or_default();
                    if !is_ignored_process_name(file_name) {
                        result.push(pid);
                    }
                }
            }
        }
        has_entry = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }

    unsafe {
        CloseHandle(snapshot);
    }

    result
}

#[cfg(not(target_os = "windows"))]
pub fn find_processes_in_directory(_directory: &Path) -> Vec<u32> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_ignored_process_name() {
        assert!(is_ignored_process_name("UnityCrashHandler64.exe"));
        assert!(is_ignored_process_name("unitycrashhandler32.exe"));
        assert!(is_ignored_process_name("CrashReportClient.exe"));
        assert!(is_ignored_process_name("WerFault.exe"));
        assert!(is_ignored_process_name("werfaultsecure.exe"));
        assert!(is_ignored_process_name("bugsplat.exe"));
        assert!(is_ignored_process_name("dxsetup.exe"));
        assert!(is_ignored_process_name("vcredist_x64.exe"));

        assert!(!is_ignored_process_name("Game.exe"));
        assert!(!is_ignored_process_name("Captive_Lili.exe"));
        assert!(!is_ignored_process_name("CoinPussy.exe"));
        assert!(!is_ignored_process_name("Game-Win64-Shipping.exe"));
    }

    #[test]
    fn test_is_process_in_directory() {
        let dir = Path::new("E:\\GameSaverGames\\games\\abc-123");

        let nested = Path::new("e:/gamesavergames/games/abc-123/binaries/win64/game.exe");
        assert!(is_process_in_directory(nested, dir));

        let direct = Path::new("E:\\GameSaverGames\\games\\abc-123\\Game.exe");
        assert!(is_process_in_directory(direct, dir));

        let sibling = Path::new("E:\\GameSaverGames\\games\\abc-123-extra\\Game.exe");
        assert!(!is_process_in_directory(sibling, dir));

        let parent = Path::new("E:\\GameSaverGames\\games\\Game.exe");
        assert!(!is_process_in_directory(parent, dir));
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn test_get_current_process_image_path() {
        let current_pid = std::process::id();
        let path = get_process_image_path(current_pid);
        assert!(path.is_some(), "Current process should have an image path");
        let path = path.unwrap();
        assert!(path.exists());
        assert!(path.to_string_lossy().ends_with(".exe"));
    }

    /// 取消游戏会话的收尾全靠这条能力：`TrackedProcessHandle` 只有查询权限，
    /// 想停掉被跟踪的子进程必须走独立的 `terminate_process`。
    #[test]
    #[cfg(target_os = "windows")]
    fn test_terminate_process_kills_a_live_child() {
        let mut child = std::process::Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn helper process");
        let pid = child.id();
        assert!(is_process_running(pid), "辅助进程应处于运行状态");

        assert!(terminate_process(pid), "终止调用应报告成功");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while is_process_running(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!is_process_running(pid), "被终止的进程不应仍然存活");
        let _ = child.wait();
    }

    /// 自身与 PID 0 必须被拒绝：前者是自杀，后者必然失败。
    #[test]
    #[cfg(target_os = "windows")]
    fn test_terminate_process_refuses_self_and_idle() {
        assert!(!terminate_process(0));
        assert!(!terminate_process(std::process::id()));
    }
}
