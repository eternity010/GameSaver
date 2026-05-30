use crate::{
    learning,
    runtime,
    shared::RuntimeStatus,
};

#[tauri::command]
pub(crate) fn get_runtime_status() -> Result<RuntimeStatus, String> {
    let is_admin = learning::is_running_as_admin();
    let message = if is_admin {
        "running in elevated mode, ETW is available".to_string()
    } else {
        "running without elevation, learning will fall back to snapshot mode".to_string()
    };
    Ok(RuntimeStatus {
        is_admin,
        can_use_etw: is_admin,
        message,
    })
}

#[tauri::command]
pub(crate) fn restart_as_admin() -> Result<(), String> {
    runtime::restart_as_admin()
}
