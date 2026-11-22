use crate::{
    logging,
    repositories::store_file::{atomic_replace, load_with_recovery},
};
use serde::{Deserialize, Serialize};
use std::{mem::size_of, path::Path};
use tauri::{AppHandle, Manager, WebviewWindow};
use windows_sys::Win32::{
    Foundation::{HWND, RECT},
    UI::WindowsAndMessaging::{
        GetWindowPlacement, SetWindowPlacement, SW_HIDE, SW_SHOWMAXIMIZED, SW_SHOWNORMAL,
        WINDOWPLACEMENT,
    },
};

const FILE_NAME: &str = "window-placement.json";
const LABEL: &str = "主窗口位置";

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SavedPlacement {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    maximized: bool,
}

impl SavedPlacement {
    fn from_native(placement: WINDOWPLACEMENT) -> Self {
        let bounds = placement.rcNormalPosition;
        Self {
            left: bounds.left,
            top: bounds.top,
            right: bounds.right,
            bottom: bounds.bottom,
            maximized: placement.showCmd == SW_SHOWMAXIMIZED as u32,
        }
    }

    fn into_native(self) -> WINDOWPLACEMENT {
        WINDOWPLACEMENT {
            showCmd: if self.maximized {
                SW_SHOWMAXIMIZED
            } else {
                SW_SHOWNORMAL
            } as u32,
            rcNormalPosition: RECT {
                left: self.left,
                top: self.top,
                right: self.right,
                bottom: self.bottom,
            },
            ..new_placement()
        }
    }

    fn validate(&self) -> Result<(), String> {
        let width = self.right.checked_sub(self.left);
        let height = self.bottom.checked_sub(self.top);
        if [width, height]
            .into_iter()
            .all(|extent| extent.is_some_and(|extent| (100..=32_768).contains(&extent)))
        {
            Ok(())
        } else {
            Err("主窗口位置记录的尺寸无效".into())
        }
    }
}

fn new_placement() -> WINDOWPLACEMENT {
    WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    }
}

fn decode(bytes: &[u8]) -> Result<SavedPlacement, String> {
    let saved: SavedPlacement =
        serde_json::from_slice(bytes).map_err(|error| format!("读取主窗口位置失败：{error}"))?;
    saved.validate()?;
    Ok(saved)
}

fn load(data_dir: &Path) -> Result<Option<SavedPlacement>, String> {
    load_with_recovery(&data_dir.join(FILE_NAME), LABEL, |bytes| {
        decode(bytes).is_ok()
    })?
    .map(|bytes| decode(&bytes))
    .transpose()
}

fn persist(data_dir: &Path, placement: &SavedPlacement) -> Result<(), String> {
    placement.validate()?;
    let bytes =
        serde_json::to_vec(placement).map_err(|error| format!("编码主窗口位置失败：{error}"))?;
    atomic_replace(&data_dir.join(FILE_NAME), &bytes, LABEL)
}

fn window_handle(window: &WebviewWindow) -> Result<HWND, String> {
    window
        .hwnd()
        .map(|hwnd| hwnd.0 as HWND)
        .map_err(|error| format!("读取主窗口句柄失败：{error}"))
}

fn apply_saved_placement(
    saved: SavedPlacement,
    mut apply: impl FnMut(&WINDOWPLACEMENT) -> Result<(), String>,
) -> Result<(), String> {
    let mut placement = saved.into_native();
    let show_command = placement.showCmd;
    placement.showCmd = SW_HIDE as u32;
    apply(&placement)?;
    placement.showCmd = show_command;
    apply(&placement)
}

fn restore_and_show(window: &WebviewWindow, data_dir: &Path) -> Result<bool, String> {
    let Some(saved) = load(data_dir)? else {
        return Ok(false);
    };
    let hwnd = window_handle(window)?;
    apply_saved_placement(saved, |placement| {
        if unsafe { SetWindowPlacement(hwnd, placement) } == 0 {
            return Err(format!(
                "恢复主窗口位置失败：{}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    })?;
    Ok(true)
}

pub(crate) fn show_main_window(app: &AppHandle, data_dir: &Path) -> Result<(), String> {
    let window = app.get_webview_window("main").ok_or("找不到主窗口")?;
    match restore_and_show(&window, data_dir) {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        Err(error) => logging::warn(error),
    }
    window
        .show()
        .map_err(|error| format!("显示主窗口失败：{error}"))
}

pub(crate) fn save_main_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let result = (|| {
        let hwnd = window_handle(&window)?;
        let mut placement = new_placement();
        if unsafe { GetWindowPlacement(hwnd, &mut placement) } == 0 {
            return Err(format!(
                "获取主窗口位置失败：{}",
                std::io::Error::last_os_error()
            ));
        }
        let data_dir = app
            .path()
            .app_data_dir()
            .map_err(|error| format!("解析主窗口状态目录失败：{error}"))?;
        persist(&data_dir, &SavedPlacement::from_native(placement))
    })();
    if let Err(error) = result {
        logging::warn(error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempWorkspace;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWMINIMIZED;

    #[test]
    fn window_placement_round_trips_negative_monitor_coordinates() {
        let root = TempWorkspace::new("window-placement");
        assert!(load(&root).unwrap().is_none());
        let saved = SavedPlacement {
            left: -1440,
            top: -160,
            right: -240,
            bottom: 600,
            maximized: true,
        };
        persist(&root, &saved).unwrap();
        let reopened = load(&root).unwrap().unwrap();
        assert_eq!(reopened, saved);
        assert_eq!(reopened.into_native().showCmd, SW_SHOWMAXIMIZED as u32);
    }

    #[test]
    fn window_placement_reopens_a_minimized_window_at_its_normal_size() {
        let mut native = new_placement();
        native.showCmd = SW_SHOWMINIMIZED as u32;
        native.rcNormalPosition = RECT {
            left: 100,
            top: 200,
            right: 1300,
            bottom: 960,
        };
        let reopened = SavedPlacement::from_native(native).into_native();
        assert_eq!(reopened.showCmd, SW_SHOWNORMAL as u32);
        assert_eq!(
            reopened.rcNormalPosition.right - reopened.rcNormalPosition.left,
            1200
        );
        assert_eq!(
            reopened.rcNormalPosition.bottom - reopened.rcNormalPosition.top,
            760
        );
    }

    #[test]
    fn window_placement_rejects_corrupt_or_overflowing_geometry() {
        assert!(decode(b"not-json").is_err());
        assert!(
            decode(br#"{"left":4,"top":100,"right":0,"bottom":900,"maximized":false}"#).is_err()
        );
        assert!(decode(
            br#"{"left":-2147483648,"top":100,"right":2147483647,"bottom":900,"maximized":false}"#
        )
        .is_err());
    }

    #[test]
    fn window_placement_applies_saved_bounds_before_revealing_the_window() {
        for (maximized, show) in [(false, SW_SHOWNORMAL), (true, SW_SHOWMAXIMIZED)] {
            let saved = SavedPlacement {
                left: -1400,
                top: 160,
                right: -200,
                bottom: 920,
                maximized,
            };
            let mut commands = Vec::new();
            apply_saved_placement(saved, |placement| {
                let bounds = placement.rcNormalPosition;
                assert_eq!(
                    [bounds.left, bounds.top, bounds.right, bounds.bottom],
                    [-1400, 160, -200, 920]
                );
                commands.push(placement.showCmd);
                Ok(())
            })
            .unwrap();
            assert_eq!(commands, [SW_HIDE as u32, show as u32]);
        }
    }
}
