use crate::{
    domain::{AppStore, AppTask, GameRuntime, TaskCategory, TaskStatus},
    services::save_learning_service::ActiveLearningSession,
};
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
    pub(crate) learning_sessions: Mutex<HashMap<String, ActiveLearningSession>>,
    pub running_games: Mutex<HashMap<String, GameRuntime>>,
    pub save_operations: Mutex<HashSet<String>>,
    pub library_migration: Mutex<bool>,
    pub cloud_account_sync: Mutex<bool>,
    pub(crate) cloud_store_cache: crate::services::cloud_store_cache::CloudStoreCache,
    /// 用户是否已确认在游戏或后台任务未结束时退出。
    exit_confirmed: AtomicBool,
    /// 应用级事件推送器，setup 阶段注入（见 [`EventEmitter`]）。
    ///
    /// 放在 `AppState` 而不是全局静态量里，是为了让服务层在被注入 `&AppState`
    /// 时就能推送状态变化；单元测试不注入，`broadcast` 会静默跳过。
    event_emitter: Mutex<Option<EventEmitter>>,
}

impl AppState {
    pub(crate) const ACCOUNT_RESTORE_KEY: &'static str = "account:restore";
    pub(crate) const LIBRARY_MIGRATION_KEY: &'static str = "library:migration";

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
            cloud_store_cache: Default::default(),
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
    /// 云端上传与删除不修改本地存档目录，可以与本体打包并行。
    pub fn cloud_operation_key(game_uid: &str) -> String {
        format!("cloud:{}", game_uid.trim())
    }

    fn cloud_restore_operation_key(game_uid: &str) -> String {
        format!("cloud-restore:{}", game_uid.trim())
    }

    fn body_package_operation_key(game_uid: &str) -> String {
        format!("body-package:{}", game_uid.trim())
    }

    fn exclusive_or_pending_operation(operations: &HashSet<String>, game_uid: &str) -> bool {
        Self::account_restore_in_progress(operations)
            || Self::library_migration_in_progress(operations)
            || operations.contains(&Self::game_operation_key(game_uid))
            || operations.contains(&Self::removal_operation_key(game_uid))
            || operations.contains(&Self::update_operation_key(game_uid))
    }

    fn any_operation_for(&self, operations: &HashSet<String>, game_uid: &str) -> bool {
        Self::exclusive_or_pending_operation(operations, game_uid)
            || operations.contains(&Self::cloud_operation_key(game_uid))
            || operations.contains(&Self::cloud_restore_operation_key(game_uid))
            || operations.contains(&Self::body_package_operation_key(game_uid))
    }

    fn removal_operation_key(game_uid: &str) -> String {
        format!("removal:{}", game_uid.trim())
    }

    fn update_operation_key(game_uid: &str) -> String {
        format!("update:{}", game_uid.trim())
    }

    pub(crate) fn claim_game_add(&self, game_key: &str) -> Result<String, String> {
        let key = format!("add:{game_key}");
        let mut operations = self
            .save_operations
            .lock()
            .map_err(|_| "锁定游戏操作状态失败")?;
        if Self::account_restore_in_progress(&operations)
            || Self::library_migration_in_progress(&operations)
            || operations.contains(&key)
        {
            return Err("该游戏正在添加，或游戏库正在维护，请等待完成后再操作".into());
        }
        operations.insert(key.clone());
        Ok(key)
    }

    pub(crate) fn claim_learning_start(&self, game_uid: &str) -> Result<(), String> {
        let mut operations = self
            .save_operations
            .lock()
            .map_err(|_| "锁定游戏操作状态失败")?;
        if self.any_operation_for(&operations, game_uid) {
            return Err("该游戏已有操作正在进行，请勿重复启动识别".into());
        }
        if self
            .running_games
            .lock()
            .map_err(|_| "读取游戏运行状态失败")?
            .contains_key(game_uid)
            || self
                .learning_sessions
                .lock()
                .map_err(|_| "读取存档识别状态失败")?
                .values()
                .any(|session| session.view.game_uid == game_uid)
        {
            return Err("请先关闭游戏并结束当前存档识别".into());
        }
        operations.insert(Self::game_operation_key(game_uid));
        Ok(())
    }

