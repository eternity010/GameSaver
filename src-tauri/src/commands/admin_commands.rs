use serde::Serialize;
use tauri::AppHandle;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ElevationStatus {
    pub is_admin: bool,
    pub can_restart_as_admin: bool,
}

#[tauri::command]
pub fn get_elevation_status() -> ElevationStatus {
    ElevationStatus {
        is_admin: is_running_as_admin(),
        can_restart_as_admin: cfg!(target_os = "windows"),
    }
}

#[tauri::command]
pub fn restart_as_admin(app: AppHandle) -> Result<(), String> {
    if is_running_as_admin() {
        return Ok(());
    }

    #[cfg(target_os = "windows")]
    {
        relaunch_as_admin()?;
        crate::logging::info("已请求管理员模式重启");
        app.exit(0);
        return Ok(());
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = app;
        Err("当前平台不支持管理员模式重启".to_string())
    }
}

#[cfg(target_os = "windows")]
fn is_running_as_admin() -> bool {
    use std::ptr::null_mut;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError, HANDLE},
        Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY},
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            crate::logging::error(format!("管理员权限检测失败：错误 {}", GetLastError()));
            return false;
        }

        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut returned_length = 0;
        let result = GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned_length,
        ) != 0;
        CloseHandle(token);
        result
            && returned_length >= std::mem::size_of::<TOKEN_ELEVATION>() as u32
            && elevation.TokenIsElevated != 0
    }
}

#[cfg(not(target_os = "windows"))]
fn is_running_as_admin() -> bool {
    false
}

#[cfg(target_os = "windows")]
fn relaunch_as_admin() -> Result<(), String> {
    use std::{ffi::OsStr, path::Path};
    use windows_sys::Win32::{
        Foundation::{GetLastError, HINSTANCE},
        UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
    };

    let executable =
        std::env::current_exe().map_err(|error| format!("获取应用路径失败：{error}"))?;
    let working_directory = executable.parent().unwrap_or_else(|| Path::new("."));
    let arguments = std::env::args_os()
        .skip(1)
        .map(quote_windows_argument)
        .collect::<Vec<_>>()
        .join(" ");
    let verb = wide(OsStr::new("runas"));
    let executable = wide(executable.as_os_str());
    let arguments = wide(OsStr::new(&arguments));
    let working_directory = wide(working_directory.as_os_str());

    let result: HINSTANCE = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            executable.as_ptr(),
            arguments.as_ptr(),
            working_directory.as_ptr(),
            SW_SHOWNORMAL,
        )
    };
    if (result as isize) <= 32 {
        return Err(format!("请求管理员模式重启失败：错误 {}", unsafe {
            GetLastError()
        }));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn wide(value: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(target_os = "windows")]
fn quote_windows_argument(value: std::ffi::OsString) -> String {
    let value = value.to_string_lossy();
    if value.is_empty() {
        return "\"\"".to_string();
    }
    if !value
        .chars()
        .any(|character| character.is_whitespace() || character == '"')
    {
        return value.into_owned();
    }

    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for character in value.chars() {
        if character == '\\' {
            backslashes += 1;
        } else if character == '"' {
            quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
            quoted.push(character);
            backslashes = 0;
        } else {
            quoted.push_str(&"\\".repeat(backslashes));
            quoted.push(character);
            backslashes = 0;
        }
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::quote_windows_argument;
    use std::ffi::OsString;

    #[test]
    fn quotes_arguments_with_spaces() {
        assert_eq!(
            quote_windows_argument(OsString::from("C:\\Game Files")),
            "\"C:\\Game Files\""
        );
    }
}
