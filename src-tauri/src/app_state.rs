use crate::domain::{ActiveLearningSession, AppStore, AppTask, GameRuntime};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};

/// 事件推送器：由 `lib.rs` 在 setup 阶段注入，内部转调 `AppHandle::emit`。
///
/// 刻意声明成装箱函数指针、而不是直接持有 `tauri::AppHandle`：`AppState` 被大量
/// 单元测试直接构造，只要测试路径上碰到 `AppHandle` / `Emitter`，链接器就会把整条
/// Tauri 窗口运行时拉进测试二进制（多出 user32 / gdi32 / comctl32 / uxtheme 等
/// 10 个 GUI 系统依赖），测试二进制会直接启动失败（0xc0000139）。注入点放在只有
/// `run()` 才会执行的 setup 里，测试路径便完全不涉及 Tauri。
pub type EventEmitter = Box<dyn Fn(&str, serde_json::Value) + Send + Sync>;

pub struct AppState {
    pub store: Mutex<AppStore>,
    pub library_root: Mutex<PathBuf>,
    pub tasks: Mutex<HashMap<String, AppTask>>,
    pub tasks_path: PathBuf,
    pub learning_sessions: Mutex<HashMap<String, ActiveLearningSession>>,
    pub running_games: Mutex<HashMap<String, GameRuntime>>,
    pub save_operations: Mutex<HashSet<String>>,
    pub library_migration: Mutex<bool>,
    pub cloud_account_sync: Mutex<bool>,
    /// 是否已由用户确认「游戏运行中仍要退出应用」。
    ///
    /// 关闭请求的拦截在 `lib.rs` 的 `on_window_event` 里完成：只要有游戏在
    /// 运行就 `prevent_close` 并提示。用户确认后由 `confirm_app_exit` 置位
    /// 此标记，之后的关闭请求直接放行。一旦置位不撤销——它只在退出前一刻设置。
    exit_confirmed: AtomicBool,
    /// 应用级事件推送器，setup 阶段注入（见 [`EventEmitter`]）。
    ///
    /// 放在 `AppState` 而不是全局静态量里，是为了让服务层在被注入 `&AppState`
    /// 时就能推送状态变化；单元测试不注入，`broadcast` 会静默跳过。
    event_emitter: Mutex<Option<EventEmitter>>,
}

impl AppState {
    pub fn new(
        store: AppStore,
        library_root: PathBuf,
        tasks: HashMap<String, AppTask>,
        tasks_path: PathBuf,
    ) -> Self {
        Self {
            store: Mutex::new(store),
            library_root: Mutex::new(library_root),
            tasks: Mutex::new(tasks),
            tasks_path,
            learning_sessions: Mutex::new(HashMap::new()),
            running_games: Mutex::new(HashMap::new()),
            save_operations: Mutex::new(HashSet::new()),
            library_migration: Mutex::new(false),
            cloud_account_sync: Mutex::new(false),
            exit_confirmed: AtomicBool::new(false),
            event_emitter: Mutex::new(None),
        }
    }

    /// 在**全程持有 store 锁**的前提下执行一次「读 → 改 → 持久化 → 提交」。
    ///
    /// 此前大量代码写成「`lock()?.clone()` 取快照 → 释放锁 → 计算/落盘 →
    /// 再 `lock()?` 整体覆盖写回」。两次加锁之间只要别的写入者提交过，前者
    /// 就会被静默覆盖（丢失更新）。这里把整个序列收进一次加锁，从根上消除竞态。
    ///
    /// - `mutate` 拿到当前 store 的克隆，可自由增删改；
    /// - 返回 `Ok(value)` 时把改动提交回内存（`*store = candidate`）并返回 `value`；
    /// - 返回 `Err` 时不提交，内存态保持原样，错误原样透传。
    ///
    /// 注意：`mutate` 在锁内执行，**不得**再访问 `self.store`（会自锁死），
    /// 也应避免在其中做长耗时的网络请求或多轮全盘扫描，否则会阻塞其他读写者。
    pub fn with_store_mut<T>(
        &self,
        mutate: impl FnOnce(&mut AppStore) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut guard = self
            .store
            .lock()
            .map_err(|_| "锁定 GameSaver 数据失败".to_string())?;
        let mut candidate = guard.clone();
        let value = mutate(&mut candidate)?;
        *guard = candidate;
        Ok(value)
    }

    /// 当前正在运行（含启动中 / 保存中）的游戏数量。
    ///
    /// 锁中毒时按 0 处理：这只影响「是否拦截退出」的判定，不应让关闭流程
    /// 因为一次计数读取失败而卡住。
    pub fn running_game_count(&self) -> usize {
        self.running_games
            .lock()
            .map(|games| games.len())
            .unwrap_or_default()
    }

