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

    /// 「该游戏有独占操作在进行」的共用 key。
    ///
    /// `save_operations` 是一张字符串集合，各处以不同前缀区分操作种类；**没有前缀的
    /// key 就是「整游戏独占」**，由本体更新、封面处理、保存版本维护与确保存档路径共用。
    /// 新增整游戏独占操作时必须用这个构造函数，别再手写 `game_uid.to_string()` ——
    /// 判定方式散成几份时，任一处加强都会被另一处绕过。
    pub fn game_operation_key(game_uid: &str) -> String {
        game_uid.trim().to_string()
    }

    /// 云端存档同步的独占 key。
    ///
    /// 刻意**不**复用整游戏独占 key：云端同步可以在游戏运行期间进行（手动上传、还原），
    /// 而本体更新那类操作会拒绝「游戏运行中」。两者共用一把 key 会让云端操作被
    /// `running_games` 那类无关判据挡掉。云端同步需要真正互斥的只有「同一游戏的另一次
    /// 云端同步」，因为云端清单是读-改-写。
    pub fn cloud_operation_key(game_uid: &str) -> String {
        format!("cloud:{}", game_uid.trim())
    }

    /// 该游戏是否已持有整游戏独占或云端同步的任意一把 key。
    ///
    /// 判定**不区分两把 key 的种类**：它们互斥，且每种操作一次只该有一个。因此
    /// 「集合里存在该游戏的任何操作」就等于「占用中」。
    fn any_operation_for(&self, operations: &HashSet<String>, game_uid: &str) -> bool {
        let game_uid = game_uid.trim();
        operations.contains(&Self::game_operation_key(game_uid))
            || operations.contains(&Self::cloud_operation_key(game_uid))
    }

    /// 认领一次整游戏独占操作。已被占用时返回 `Err`，调用方不该继续。
    ///
    /// 除整游戏独占外也拒绝**云端同步进行中**：两边会读写同一份本地存档目录（云端
    /// 上传/还原要打包或落盘存档，本体更新与封面处理要动受管目录与封面），必须互相
    /// 可见。判据放在插入**之前**：`insert` 会真的写进去，发现冲突再回滚等于制造一个
    /// 短暂的半认领状态，让并发调用看到「无冲突」。
    pub fn claim_operation(&self, game_uid: &str, busy: &str) -> Result<(), String> {
        let mut operations = self
            .save_operations
            .lock()
            .map_err(|_| "锁定游戏操作状态失败".to_string())?;
        if self.any_operation_for(&operations, game_uid) {
            return Err(busy.to_string());
        }
        operations.insert(Self::game_operation_key(game_uid));
        Ok(())
    }

    /// 认领该游戏的云端同步独占权。返回需要交给释放方的 key。
    ///
    /// 返回 key 而不是 RAII 凭据：凭据要活到工作线程结束，而 Tauri 命令签名里的
    /// `State<AppState>` 只是短命引用，借用它构造的凭据移不进线程（`state does not
    /// live long enough`）。工作线程本来就能用 `app.state::<AppState>()` 取到同一个
    /// `AppState`，于是沿用项目里既有的「认领 → 起线程 → 线程内释放」写法，
    /// 与 `save_version_commands::reserve_maintenance` 一致。
    pub fn claim_cloud_operation(&self, game_uid: &str) -> Result<String, String> {
        let mut operations = self
            .save_operations
            .lock()
            .map_err(|_| "锁定游戏操作状态失败".to_string())?;
        if self.any_operation_for(&operations, game_uid) {
            return Err("该游戏已有云端同步或本体操作正在进行".to_string());
        }
        let key = Self::cloud_operation_key(game_uid);
        operations.insert(key.clone());
        Ok(key)
    }

    /// 释放一次独占操作。幂等，且**不**因「没认领过」报错 —— 失败路径上重复释放
    /// 不该再抛一个掩盖真实原因的错误。
    pub fn release_operation(&self, key: &str) {
        if let Ok(mut operations) = self.save_operations.lock() {
            operations.remove(key);
        }
    }

    /// 该游戏是否有云端同步或整游戏独占操作在进行（自行取锁）。
    ///
    /// 持锁调用方直接把 `MutexGuard` 传进来即可 —— `&MutexGuard<HashSet<_>>` 会自动
    /// 解引用成 `&HashSet<_>`。之所以不另开一个「持锁版本」，是因为
    /// `save_operations` 是普通 `std::sync::Mutex`、不可重入：多一个长得像「安全版」
    /// 的方法，就多一次在持锁状态下误调自行取锁版本而自锁死的机会。
    pub fn has_exclusive_operation(&self, operations: &HashSet<String>, game_uid: &str) -> bool {
        self.any_operation_for(operations, game_uid)
    }

    /// 当前正在运行（含启动中 / 保存中）的游戏数量。
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

