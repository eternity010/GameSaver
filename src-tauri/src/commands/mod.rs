pub mod add_game_commands;
pub mod admin_commands;
pub mod app_commands;
pub mod baidu_commands;
pub mod baidu_config_commands;
pub mod cloud_account_commands;
pub mod cloud_save_commands;
pub mod diagnostics_commands;
pub mod game_body_commands;
pub mod game_commands;
pub mod launch_commands;
pub mod library_commands;
pub mod save_commands;
pub mod save_version_commands;
pub mod task_commands;

/// 把命令里那点阻塞工作放到 tokio 的**阻塞线程池**上执行。
///
/// 为什么必须这样：`#[tauri::command]` 的同步版本在调用线程里直接执行函数体，而 IPC 是在
/// Tauri 主线程上进入的（tauri-macros `command/wrapper.rs` 的 `body_blocking`；WebView2 的
/// `add_WebMessageReceived` 回调在创建 webview 的线程上触发）。所以同步命令里任何一次网络
/// 往返都会让窗口消息循环与后续所有 IPC 一起停摆 —— 而 `baidu_netdisk_service` 的客户端
/// 连接超时 20 秒、总超时 120 秒，一次卡顿就是分钟量级。
///
/// 也不要只把命令声明成 `async` 就把阻塞调用留在体内：那会占住 tokio 的 worker 线程
/// （默认与 CPU 核数相同，是给异步任务用的），而不是专门为阻塞准备的池。统一走这里，
/// 命令体只留 `run_blocking(move || xxx_blocking(app)).await` 一行。
///
/// 审计依据：`docs/command-blocking-audit-2026-09-14.md` 的 F3。
pub(crate) async fn run_blocking<T, F>(work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|error| format!("后台执行失败：{error}"))?
}
