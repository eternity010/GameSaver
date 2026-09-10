//! 应用级事件推送：把「后端状态变化」主动送达前端。
//!
//! 在引入本模块之前，这一段完全没有通道：每个组件各自建定时器轮询
//! `listTasks` / `getGameRuntime`，周期与状态变化频率无关——详情页在整场游戏
//! 会话里以 700ms 轮询，而真正有意义的变化只有三次；根组件那份任务轮询最小
//! 周期 1s 且自续期，等于永不停止。同一份 `listTasks` 还被两个组件各拉一遍。
//!
//! 这里把「变化」推给前端，轮询降级为低频兜底。事件命名沿用项目既有约定
//! （kebab-case，参见 `cover-capture-ready` / `app-exit-blocked`）。
//!
//! 推送是**尽力而为**的：句柄未注入（单元测试）或发送失败时静默跳过，权威状态
//! 始终以后端的 `running_games` / 任务表为准，前端丢失一次事件只会晚一个兜底
//! 周期，不会读错状态。

use crate::{app_state::AppState, domain::GameRuntimeStatus};
use serde::Serialize;

/// 任务被创建、进度更新、结束或被删除。
pub const TASK_CHANGED: &str = "task-changed";

/// 某个游戏的运行时状态发生变化（开始运行 / 状态推进 / 结束）。
pub const RUNTIME_CHANGED: &str = "runtime-changed";

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskChangedPayload {
    /// 发生变更的任务 ID；批量删除时为空字符串。
    pub task_id: String,
    /// `created` / `updated` / `finished` / `deleted`，供前端做轻量判断，
    /// 真正需要的数据由前端重新拉取（本地读取，成本极低）。
    pub kind: &'static str,
    pub game_uid: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeChangedPayload {
    pub game_uid: String,
    /// 该游戏是否仍在运行（含启动中 / 保存中）。为 `false` 时后两项为空。
    pub running: bool,
    pub status: Option<GameRuntimeStatus>,
    pub task_id: Option<String>,
}

/// 广播一次任务变更。`state` 未持有 AppHandle 时静默跳过。
pub fn task_changed(state: &AppState, task_id: &str, kind: &'static str, game_uid: Option<&str>) {
    state.broadcast(
        TASK_CHANGED,
        TaskChangedPayload {
            task_id: task_id.to_string(),
            kind,
            game_uid: game_uid.map(str::to_string),
        },
    );
}

/// 广播某游戏的运行时状态。
///
/// 内容一律从 `running_games` 的当前值取，而不是由调用点各自拼装——否则
/// 「已移除」这种状态根本无处可拼，前端就会永久停在「运行中」。
pub fn runtime_changed(state: &AppState, game_uid: &str) {
    let payload = match state.runtime_of(game_uid) {
        Some(runtime) => RuntimeChangedPayload {
            game_uid: game_uid.to_string(),
            running: true,
            status: Some(runtime.status),
            task_id: runtime.task_id,
        },
        None => RuntimeChangedPayload {
            game_uid: game_uid.to_string(),
            running: false,
            status: None,
            task_id: None,
        },
    };
    state.broadcast(RUNTIME_CHANGED, payload);
}