    /// 用户是否已确认「知悉存档风险、仍要退出应用」。
    pub fn exit_confirmed(&self) -> bool {
        self.exit_confirmed.load(Ordering::SeqCst)
    }

    /// 记录用户的退出确认。置位后所有关闭请求直接放行。
    pub fn confirm_exit(&self) {
        self.exit_confirmed.store(true, Ordering::SeqCst);
    }

    /// 注入事件推送器。setup 阶段调用一次；重复调用以最后一次为准。
    pub fn attach_event_emitter(&self, emitter: EventEmitter) {
        if let Ok(mut guard) = self.event_emitter.lock() {
            *guard = Some(emitter);
        }
    }

    /// 向所有窗口广播一个事件。
    ///
    /// 未注入推送器（单元测试）或序列化/发送失败时静默跳过或仅记日志：事件推送是
    /// 尽力而为的优化，任何一次失败都不应影响业务逻辑本身的正确性。
    pub(crate) fn broadcast<S>(&self, event: &str, payload: S)
    where
        S: serde::Serialize,
    {
        let Ok(guard) = self.event_emitter.lock() else {
            return;
        };
        let Some(emitter) = guard.as_ref() else {
            return;
        };
        match serde_json::to_value(payload) {
            Ok(value) => emitter(event, value),
            Err(error) => crate::logging::error(format!("序列化事件 {event} 失败：{error}")),
        }
    }

    /// 读取某游戏的运行时状态快照。
    pub fn runtime_of(&self, game_uid: &str) -> Option<GameRuntime> {
        self.running_games
            .lock()
            .ok()
            .and_then(|games| games.get(game_uid).cloned())
    }

    /// 开始一场会话：仅当该游戏尚未运行时写入，检查与写入在同一次加锁内完成。
    pub fn begin_runtime(&self, runtime: GameRuntime) -> Result<(), String> {
        let game_uid = runtime.game_uid.clone();
        {
            let mut games = self
                .running_games
                .lock()
                .map_err(|_| "lock running game state failed".to_string())?;
            if games.contains_key(&game_uid) {
                return Err("游戏已经在运行".to_string());
            }
            games.insert(game_uid.clone(), runtime);
        }
        crate::events::runtime_changed(self, &game_uid);
        Ok(())
    }

    /// 写入（或整体替换）某游戏的运行时状态，并推送变化。
    pub fn set_runtime(&self, runtime: GameRuntime) {
        let game_uid = runtime.game_uid.clone();
        if let Ok(mut games) = self.running_games.lock() {
            games.insert(game_uid.clone(), runtime);
        }
        crate::events::runtime_changed(self, &game_uid);
    }

    /// 就地推进某游戏的运行时状态（不存在则不做任何事），并推送变化。
    pub fn update_runtime(&self, game_uid: &str, mutate: impl FnOnce(&mut GameRuntime)) {
        let changed = match self.running_games.lock() {
            Ok(mut games) => match games.get_mut(game_uid) {
                Some(runtime) => {
                    mutate(runtime);
                    true
                }
                None => false,
            },
            Err(_) => false,
        };
        if changed {
            crate::events::runtime_changed(self, game_uid);
        }
    }

    /// 结束某游戏的运行时状态（不存在则静默），并推送变化。幂等。
    pub fn remove_runtime(&self, game_uid: &str) {
        let removed = self
            .running_games
            .lock()
            .map(|mut games| games.remove(game_uid).is_some())
            .unwrap_or(false);
        if removed {
            crate::events::runtime_changed(self, game_uid);
        }
    }

    pub fn library_root_path(&self) -> Result<PathBuf, String> {
        self.library_root
            .lock()
            .map(|path| path.clone())
            .map_err(|_| "读取游戏库根目录失败".to_string())
    }

    pub fn games_root(&self) -> Result<PathBuf, String> {
        Ok(self.library_root_path()?.join("games"))
    }

    pub fn body_packages_root(&self) -> Result<PathBuf, String> {
        Ok(self.library_root_path()?.join("body-packages"))
    }