    pub(crate) fn claim_game_update(&self, game_uid: &str, resume: bool) -> Result<(), String> {
        let mut operations = self
            .save_operations
            .lock()
            .map_err(|_| "锁定游戏操作状态失败")?;
        if Self::account_restore_in_progress(&operations)
            || Self::library_migration_in_progress(&operations)
            || operations.contains(&Self::game_operation_key(game_uid))
            || operations.contains(&Self::cloud_operation_key(game_uid))
            || operations.contains(&Self::cloud_restore_operation_key(game_uid))
            || operations.contains(&Self::body_package_operation_key(game_uid))
            || operations.contains(&Self::removal_operation_key(game_uid))
            || (!resume && operations.contains(&Self::update_operation_key(game_uid)))
        {
            return Err("该游戏已有操作或更新事务待恢复，请先完成后再操作".into());
        }
        self.ensure_body_operation_idle(game_uid)?;
        operations.insert(Self::game_operation_key(game_uid));
        Ok(())
    }

    pub(crate) fn claim_body_package(&self, game_uid: &str) -> Result<String, String> {
        let game_uid = game_uid.trim();
        let mut operations = self
            .save_operations
            .lock()
            .map_err(|_| "锁定游戏操作状态失败")?;
        let key = Self::body_package_operation_key(game_uid);
        if Self::exclusive_or_pending_operation(&operations, game_uid)
            || operations.contains(&Self::cloud_restore_operation_key(game_uid))
            || operations.contains(&key)
        {
            return Err("该游戏正在操作游戏本体、恢复存档或维护游戏库，请稍后再试".into());
        }
        self.ensure_body_operation_idle(game_uid)?;
        operations.insert(key.clone());
        Ok(key)
    }

    fn ensure_body_operation_idle(&self, game_uid: &str) -> Result<(), String> {
        if self
            .running_games
            .lock()
            .map_err(|_| "读取游戏运行状态失败")?
            .contains_key(game_uid)
            || self
                .learning_sessions
                .lock()
                .map_err(|_| "读取存档识别状态失败")?
                .values()
                .any(|session| session.view.game_uid == game_uid)
        {
            return Err("请先关闭游戏并结束存档识别，再操作游戏本体".into());
        }
        Ok(())
    }

    pub(crate) fn finish_game_update(&self, game_uid: &str, pending: bool) {
        if let Ok(mut operations) = self.save_operations.lock() {
            if pending {
                operations.insert(Self::update_operation_key(game_uid));
            } else {
                operations.remove(&Self::update_operation_key(game_uid));
            }
            operations.remove(&Self::game_operation_key(game_uid));
        }
    }

    pub(crate) fn claim_game_removal(&self, game_uid: &str, resume: bool) -> Result<(), String> {
        let mut operations = self
            .save_operations
            .lock()
            .map_err(|_| "锁定游戏操作状态失败")?;
        if Self::account_restore_in_progress(&operations)
            || Self::library_migration_in_progress(&operations)
            || operations.contains(&Self::game_operation_key(game_uid))
            || operations.contains(&Self::cloud_operation_key(game_uid))
            || operations.contains(&Self::cloud_restore_operation_key(game_uid))
            || operations.contains(&Self::body_package_operation_key(game_uid))
            || operations.contains(&Self::update_operation_key(game_uid))
            || (!resume && operations.contains(&Self::removal_operation_key(game_uid)))
        {
            return Err("该游戏有操作或待恢复的删除事务，请先完成后再操作".into());
        }
        if self
            .running_games
            .lock()
            .map_err(|_| "读取游戏运行状态失败")?
            .contains_key(game_uid)
        {
            return Err("请先关闭游戏，再卸载或删除本体".into());
        }
        if self
            .learning_sessions
            .lock()
            .map_err(|_| "读取存档识别状态失败")?
            .values()
            .any(|session| session.view.game_uid == game_uid)
        {
            return Err("请先结束存档识别，再卸载或删除游戏".into());
        }
        operations.insert(Self::game_operation_key(game_uid));
        Ok(())
    }

    pub(crate) fn finish_game_removal(&self, game_uid: &str, pending: bool) {
        if let Ok(mut operations) = self.save_operations.lock() {
            if pending {
                operations.insert(Self::removal_operation_key(game_uid));
            } else {
                operations.remove(&Self::removal_operation_key(game_uid));
            }
            operations.remove(&Self::game_operation_key(game_uid));
        }
    }

    pub(crate) fn account_restore_in_progress(operations: &HashSet<String>) -> bool {
        operations.contains(Self::ACCOUNT_RESTORE_KEY)
    }

    pub(crate) fn library_migration_in_progress(operations: &HashSet<String>) -> bool {
        operations.contains(Self::LIBRARY_MIGRATION_KEY)
    }