/// 一次云端操作的认领凭据：**在 `Drop` 里释放**。
///
/// 为什么需要它：`claim_cloud_operation` 返回的是一个字符串 key，认领与释放要调用方
/// 手写配对。C2 把这些认领加进 `cloud_save_commands.rs` 时，就漏掉了「认领之后、
/// 释放之前」的 `?` 早退 —— 那里有三处（凭据读取失败、云端清单取不到、指定版本不
/// 存在），任何一处命中都不会释放。而 `save_operations` 是**纯内存**集合、进程存活期
/// 内没有任何清理，于是那个游戏此后每一次云同步都返回「该游戏已有云端同步或本体
/// 操作正在进行」，**只有重启应用能解** —— 错误文案还把用户引向「稍后再试」。
/// 这与 C5（暂存目录残留导致永久装不上）是同一类后果。
///
/// 所以把释放交给 `Drop`：命令作用域内任何早退（含 `?`）都会释放，不必在每个失败
/// 分支上手写，也就不可能再漏。
///
/// **为什么持 `&AppState` 而不是 `AppHandle`**：持 `AppHandle` 才能把守卫移进工作
/// 线程，但那会让本文件依赖 `tauri::AppHandle` —— 而 `AppState` 被大量单元测试直接
/// 构造，测试路径一碰到 Tauri 类型就会把整条窗口运行时链进测试二进制并直接启动失败
/// （见本文件开头的 `EventEmitter` 注释）。所以守卫只活命令作用域，交接给线程时用
/// [`disarm`](Self::disarm) 把 key 交出去，由线程照旧在结束时释放。
///
/// `#[must_use]`：**把守卫当语句丢掉**（`CloudOperationClaim::claim(..)?;` 不绑定任何
/// 变量）会让它立刻 `Drop` —— 刚认领就释放，互斥**静默失效**，退回 C2 之前的丢条目
/// 竞态。这种写法必须报警告，而不是悄悄通过。
#[must_use = "认领守卫被丢弃就会立刻释放，互斥随之失效；请绑定它（let claim = ...）"]
pub struct CloudOperationClaim<'a> {
    state: &'a AppState,
    /// `disarm` 之后为空串，表示释放责任已交出去。
    key: String,
}

impl<'a> CloudOperationClaim<'a> {
    /// 认领该游戏的云端操作；已被占用时返回 `Err`，不产生守卫。
    pub fn claim(state: &'a AppState, game_uid: &str) -> Result<Self, String> {
        let key = state.claim_cloud_operation(game_uid)?;
        Ok(Self { state, key })
    }

    /// 交出释放责任，并**把当初认领的那把 key 原样返回**，由工作线程在结束时释放。
    ///
    /// 返回 key 而不是让线程用 `cloud_operation_key` 重新推导：这样「释放的」与
    /// 「认领的」在类型上就是同一个值，读的人不必去核对两处推导是否一致。
    pub fn disarm(mut self) -> String {
        std::mem::take(&mut self.key)
    }
}