    pub fn saves_root(&self) -> Result<PathBuf, String> {
        Ok(self.library_root_path()?.join("saves"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Game, GameRuntime, GameRuntimeStatus};

    fn test_state() -> AppState {
        AppState::new(
            AppStore::default(),
            PathBuf::from("."),
            HashMap::new(),
            PathBuf::from("."),
        )
    }

    fn sample_game(name: &str) -> Game {
        Game::new_pending(name, "C:/games/sample", "game.exe")
    }

    #[test]
    fn with_store_mut_commits_the_mutation_on_success() {
        let state = test_state();
        let value = state
            .with_store_mut(|store| {
                store.games.push(sample_game("A"));
                Ok(7usize)
            })
            .expect("with_store_mut should succeed");
        assert_eq!(value, 7);
        assert_eq!(state.store.lock().unwrap().games.len(), 1);
    }

    #[test]
    fn with_store_mut_discards_the_mutation_on_error() {
        let state = test_state();
        let error = state
            .with_store_mut::<()>(|store| {
                store.games.push(sample_game("B"));
                Err("boom".to_string())
            })
            .expect_err("with_store_mut should propagate the error");
        assert_eq!(error, "boom");
        assert!(
            state.store.lock().unwrap().games.is_empty(),
            "闭包返回 Err 时不得提交任何改动"
        );
    }

    /// 这条是这次修复的核心契约：锁必须覆盖整个闭包，而不是只覆盖 clone。
    /// 否则「取快照 → 释放锁 → 再整体覆盖写回」之间仍会丢失并发更新。
    #[test]
    fn with_store_mut_holds_the_lock_for_the_whole_closure() {
        let state = test_state();
        state
            .with_store_mut(|_store| {
                assert!(
                    state.store.try_lock().is_err(),
                    "闭包执行期间 store 必须处于加锁状态"
                );
                Ok(())
            })
            .expect("with_store_mut should succeed");
        assert!(state.store.try_lock().is_ok(), "闭包结束后必须释放锁");
    }

    /// 退出拦截的判定完全依赖这两个状态：运行中游戏数量、用户确认标记。
    #[test]
    fn exit_guard_tracks_running_games_and_confirmation() {
        let state = test_state();
        assert_eq!(state.running_game_count(), 0, "空闲时不应拦截退出");
        assert!(!state.exit_confirmed(), "初始状态不得视为已确认退出");

        state.running_games.lock().unwrap().insert(
            "uid-1".to_string(),
            GameRuntime {
                game_uid: "uid-1".to_string(),
                status: GameRuntimeStatus::Running,
                pid: Some(4321),
                started_at: None,
                task_id: Some("task-1".to_string()),
            },
        );
        assert_eq!(state.running_game_count(), 1, "有游戏运行时必须拦截退出");

        state.confirm_exit();
        assert!(state.exit_confirmed(), "确认后必须放行退出");
    }

    fn running_runtime(game_uid: &str) -> GameRuntime {
        GameRuntime {
            game_uid: game_uid.to_string(),
            status: GameRuntimeStatus::Running,
            pid: Some(1234),
            started_at: None,
            task_id: None,
        }
    }

    /// 会话的「开始」必须是原子的：检查与写入在同一次加锁内完成，否则同一游戏
    /// 可以被并发拉起两次——而 `running_games` 正是退出拦截的唯一判据。
    #[test]
    fn begin_runtime_rejects_a_second_session() {
        let state = test_state();
        state
            .begin_runtime(running_runtime("uid-1"))
            .expect("首个会话应当成功");
        let error = state
            .begin_runtime(running_runtime("uid-1"))
            .expect_err("同一游戏的第二个会话必须被拒绝");
        assert_eq!(error, "游戏已经在运行");
        assert_eq!(state.running_game_count(), 1);
    }

    /// 运行时状态的读改删语义：未注入 AppHandle 时推送必须静默跳过（不能 panic），
    /// 而状态本身要正确、且删除幂等。
    #[test]
    fn runtime_helpers_stay_consistent_without_an_app_handle() {
        let state = test_state();
        state.set_runtime(running_runtime("uid-1"));
        assert_eq!(state.running_game_count(), 1);

        state.update_runtime("uid-1", |runtime| {
            runtime.status = GameRuntimeStatus::Saving;
            runtime.task_id = Some("task-1".to_string());
        });
        let runtime = state.runtime_of("uid-1").expect("runtime 应当存在");
        assert_eq!(runtime.status, GameRuntimeStatus::Saving);
        assert_eq!(runtime.task_id.as_deref(), Some("task-1"));

        // 更新不存在的游戏不得凭空创建。
        state.update_runtime("uid-missing", |runtime| {
            runtime.status = GameRuntimeStatus::Saving;
        });
        assert!(state.runtime_of("uid-missing").is_none());

        state.remove_runtime("uid-1");
        assert_eq!(state.running_game_count(), 0);
        state.remove_runtime("uid-1");
        assert_eq!(state.running_game_count(), 0, "重复移除必须幂等");
    }
}