    pub(crate) fn claim_library_migration(&self, resume: bool) -> Result<(), String> {
        let mut operations = self
            .save_operations
            .lock()
            .map_err(|_| "锁定游戏操作状态失败".to_string())?;
        let resuming =
            resume && operations.len() == 1 && Self::library_migration_in_progress(&operations);
        if !operations.is_empty() && !resuming {
            return Err("游戏或存档操作进行中，暂时不能迁移游戏库".to_string());
        }
        if !self
            .running_games
            .lock()
            .map_err(|_| "读取游戏运行状态失败")?
            .is_empty()
        {
            return Err("请先关闭游戏，再迁移游戏库".to_string());
        }
        if !self
            .learning_sessions
            .lock()
            .map_err(|_| "读取存档识别状态失败")?
            .is_empty()
        {
            return Err("请先结束存档识别，再迁移游戏库".to_string());
        }
        if self
            .tasks
            .lock()
            .map_err(|_| "读取后台任务失败")?
            .values()
            .any(|task| {
                matches!(
                    task.status,
                    crate::domain::TaskStatus::Pending | crate::domain::TaskStatus::Running
                )
            })
        {
            return Err("请等待其他后台任务结束，再迁移游戏库".to_string());
        }
        let mut migrating = self
            .library_migration
            .lock()
            .map_err(|_| "读取游戏库迁移状态失败")?;
        if *migrating {
            return Err("已有游戏库迁移任务正在进行".to_string());
        }
        *migrating = true;
        operations.insert(Self::LIBRARY_MIGRATION_KEY.to_string());
        Ok(())
    }

    pub(crate) fn finish_library_migration(&self, retain_block: bool) {
        if let Ok(mut operations) = self.save_operations.lock() {
            if let Ok(mut migrating) = self.library_migration.lock() {
                *migrating = false;
                if retain_block {
                    operations.insert(Self::LIBRARY_MIGRATION_KEY.to_string());
                } else {
                    operations.remove(Self::LIBRARY_MIGRATION_KEY);
                }
            }
        }
    }

    pub(crate) fn claim_account_restore(&self) -> Result<(), String> {
        let mut operations = self
            .save_operations
            .lock()
            .map_err(|_| "锁定游戏操作状态失败".to_string())?;
        if !operations.is_empty() {
            return Err("游戏或存档操作进行中，暂时不能恢复云端档案".to_string());
        }
        if *self
            .library_migration
            .lock()
            .map_err(|_| "读取游戏库迁移状态失败".to_string())?
        {
            return Err("游戏库迁移中，暂时不能恢复云端档案".to_string());
        }
        if !self
            .running_games
            .lock()
            .map_err(|_| "读取游戏运行状态失败".to_string())?
            .is_empty()
        {
            return Err("请先关闭游戏，再恢复云端档案".to_string());
        }
        if !self
            .learning_sessions
            .lock()
            .map_err(|_| "读取存档识别状态失败".to_string())?
            .is_empty()
        {
            return Err("存档识别进行中，暂时不能恢复云端档案".to_string());
        }
        if self
            .tasks
            .lock()
            .map_err(|_| "读取后台任务失败".to_string())?
            .values()
            .any(|task| {
                matches!(
                    task.status,
                    crate::domain::TaskStatus::Pending | crate::domain::TaskStatus::Running
                )
            })
        {
            return Err("请等待其他后台任务结束，再恢复云端档案".to_string());
        }
        operations.insert(Self::ACCOUNT_RESTORE_KEY.to_string());
        Ok(())
    }