impl Drop for CloudOperationClaim<'_> {
    fn drop(&mut self) {
        if !self.key.is_empty() {
            self.state.release_operation(&self.key);
        }
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

    /// 走「持锁判定」那条路径问一次占用状态，与 `launch` 的调用形态一致。
    fn occupied(state: &AppState, game_uid: &str) -> bool {
        let operations = state.save_operations.lock().expect("锁定操作集合");
        state.has_exclusive_operation(&operations, game_uid)
    }

    /// 认领守卫必须在**任何**早退路径上释放。
    ///
    /// 这是 C2 带进来的回归：`cloud_save_commands.rs` 里认领之后有三处 `?` 早退漏了
    /// `release_operation`，泄漏的 key 让该游戏在此进程内再也发不起云同步 —— 而
    /// `save_operations` 是纯内存，只有重启才清。
    #[test]
    fn a_claim_guard_releases_even_when_the_command_returns_early() {
        let state = test_state();

        // 模拟命令体：认领之后立刻用 `?` 早退（真实场景是清单取不到 / 版本不存在）。
        fn command_body(state: &AppState) -> Result<(), String> {
            let _claim = CloudOperationClaim::claim(state, "uid-1")?;
            Err("云端清单取不到".to_string())?;
            Ok(())
        }

        assert!(command_body(&state).is_err());
        assert!(
            !occupied(&state, "uid-1"),
            "早退后必须已释放，否则该游戏的云同步会被永久挡住"
        );
        // 释放干净之后必须能重新认领 —— 这才是用户重试能成功的判据。
        state
            .claim_cloud_operation("uid-1")
            .expect("守卫释放后应当能再次认领");
    }

    /// `disarm` 交出 key 之后，守卫自己**不能再**释放。
    ///
    /// 否则工作线程还在跑、key 就被守卫提前放掉，另一个同步会挤进来 —— 那正是 C2 要防的
    /// 并发丢条目。所以这里要同时验证两件事：守卫 `Drop` 后仍被持有，且交出的 key 能正常
    /// 释放（否则线程结束时的释放就成了空操作，key 反而泄漏）。
    #[test]
    fn a_disarmed_claim_guard_hands_the_key_over_instead_of_releasing_it() {
        let state = test_state();
        let key = CloudOperationClaim::claim(&state, "uid-1")
            .expect("首次认领应当成功")
            .disarm();

        assert!(occupied(&state, "uid-1"), "disarm 之后 key 必须仍然被持有");
        assert!(
            state.claim_cloud_operation("uid-1").is_err(),
            "disarm 后仍应挡住第二次操作"
        );

        // 交出的 key 必须就是当初认领的那把：用它释放后要能重新认领。
        state.release_operation(&key);
        assert!(!occupied(&state, "uid-1"), "交出的 key 应当能释放掉占用");
        state
            .claim_cloud_operation("uid-1")
            .expect("释放后应当能再次认领");
    }

    /// C2 的核心：同一游戏的两次云端同步必须互斥。
    ///
    /// 云端清单是「读-改-写」，两次并发会各自读到同一份旧清单、后写的那次把先写的
    /// 条目整条丢掉。退出游戏时的自动同步与用户手动同步正是可以并发的两条路径。
    #[test]
    fn a_second_cloud_sync_for_the_same_game_is_rejected() {
        let state = test_state();
        let first = state
            .claim_cloud_operation("uid-1")
            .expect("首次认领应当成功");

        let error = state
            .claim_cloud_operation("uid-1")
            .expect_err("同一游戏的第二次云端同步必须被拒绝");
        assert_eq!(error, "该游戏已有云端同步或本体操作正在进行");

        // 释放后必须能再次认领，否则一次失败就会把该游戏的云端功能永久锁死。
        state.release_operation(&first);
        state
            .claim_cloud_operation("uid-1")
            .expect("释放后应当可以重新认领");
    }

    /// 不同游戏之间不能互相阻塞。
    #[test]
    fn cloud_sync_claims_are_per_game() {
        let state = test_state();
        let first = state
            .claim_cloud_operation("uid-1")
            .expect("uid-1 认领应当成功");
        state
            .claim_cloud_operation("uid-2")
            .expect("另一个游戏的云端同步不该被 uid-1 挡住");
        assert!(occupied(&state, "uid-1"));
        assert!(occupied(&state, "uid-2"));
        state.release_operation(&first);
        assert!(!occupied(&state, "uid-1"));
        assert!(occupied(&state, "uid-2"), "释放一个不该影响另一个");
    }

    /// 云端同步与整游戏独占**互相排斥**，且判定要同时覆盖两把 key。
    ///
    /// 为什么必须是两把不同的 key：云端同步可以在游戏运行期间进行（手动上传、还原），
    /// 而本体更新那类操作会拒绝「游戏运行中」；共用一把 key 会让云端操作被
    /// `running_games` 那类无关判据挡掉。
    ///
    /// 为什么又必须互相可见：两边会读写同一份本地存档目录 —— 云端上传要打包存档、
    /// 本体更新要动受管目录。所以任意一方在进行时，另一方都该被拒绝。
    #[test]
    fn exclusive_operations_and_cloud_sync_block_each_other() {
        let state = test_state();

        // 整游戏独占在先 → 云端同步被拒。
        state
            .claim_operation("uid-1", "占用")
            .expect("整游戏独占认领应当成功");
        assert!(occupied(&state, "uid-1"));
        assert!(!occupied(&state, "uid-2"), "其他游戏不受影响");
        assert!(
            state.claim_cloud_operation("uid-1").is_err(),
            "整游戏独占进行中不该放行云端同步"
        );

        // 双向：释放后再让云端先占，整游戏独占也必须被拒。
        state.release_operation(&AppState::game_operation_key("uid-1"));
        assert!(!occupied(&state, "uid-1"));
        let cloud_key = state
            .claim_cloud_operation("uid-1")
            .expect("云端同步认领应当成功");
        assert!(occupied(&state, "uid-1"));
        assert!(
            state.claim_operation("uid-1", "占用").is_err(),
            "云端同步进行中不该放行本体/封面操作"
        );

        state.release_operation(&cloud_key);
        assert!(!occupied(&state, "uid-1"), "全部释放后应恢复空闲");
        state
            .claim_operation("uid-1", "占用")
            .expect("释放后整游戏独占应当可以认领");
    }

    /// 持锁判定（启动游戏那条路径的形态）与 `claim_*` 的结论必须一致。
    ///
    /// `has_exclusive_operation` 刻意只提供「传 guard 进来」这一种形态：`save_operations`
    /// 是普通 `Mutex`、不可重入，多一个自行取锁的同名方法就多一次自锁死的机会。
    #[test]
    fn locked_view_agrees_with_claims() {
        let state = test_state();
        assert!(!occupied(&state, "uid-1"), "初始应空闲");

        state
            .claim_operation("uid-1", "占用")
            .expect("整游戏独占认领应当成功");
        assert!(occupied(&state, "uid-1"), "认领后持锁判定必须为真");
        assert!(!occupied(&state, "uid-2"), "另一个游戏不该被判为占用");

        state.release_operation(&AppState::game_operation_key("uid-1"));
        assert!(!occupied(&state, "uid-1"));
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
