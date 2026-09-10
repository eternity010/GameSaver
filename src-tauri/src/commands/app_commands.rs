use crate::{app_state::AppState, logging};
use tauri::{AppHandle, State};

/// 前端在用户确认「知悉存档风险、仍要退出」后调用。
///
/// 关闭请求本身由 `lib.rs` 的 `on_window_event` 拦截：只要有游戏在运行，
/// 它会 `prevent_close` 并推送 `app-exit-blocked`。用户若仍选择退出，前端
/// 调用本命令——先置位确认标记（保证后续关闭请求不再被拦），再结束进程。
///
/// 注意：这里**不会**提交任何存档。这正是提示存在的原因：一旦退出，
/// 承载会话的线程随进程消亡，本次游玩的存档不会被自动提交。
#[tauri::command]
pub fn confirm_app_exit(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    state.confirm_exit();
    logging::info(format!(
        "用户确认在 {} 个游戏运行时退出应用，本次会话的存档不会自动提交",
        state.running_game_count()
    ));
    app.exit(0);
    Ok(())
}