    /// 认领一次整游戏独占操作。已被占用时返回 `Err`，调用方不该继续。
    ///
    /// 在同一锁内检查现有认领，再插入整游戏独占 key。
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
        self.claim_cloud_save(game_uid, false)
    }

    fn claim_cloud_save(&self, game_uid: &str, restore: bool) -> Result<String, String> {
        let mut operations = self
            .save_operations
            .lock()
            .map_err(|_| "锁定游戏操作状态失败".to_string())?;
        if Self::exclusive_or_pending_operation(&operations, game_uid)
            || operations.contains(&Self::cloud_operation_key(game_uid))
            || operations.contains(&Self::cloud_restore_operation_key(game_uid))
            || (restore && operations.contains(&Self::body_package_operation_key(game_uid)))
        {
            return Err("该游戏已有云端同步或本体操作正在进行".to_string());
        }
        let key = if restore {
            Self::cloud_restore_operation_key(game_uid)
        } else {
            Self::cloud_operation_key(game_uid)
        };
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

    pub fn ensure_save_restore_idle(&self, game_uid: &str) -> Result<(), String> {
        let sessions = self
            .learning_sessions
            .lock()
            .map_err(|_| "锁定存档识别状态失败".to_string())?;
        if sessions
            .values()
            .any(|session| session.view.game_uid == game_uid)
        {
            return Err("游戏正在进行存档识别，不能恢复存档".to_string());
        }
        drop(sessions);
        let running = self
            .running_games
            .lock()
            .map_err(|_| "锁定游戏运行状态失败".to_string())?;
        if running.contains_key(game_uid) {
            return Err("请先关闭游戏，再恢复云端存档".to_string());
        }
        Ok(())
    }

    /// 该游戏是否有会阻止启动的操作在进行。
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

    pub(crate) fn invalidate_cloud_store(&self, reset: bool) {
        self.cloud_store_cache.invalidate();
        self.broadcast(
            "cloud-store-invalidated",
            serde_json::json!({ "reset": reset }),
        );
    }

    pub fn active_background_task_count(&self) -> usize {
        let tasks = self.tasks.lock().unwrap_or_else(|error| error.into_inner());
        tasks
            .values()
            .filter(|task| {
                matches!(task.status, TaskStatus::Pending | TaskStatus::Running)
                    && task
                        .category
                        .unwrap_or_else(|| TaskCategory::infer(&task.task_type))
                        != TaskCategory::Session
            })
            .count()
    }

    /// 用户是否已确认退出风险。
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
    pub fn claim_restore(state: &'a AppState, game_uid: &str) -> Result<Self, String> {
        let key = state.claim_cloud_save(game_uid, true)?;
        let claim = Self { state, key };
        state.ensure_save_restore_idle(game_uid)?;
        Ok(claim)
    }
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
    fn adding_key_is_reserved_until_worker_finishes() {
        let state = test_state();
        let key = state.claim_game_add("same-game").unwrap();
        assert!(state.claim_game_add("same-game").is_err());
        assert!(state.claim_library_migration(false).is_err());
        assert!(state.claim_account_restore().is_err());
        let other = state.claim_game_add("other-game").unwrap();
        state.release_operation(&other);
        state.release_operation(&key);
        state.claim_game_add("same-game").unwrap();
    }

    #[test]
    fn learning_start_reservation_blocks_reentry_removal_and_global_maintenance() {
        let state = test_state();
        state.claim_learning_start("game-a").unwrap();
        assert!(state.claim_learning_start("game-a").is_err());
        assert!(state.claim_game_removal("game-a", false).is_err());
        assert!(state.claim_game_update("game-a", false).is_err());
        assert!(state.claim_library_migration(false).is_err());
        assert!(state.claim_account_restore().is_err());
        state.release_operation("game-a");
        state.claim_game_removal("game-a", false).unwrap();
    }

    #[test]
    fn pending_removal_blocks_game_cloud_and_global_operations_until_retry_finishes() {
        let state = test_state();
        state.finish_game_removal("game-a", true);
        assert!(state.claim_operation("game-a", "busy").is_err());
        assert!(state.claim_cloud_operation("game-a").is_err());
        assert!(state.claim_game_removal("game-a", false).is_err());
        assert!(state.claim_account_restore().is_err());
        assert!(state.claim_library_migration(false).is_err());
        assert!(state.has_exclusive_operation(&state.save_operations.lock().unwrap(), "game-a"));
        state.claim_game_removal("game-a", true).unwrap();
        assert!(state.claim_game_removal("game-a", true).is_err());
        state.finish_game_removal("game-a", true);
        assert!(state.claim_operation("game-a", "busy").is_err());
        state.claim_game_removal("game-a", true).unwrap();
        state.finish_game_removal("game-a", false);
        state.claim_operation("game-a", "busy").unwrap();
    }

    #[test]
    fn pending_update_blocks_conflicting_operations_until_recovery_finishes() {
        let state = test_state();
        state.finish_game_update("game-a", true);
        assert!(state.claim_operation("game-a", "busy").is_err());
        assert!(state.claim_cloud_operation("game-a").is_err());
        assert!(state.claim_game_removal("game-a", true).is_err());
        assert!(state.claim_game_update("game-a", false).is_err());
        assert!(state.claim_account_restore().is_err());
        assert!(state.claim_library_migration(false).is_err());
        assert!(state.has_exclusive_operation(&state.save_operations.lock().unwrap(), "game-a"));
        state.claim_game_update("game-a", true).unwrap();
        assert!(state.claim_game_update("game-a", true).is_err());
        state.finish_game_update("game-a", true);
        assert!(state.claim_game_update("game-a", false).is_err());
        state.claim_game_update("game-a", true).unwrap();
        state.finish_game_update("game-a", false);
        state.claim_operation("game-a", "busy").unwrap();
    }

    #[test]
    fn update_admission_rejects_running_games_and_pending_removals() {
        let state = test_state();
        state.begin_runtime(running_runtime("uid-1")).unwrap();
        assert!(state.claim_game_update("uid-1", false).is_err());
        state.remove_runtime("uid-1");
        state.finish_game_removal("uid-1", true);
        assert!(state.claim_game_update("uid-1", true).is_err());
    }

    #[test]
    fn removal_admission_rejects_running_game_and_global_maintenance() {
        let state = test_state();
        state
            .begin_runtime(GameRuntime {
                game_uid: "game-a".into(),
                status: GameRuntimeStatus::Launching,
                pid: None,
                started_at: None,
                task_id: None,
            })
            .unwrap();
        assert!(state.claim_game_removal("game-a", false).is_err());
        assert!(state.save_operations.lock().unwrap().is_empty());
        state.remove_runtime("game-a");
        state.claim_library_migration(false).unwrap();
        assert!(state.claim_game_removal("game-a", true).is_err());
        state.finish_library_migration(false);
        state.claim_account_restore().unwrap();
        assert!(state.claim_game_removal("game-a", true).is_err());
    }

    #[test]
    fn account_restore_rejects_running_games_and_finished_learning_tasks_with_live_sessions() {
        let state = test_state();
        state.running_games.lock().unwrap().insert(
            "game".into(),
            GameRuntime {
                game_uid: "game".into(),
                status: crate::domain::GameRuntimeStatus::Running,
                pid: Some(1),
                started_at: None,
                task_id: None,
            },
        );
        assert!(state
            .claim_account_restore()
            .unwrap_err()
            .contains("关闭游戏"));
        assert!(state
            .claim_library_migration(false)
            .unwrap_err()
            .contains("关闭游戏"));
        state.running_games.lock().unwrap().clear();
        state.learning_sessions.lock().unwrap().insert(
            "session".into(),
            ActiveLearningSession {
                view: crate::domain::LearningSessionView {
                    session_id: "session".into(),
                    game_uid: "game".into(),
                    root_pid: 1,
                    started_at: "1".into(),
                    status: crate::domain::LearningStatus::Capturing,
                },
                roots: Vec::new(),
                baseline: None,
                tracked_processes: std::sync::Arc::new(Mutex::new(Default::default())),
                process_tracker_stop: std::sync::Arc::new(AtomicBool::new(false)),
                process_tracker_done: std::sync::Arc::new(AtomicBool::new(false)),
                etw_capture: None,
                etw_start_error: None,
                validation_only: false,
            },
        );
        assert!(state
            .claim_account_restore()
            .unwrap_err()
            .contains("存档识别"));
        assert!(state
            .claim_library_migration(false)
            .unwrap_err()
            .contains("存档识别"));
        assert!(state.save_operations.lock().unwrap().is_empty());
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

    #[test]
    fn exit_guard_counts_only_unfinished_background_tasks() {
        let state = test_state();
        assert_eq!(state.active_background_task_count(), 0);
        for task_type in [
            "package_game_body",
            "upload_game_body_package",
            "download_game_body_package",
            "sync_cloud_save",
            "cloud_account_sync",
            "restore_save_version",
            "analyze_saves",
            "launch_game",
        ] {
            let mut task: AppTask = serde_json::from_value(serde_json::json!({
                "taskId": "exit-test",
                "taskType": task_type,
                "status": "pending",
                "progress": 0,
                "message": "test"
            }))
            .unwrap();
            for category in [None, Some(TaskCategory::infer(task_type))] {
                task.category = category;
                for status in [
                    TaskStatus::Pending,
                    TaskStatus::Running,
                    TaskStatus::Success,
                    TaskStatus::Failed,
                    TaskStatus::Cancelled,
                    TaskStatus::Interrupted,
                ] {
                    task.status = status.clone();
                    task.cancel_requested = true;
                    state
                        .tasks
                        .lock()
                        .unwrap()
                        .insert(task.task_id.clone(), task.clone());
                    let expected = usize::from(
                        task_type != "launch_game"
                            && matches!(status, TaskStatus::Pending | TaskStatus::Running),
                    );
                    assert_eq!(
                        state.active_background_task_count(),
                        expected,
                        "{task_type}: {status:?}"
                    );
                }
            }
        }
    }

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

    #[test]
    fn cloud_restore_rejects_running_game_and_releases_claim() {
        let state = test_state();
        state.set_runtime(running_runtime("game-1"));
        assert!(CloudOperationClaim::claim_restore(&state, "game-1").is_err());
        assert!(state.save_operations.lock().unwrap().is_empty());
        let upload = CloudOperationClaim::claim(&state, "game-1").unwrap();
        drop(upload);
        state.remove_runtime("game-1");
        let restore = CloudOperationClaim::claim_restore(&state, "game-1").unwrap();
        assert!(occupied(&state, "game-1"));
        assert!(CloudOperationClaim::claim(&state, "game-1").is_err());
        drop(restore);
        assert!(!occupied(&state, "game-1"));
    }

    #[test]
    fn body_package_and_cloud_save_upload_can_overlap_in_either_order() {
        let state = test_state();
        let game_uid = "game-1";
        let cloud = state.claim_cloud_operation(game_uid).unwrap();
        let package = state.claim_body_package(game_uid).unwrap();
        assert!(state.claim_body_package(game_uid).is_err());
        assert!(state.claim_cloud_operation(game_uid).is_err());
        state.release_operation(&cloud);
        assert!(occupied(&state, game_uid));
        let next_cloud = state.claim_cloud_operation(game_uid).unwrap();
        state.release_operation(&package);
        assert!(occupied(&state, game_uid));
        state.release_operation(&next_cloud);
        assert!(!occupied(&state, game_uid));
    }

    #[test]
    fn body_package_blocks_operations_that_can_change_its_inputs() {
        let state = test_state();
        let game_uid = "game-1";
        let package = state.claim_body_package(game_uid).unwrap();
        assert!(CloudOperationClaim::claim_restore(&state, game_uid).is_err());
        assert!(state.claim_operation(game_uid, "busy").is_err());
        assert!(state.claim_game_update(game_uid, false).is_err());
        assert!(state.claim_game_removal(game_uid, false).is_err());
        assert!(state.claim_learning_start(game_uid).is_err());
        assert!(state.claim_library_migration(false).is_err());
        assert!(state.claim_account_restore().is_err());
        state.release_operation(&package);

        let restore = CloudOperationClaim::claim_restore(&state, game_uid).unwrap();
        assert!(state.claim_body_package(game_uid).is_err());
        drop(restore);
        state.set_runtime(running_runtime(game_uid));
        assert!(state.claim_body_package(game_uid).is_err());
        state.remove_runtime(game_uid);
        state.finish_game_update(game_uid, true);
        assert!(state.claim_body_package(game_uid).is_err());
        state.finish_game_update(game_uid, false);
        state.claim_account_restore().unwrap();
        assert!(state.claim_body_package(game_uid).is_err());
    }

    #[test]
    fn game_update_and_cloud_restore_are_mutually_exclusive() {
        let state = test_state();
        let game_uid = "game-1";

        state.claim_game_update(game_uid, false).unwrap();
        assert!(CloudOperationClaim::claim_restore(&state, game_uid).is_err());
        state.finish_game_update(game_uid, false);

        let restore = CloudOperationClaim::claim_restore(&state, game_uid).unwrap();
        assert!(state.claim_game_update(game_uid, false).is_err());
        drop(restore);

        state.claim_game_update(game_uid, false).unwrap();
        state.finish_game_update(game_uid, false);
    }

    #[test]
    fn game_update_and_cloud_upload_are_mutually_exclusive() {
        let state = test_state();
        let game_uid = "game-1";

        state.claim_game_update(game_uid, false).unwrap();
        assert!(CloudOperationClaim::claim(&state, game_uid).is_err());
        state.finish_game_update(game_uid, false);

        let upload = CloudOperationClaim::claim(&state, game_uid).unwrap();
        assert!(state.claim_game_update(game_uid, false).is_err());
        drop(upload);

        state.claim_game_update(game_uid, false).unwrap();
        state.finish_game_update(game_uid, false);
    }

    #[test]
    fn game_removal_blocks_update_cloud_upload_and_cloud_restore() {
        let state = test_state();
        let game_uid = "game-1";

        state.claim_game_removal(game_uid, false).unwrap();
        assert!(state.claim_game_update(game_uid, false).is_err());
        assert!(CloudOperationClaim::claim(&state, game_uid).is_err());
        assert!(CloudOperationClaim::claim_restore(&state, game_uid).is_err());
        state.finish_game_removal(game_uid, false);

        state.claim_game_update(game_uid, false).unwrap();
        state.finish_game_update(game_uid, false);
        let upload = CloudOperationClaim::claim(&state, game_uid).unwrap();
        drop(upload);
        let restore = CloudOperationClaim::claim_restore(&state, game_uid).unwrap();
        drop(restore);
    }

    #[test]
    fn duplicate_update_cloud_upload_and_cloud_restore_are_rejected() {
        let state = test_state();
        let game_uid = "game-1";

        state.claim_game_update(game_uid, false).unwrap();
        assert!(state.claim_game_update(game_uid, false).is_err());
        state.finish_game_update(game_uid, false);

        let upload = CloudOperationClaim::claim(&state, game_uid).unwrap();
        assert!(CloudOperationClaim::claim(&state, game_uid).is_err());
        drop(upload);

        let restore = CloudOperationClaim::claim_restore(&state, game_uid).unwrap();
        assert!(CloudOperationClaim::claim_restore(&state, game_uid).is_err());
        drop(restore);
    }

    type Admission = fn(&AppState, &str) -> Result<String, String>;

    fn admission_cases() -> [(&'static str, Admission); 5] {
        [
            ("update", |state, uid| {
                state.claim_game_update(uid, false)?;
                Ok(AppState::game_operation_key(uid))
            }),
            ("removal", |state, uid| {
                state.claim_game_removal(uid, false)?;
                Ok(AppState::game_operation_key(uid))
            }),
            ("package", AppState::claim_body_package),
            ("upload", |state, uid| {
                CloudOperationClaim::claim(state, uid).map(CloudOperationClaim::disarm)
            }),
            ("restore", |state, uid| {
                CloudOperationClaim::claim_restore(state, uid).map(CloudOperationClaim::disarm)
            }),
        ]
    }

    fn admissions_can_overlap(first: &str, second: &str) -> bool {
        matches!(
            (first, second),
            ("package", "upload") | ("upload", "package")
        )
    }

    #[test]
    fn admission_matrix_enforces_same_game_conflicts_in_both_orders() {
        for (first_name, first_claim) in admission_cases() {
            for (second_name, second_claim) in admission_cases() {
                let state = test_state();
                let first_key = first_claim(&state, "game-1").unwrap();
                let second = second_claim(&state, "game-1");
                assert_eq!(
                    second.is_ok(),
                    admissions_can_overlap(first_name, second_name),
                    "{first_name} -> {second_name}"
                );
                if let Ok(second_key) = second {
                    state.release_operation(&second_key);
                }
                assert_eq!(
                    *state.save_operations.lock().unwrap(),
                    HashSet::from([first_key.clone()]),
                    "second admission changed the first claim: {first_name} -> {second_name}"
                );
                state.release_operation(&first_key);
                let retry_key = second_claim(&state, "game-1").unwrap();
                state.release_operation(&retry_key);
                assert!(state.save_operations.lock().unwrap().is_empty());
            }
        }
    }

    #[test]
    fn admission_matrix_allows_different_games_and_isolates_release() {
        for (first_name, first_claim) in admission_cases() {
            for (second_name, second_claim) in admission_cases() {
                let state = test_state();
                let first_key = first_claim(&state, "game-1").unwrap();
                let second_key = second_claim(&state, "game-2")
                    .unwrap_or_else(|error| panic!("{first_name} -> {second_name}: {error}"));
                assert_ne!(first_key, second_key);
                state.release_operation(&first_key);
                state.release_operation(&first_key);
                assert_eq!(
                    *state.save_operations.lock().unwrap(),
                    HashSet::from([second_key.clone()])
                );
                assert!(second_claim(&state, "game-2").is_err());
                let retry_key = first_claim(&state, "game-1").unwrap();
                state.release_operation(&retry_key);
                state.release_operation(&second_key);
                assert!(state.save_operations.lock().unwrap().is_empty());
            }
        }
    }

    fn assert_concurrent_admission_matrix(different_games: bool) {
        for (first_name, first_claim) in admission_cases() {
            for (second_name, second_claim) in admission_cases() {
                for round in 0..4 {
                    let state = test_state();
                    let start = std::sync::Barrier::new(3);
                    let second_uid = if different_games { "game-2" } else { "game-1" };
                    let (first, second) = std::thread::scope(|scope| {
                        let first_worker = scope.spawn(|| {
                            start.wait();
                            first_claim(&state, "game-1")
                        });
                        let second_worker = scope.spawn(|| {
                            start.wait();
                            second_claim(&state, second_uid)
                        });
                        start.wait();
                        (first_worker.join().unwrap(), second_worker.join().unwrap())
                    });
                    let expected =
                        if different_games || admissions_can_overlap(first_name, second_name) {
                            2
                        } else {
                            1
                        };
                    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), expected,
                        "concurrent {first_name} / {second_name}, different_games={different_games}, round={round}");
                    let keys: HashSet<_> = first.into_iter().chain(second).collect();
                    assert_eq!(keys.len(), expected);
                    assert_eq!(*state.save_operations.lock().unwrap(), keys);
                    for key in keys {
                        state.release_operation(&key);
                    }
                    let first_key = first_claim(&state, "game-1").unwrap();
                    state.release_operation(&first_key);
                    let second_key = second_claim(&state, second_uid).unwrap();
                    state.release_operation(&second_key);
                    assert!(state.save_operations.lock().unwrap().is_empty());
                }
            }
        }
    }

    #[test]
    fn concurrent_same_game_admissions_keep_only_compatible_claims() {
        assert_concurrent_admission_matrix(false);
    }

    #[test]
    fn concurrent_different_game_admissions_do_not_block_each_other() {
        assert_concurrent_admission_matrix(true);
    }

    fn assert_pending_transaction_isolation(removal: bool) {
        for (name, claim) in admission_cases() {
            let state = test_state();
            if removal {
                state.claim_game_removal("game-1", false).unwrap();
                state.finish_game_removal("game-1", true);
            } else {
                state.claim_game_update("game-1", false).unwrap();
                state.finish_game_update("game-1", true);
            }
            assert!(claim(&state, "game-1").is_err(), "pending -> {name}");
            let other_key = claim(&state, "game-2").unwrap();
            state.release_operation(&other_key);
            assert!(claim(&state, "game-1").is_err());
            if removal {
                state.claim_game_removal("game-1", true).unwrap();
                state.finish_game_removal("game-1", false);
            } else {
                state.claim_game_update("game-1", true).unwrap();
                state.finish_game_update("game-1", false);
            }
            let retry_key = claim(&state, "game-1").unwrap();
            state.release_operation(&retry_key);
            assert!(state.save_operations.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn pending_update_blocks_its_game_without_blocking_other_games() {
        assert_pending_transaction_isolation(false);
    }

    #[test]
    fn pending_removal_blocks_its_game_without_blocking_other_games() {
        assert_pending_transaction_isolation(true);
    }

    #[test]
    fn cloud_restore_guard_releases_claim_after_command_error() {
        fn command(state: &AppState) -> Result<(), String> {
            let _claim = CloudOperationClaim::claim_restore(state, "game-1")?;
            Err("manifest unavailable".into())
        }
        let state = test_state();
        assert!(command(&state).is_err());
        assert!(state.save_operations.lock().unwrap().is_empty());
        let restore = CloudOperationClaim::claim_restore(&state, "game-1").unwrap();
        drop(restore);
        assert!(state.save_operations.lock().unwrap().is_empty());
    }

    #[test]
    fn cloud_restore_claim_survives_worker_handoff_until_worker_releases_it() {
        let state = test_state();
        let key = CloudOperationClaim::claim_restore(&state, "game-1")
            .unwrap()
            .disarm();
        assert!(state.claim_game_update("game-1", false).is_err());
        std::thread::scope(|scope| {
            scope
                .spawn(|| state.release_operation(&key))
                .join()
                .unwrap();
        });
        assert!(state.save_operations.lock().unwrap().is_empty());
        state.claim_game_update("game-1", false).unwrap();
        state.finish_game_update("game-1", false);
    }

    #[test]
    fn cloud_restore_checks_runtime_again_before_writing() {
        let state = test_state();
        let claim = CloudOperationClaim::claim_restore(&state, "game-1").unwrap();
        state.set_runtime(running_runtime("game-1"));
        assert!(state.ensure_save_restore_idle("game-1").is_err());
        assert!(state.ensure_save_restore_idle("game-2").is_ok());
        drop(claim);
    }

    #[test]
    fn cloud_restore_rejects_active_learning_even_without_runtime_entry() {
        let state = test_state();
        state.learning_sessions.lock().unwrap().insert(
            "session-1".into(),
            ActiveLearningSession {
                view: crate::domain::LearningSessionView {
                    session_id: "session-1".into(),
                    game_uid: "game-1".into(),
                    root_pid: 1,
                    started_at: "0".into(),
                    status: crate::domain::LearningStatus::Capturing,
                },
                roots: Vec::new(),
                baseline: None,
                tracked_processes: std::sync::Arc::new(Mutex::new(Default::default())),
                process_tracker_stop: std::sync::Arc::new(AtomicBool::new(false)),
                process_tracker_done: std::sync::Arc::new(AtomicBool::new(false)),
                etw_capture: None,
                etw_start_error: None,
                validation_only: false,
            },
        );
        assert!(CloudOperationClaim::claim_restore(&state, "game-1").is_err());
        assert!(state.save_operations.lock().unwrap().is_empty());
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
