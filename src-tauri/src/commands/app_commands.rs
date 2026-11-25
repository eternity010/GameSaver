use crate::{app_state::AppState, logging};
use tauri::{AppHandle, State};

/// 用户确认游戏会话或后台任务被中断的风险后退出。
#[tauri::command]
pub fn confirm_app_exit(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    state.confirm_exit();
    logging::info(format!(
        "用户确认退出应用：{} 个游戏正在运行，{} 项后台任务未结束",
        state.running_game_count(),
        state.active_background_task_count()
    ));
    #[cfg(target_os = "windows")]
    crate::services::window_placement::save_main_window(&app);
    app.exit(0);
    Ok(())
}
