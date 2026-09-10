use crate::services::process_service::TrackedProcessHandle;
use crate::{
    app_state::AppState,
    domain::{
        AppStore, AppTask, Game, GameLifecycle, GameRuntime, GameRuntimeStatus, SaveProfile,
        SaveVersion, TaskCategory, TaskStatus,
    },
    repositories::{GameRepository, SaveRepository},
    services::{GameLibraryService, TaskService},
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Manager};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchPrecheck {
    pub game_uid: String,
    pub can_launch: bool,
    pub executable_exists: bool,
    pub save_profile_ready: bool,
    pub valid_scope_count: usize,
    pub issues: Vec<String>,
}

pub struct LaunchService;

impl LaunchService {
    pub fn precheck(
        store: &crate::domain::AppStore,
        game_uid: &str,
    ) -> Result<LaunchPrecheck, String> {
        let game =
            GameLibraryService::find(store, game_uid).ok_or_else(|| "游戏不存在".to_string())?;
        let mut issues = Vec::new();
        if !matches!(game.lifecycle, GameLifecycle::Active) {
            issues.push("游戏尚未完成设置".to_string());
        }
        if !Path::new(&game.managed_path).is_dir() {
            issues.push("游戏本体目录不存在".to_string());
        }
        let executable = match managed_executable_path(&game) {
            Ok(path) => path,
            Err(error) => {
                issues.push(error);
                PathBuf::new()
            }
        };
        let executable_exists = executable.is_file();
        if !executable_exists {
            issues.push("启动程序不存在".to_string());
        }
        let profile = store.save_profiles.iter().find(|profile| {
            profile.game_uid == game.game_uid
                && game.save_profile_id.as_deref() == Some(profile.profile_id.as_str())
                && profile.enabled
        });
        let save_profile_ready = profile.is_some();
        let valid_scope_count = profile
            .map(GameLibraryService::valid_scope_count)
            .unwrap_or_default();
        Ok(LaunchPrecheck {
            game_uid: game.game_uid,
            can_launch: issues.is_empty(),
            executable_exists,
            save_profile_ready,
            valid_scope_count,
            issues,
        })
    }

    pub fn launch(app: &AppHandle, state: &AppState, game_uid: String) -> Result<String, String> {
        let game_uid = game_uid.trim().to_string();
        let operation_lock = state
            .save_operations
            .lock()
            .map_err(|_| "lock save operation state failed".to_string())?;
        if operation_lock.contains(&game_uid) {
            return Err("游戏正在进行存档版本操作".to_string());
        }
        if let Ok(sessions) = state.learning_sessions.lock() {
            if sessions.values().any(|s| s.view.game_uid == game_uid) {
                return Err("游戏正在进行存档识别学习，无法直接启动".to_string());
            }
        }
        // 检查与写入在同一次加锁内完成（`begin_runtime` 内部保证），并且先于
        // 释放 operation_lock，避免同一游戏被并发拉起两个实例。
        state.begin_runtime(GameRuntime {
            game_uid: game_uid.clone(),
            status: GameRuntimeStatus::Launching,
            pid: None,
            started_at: Some(now_iso()),
            task_id: None,
        })?;
        drop(operation_lock);
        let loaded = (|| -> Result<(Game, Option<SaveProfile>, Option<SaveVersion>), String> {
            let store = state
                .store
                .lock()
                .map_err(|_| "lock GameSaver store failed".to_string())?;
            let check = Self::precheck(&store, &game_uid)?;
            if !check.can_launch {
                return Err(format!("无法启动游戏：{}", check.issues.join("、")));
            }
            let game = GameLibraryService::find(&store, &game_uid)
                .ok_or_else(|| "游戏不存在".to_string())?;
            // 与跨重启接手共用同一套挑选规则，避免两条路径认的配置不是同一个。
            let (profile, latest) = session_inputs_from_store(&store, &game);
            Ok((game, profile, latest))
        })();
        let (game, profile, latest) = match loaded {
            Ok(value) => value,
            Err(error) => {
                state.remove_runtime(&game_uid);
                return Err(error);
            }
        };
        let task_id = match TaskService::create(
            state,
            "launch_game",
            TaskCategory::Session,
            Some(game_uid.clone()),
            "准备启动游戏",
        ) {
            Ok(task_id) => task_id,
            Err(error) => {
                state.remove_runtime(&game_uid);
                return Err(error);
            }
        };
        state.update_runtime(&game_uid, |runtime| {
            runtime.task_id = Some(task_id.clone());
        });
        let app_handle = app.clone();
        let task_id_for_thread = task_id.clone();
        thread::spawn(move || {
            let result = run_game_session(
                &app_handle,
                &game,
                profile.as_ref(),
                latest.as_ref(),
                &task_id_for_thread,
            );
            conclude_session(&app_handle, &game.game_uid, &task_id_for_thread, result);
        });
        Ok(task_id)
    }

    /// 跨重启重建：把上次仍在运行、但运行时标记已随进程消亡的游戏会话接回来。
    ///
    /// 应用退出时承载会话的线程随之消失，但**游戏进程不会跟着死**。不重建的话，用户
    /// 重启后会看到「可启动」，再点一次就拉起第二个实例 —— 对存档型游戏，两个进程
    /// 同时写同一份存档；而且这场游玩结束时不会再有人提交存档版本，产品的核心承诺
    /// （「游戏退出后自动提交」）直接失效。
    ///
    /// 重建只做「接手」：不 spawn、不打断游戏，只把运行标记与任务记录恢复出来，然后
    /// 挂起一个等价于原会话的等待线程，等它自然退出后照常提交存档。
    ///
    /// 必须在事件推送器注入**之后**调用，否则重建产生的状态变化推不到前端。
    pub fn restore_running_sessions(app: &AppHandle, state: &AppState) -> Vec<String> {
        let mut restored = Vec::new();
        for (game, root) in adoptable_sessions(state) {
            match resume_session(app, state, game, root) {
                Ok(game_uid) => restored.push(game_uid),
                Err(error) => crate::logging::error(format!("重建游戏会话失败：{error}")),
            }
        }
        if !restored.is_empty() {
            crate::logging::info(format!("已接回 {} 场上次未结束的游戏会话", restored.len()));
        }
        restored
    }
}

/// 会话的「根进程」：决定如何等待它退出、以及取消时如何终止它。
///
/// 区分两条来源，是因为跨重启接手的会话没有 `Child` 句柄 —— 进程在应用启动之前
/// 就已经跑起来了，我们只是半路认领，只能靠句柄探测存活、按 PID 终止。
enum SessionRoot {
    /// 本次会话由自己 spawn，持有完整句柄。
    Spawned(Child),
    /// 跨重启接手：进程早于本次应用启动就已存在。
    Adopted {
        pid: u32,
        /// 持有句柄有两个作用：让存活探测无需反复开句柄；在会话存续期间阻止
        /// Windows 回收该 PID，使后续按 PID 的终止不会打错目标。
        handle: TrackedProcessHandle,
    },
}

impl SessionRoot {
    fn pid(&self) -> u32 {
        match self {
            SessionRoot::Spawned(child) => child.id(),
            SessionRoot::Adopted { pid, .. } => *pid,
        }
    }

    /// 探测根进程是否已退出。
    fn probe(&mut self) -> Result<RootProbe, String> {
        match self {
            SessionRoot::Spawned(child) => child
                .try_wait()
                .map_err(|err| format!("等待游戏退出失败：{err}"))
                .map(|status| match status {
                    Some(status) => RootProbe::Exited(Some(status)),
                    None => RootProbe::Running,
                }),
            SessionRoot::Adopted { handle, .. } => Ok(if handle.is_alive() {
                RootProbe::Running
            } else {
                // 接手的会话拿不到退出码：进程不是我们的子进程，没有可回收的退出状态。
                RootProbe::Exited(None)
            }),
        }
    }

    /// 终止根进程。调用方负责在有界等待后处理仍未退出的子进程。
    fn terminate(&mut self) {
        match self {
            SessionRoot::Spawned(child) => {
                let _ = child.kill();
                let _ = child.wait();
            }
            SessionRoot::Adopted { pid, .. } => {
                crate::services::process_service::terminate_process(*pid);
            }
        }
    }
}

/// 一次根进程探测的结果。
enum RootProbe {
    Running,
    /// 已退出。`None` 表示拿不到退出码（接手的会话）。
    Exited(Option<ExitStatus>),
}

/// 一场游戏会话的结束方式。
///
/// 必须在类型上把「游戏自己退出」和「用户主动取消」分开：两者的收尾完全不同 ——
/// 前者要提交存档快照，后者绝不能提交（游戏进程可能还在写盘）。此前这里统一返回
/// `ExitStatus`，上游无从分辨，取消也就被当成正常退出，照常快照并标记成功。
#[derive(Debug)]
enum GameSessionEnd {
    /// 根进程与所有被跟踪的子进程都已自然退出。
    ///
    /// 退出码是 `Option`：自己 spawn 的会话能拿到，跨重启接手的会话拿不到。
    Exited(Option<ExitStatus>),
    /// 用户请求取消，进程树已由 [`terminate_session`] 收尾。
    Cancelled,
}

/// [`run_game_session`] 的结果：要么产出了存档版本，要么整场会话被取消。
#[derive(Debug)]
enum GameSessionOutcome {
    Saved {
        message: String,
        summary: serde_json::Value,
    },
    Cancelled,
}

fn run_game_session(
    app: &AppHandle,
    game: &Game,
    profile: Option<&SaveProfile>,
    latest: Option<&SaveVersion>,
    task_id: &str,
) -> Result<GameSessionOutcome, String> {
    let state = app.state::<AppState>();
    // spawn 之前先看一眼：任务若在「创建」与「spawn」之间被取消（或已被删除），
    // 就不要再拉起一个没人跟踪的游戏进程。
    if TaskService::is_cancelled(&state, task_id) {
        return Ok(GameSessionOutcome::Cancelled);
    }
    let executable = managed_executable_path(game)?;
    let working_directory = game
        .launch
        .working_directory_relative_path
        .as_deref()
        .map(|relative| safe_join(Path::new(&game.managed_path), relative))
        .transpose()?
        .unwrap_or_else(|| {
            executable
                .parent()
                .unwrap_or(Path::new(&game.managed_path))
                .to_path_buf()
        });
    let mut command = Command::new(&executable);
    command
        .args(&game.launch.arguments)
        .current_dir(&working_directory);
    let child = command
        .spawn()
        .map_err(|err| format!("启动游戏失败：{err}"))?;
    let pid = child.id();
    state.set_runtime(GameRuntime {
        game_uid: game.game_uid.clone(),
        status: GameRuntimeStatus::Running,
        pid: Some(pid),
        started_at: Some(now_iso()),
        task_id: Some(task_id.to_string()),
    });
    let mut root = SessionRoot::Spawned(child);
    finish_game_session(app, game, profile, latest, task_id, &mut root)
}

/// 等会话结束，然后提交存档快照。
///
/// 「自己 spawn」与「跨重启接手」两条路径在这里合流：它们只在对根进程的等待方式上
/// 不同，而「游戏退出之后要做什么」完全一致。分开写会让收尾逻辑各自演化 —— 取消该
/// 不该提交存档这类判断，最怕的就是两处不一致。
fn finish_game_session(
    app: &AppHandle,
    game: &Game,
    profile: Option<&SaveProfile>,
    latest: Option<&SaveVersion>,
    task_id: &str,
    root: &mut SessionRoot,
) -> Result<GameSessionOutcome, String> {
    let state = app.state::<AppState>();
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        10,
        running_session_message(profile.is_some()),
        None,
    );
    match wait_for_game_session(root, Path::new(&game.managed_path), &state, task_id)? {
        // 退出码不参与业务判断，但非正常退出时它是最直接的线索。
        GameSessionEnd::Exited(Some(status)) => {
            if !status.success() {
                crate::logging::error(format!(
                    "游戏进程异常退出：task_id={task_id} status={status}"
                ));
            }
        }
        // 接手的会话没有退出码可看，只留一条线索说明这条记录是怎么来的。
        GameSessionEnd::Exited(None) => {
            crate::logging::info(format!("接手的游戏会话已结束：task_id={task_id}"));
        }
        // 取消：进程树已经收尾，此时再去快照存档，等于对着一堆刚刚还在写盘的文件
        // 拍照，而且会话一结束就没人再跟踪这个游戏了。整场会话直接作废。
        GameSessionEnd::Cancelled => return Ok(GameSessionOutcome::Cancelled),
    }
    state.update_runtime(&game.game_uid, |runtime| {
        runtime.status = GameRuntimeStatus::Saving;
    });
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        70,
        if profile.is_some() {
            "游戏已退出，正在提交存档版本"
        } else {
            "游戏已退出，正在更新游戏状态"
        },
        None,
    );
    let version = profile
        .map(|profile| {
            SaveRepository::commit(app, game, profile, latest, |progress, message| {
                TaskService::update(
                    &state,
                    task_id,
                    TaskStatus::Running,
                    70 + progress / 3,
                    message,
                    None,
                );
            })
        })
        .transpose()?
        .flatten();
    let pending_version = version.clone();
    // 剪枝只改内存；对象回收（GC）必须等持久化成功之后再做，因此把剪枝结果带出闭包。
    let (version_summary, pruned_versions) = match state.with_store_mut(|candidate| {
        let now = now_iso();
        if let Some(version) = version {
            let version_id = version.version_id.clone();
            let file_count = version.files.len();
            candidate.save_versions.push(version);

            let keep_versions = profile.map(|p| p.keep_versions).unwrap_or(5);
            let pruned =
                SaveRepository::prune_game_save_versions(candidate, &game.game_uid, keep_versions);

            let Some(game_record) = candidate
                .games
                .iter_mut()
                .find(|item| item.game_uid == game.game_uid)
            else {
                return Err("游戏记录不存在".to_string());
            };
            game_record.latest_save_version_id = Some(version_id.clone());
            game_record.last_played_at = Some(now);
            GameRepository::persist(app, candidate)?;
            Ok((
                serde_json::json!({ "created": true, "versionId": version_id, "fileCount": file_count }),
                pruned,
            ))
        } else {
            if let Some(game_record) = candidate
                .games
                .iter_mut()
                .find(|item| item.game_uid == game.game_uid)
            {
                game_record.last_played_at = Some(now);
            }
            GameRepository::persist(app, candidate)?;
            Ok((serde_json::json!({ "created": false, "fileCount": 0 }), None))
        }
    }) {
        Ok(result) => result,
        Err(error) => {
            if let Some(version) = pending_version.as_ref() {
                crate::repositories::release_pending_objects(version);
            }
            return Err(error);
        }
    };
    // 版本清单已经落盘，此刻回收旧对象才是安全的：即使这里失败，磁盘清单与现存对象仍然
    // 自洽（最多多留几个已无引用的对象文件）。若把回收放进上面的闭包、跑在持久化之前，
    // 一旦 `persist` 失败，磁盘清单就会列着对象已被删除的版本，那些版本将永远无法恢复。
    if let Some(versions) = pruned_versions.as_ref() {
        let _ = SaveRepository::collect_garbage(app, versions);
    }
    if let Some(version) = pending_version.as_ref() {
        crate::repositories::release_pending_objects(version);
    }
    let message = if version_summary
        .get("created")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        "游戏已退出，存档版本已提交"
    } else {
        "游戏已退出，没有新的存档变化"
    };
    Ok(GameSessionOutcome::Saved {
        message: message.to_string(),
        summary: version_summary,
    })
}

fn wait_for_game_session(
    root: &mut SessionRoot,
    managed_path: &Path,
    state: &AppState,
    task_id: &str,
) -> Result<GameSessionEnd, String> {
    let root_pid = root.pid();
    let mut tracked_pids = HashSet::from([root_pid]);
    let mut tracked_handles: HashMap<u32, TrackedProcessHandle> = HashMap::new();
    let mut root_exited = false;
    let mut root_status: Option<ExitStatus> = None;
    let mut last_scan = Instant::now();

    let refresh_processes =
        |pids: &mut HashSet<u32>, handles: &mut HashMap<u32, TrackedProcessHandle>| {
            // 1. Expand process tree via Toolhelp snapshot (parent -> child)
            let _ = crate::services::learning::extend_tracked_process_tree(pids);

            // 2. Discover any process whose executable image is located within managed_path (handles UAC/detached launchers)
            let dir_pids =
                crate::services::process_service::find_processes_in_directory(managed_path);
            pids.extend(dir_pids);

            // 3. For any newly discovered PID (except root_pid and ignored crash handlers), open and hold handle
            for &pid in pids.iter() {
                if pid != root_pid && !handles.contains_key(&pid) {
                    if let Some(handle) = TrackedProcessHandle::open(pid) {
                        if handle.is_alive() {
                            if let Some(image_path) =
                                crate::services::process_service::get_process_image_path(pid)
                            {
                                let file_name = image_path
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or_default();
                                if crate::services::process_service::is_ignored_process_name(
                                    file_name,
                                ) {
                                    continue;
                                }
                            }
                            handles.insert(pid, handle);
                        }
                    }
                }
            }
        };

    // Initial scan to catch immediate launcher sub-processes
    refresh_processes(&mut tracked_pids, &mut tracked_handles);

    loop {
        if TaskService::is_cancelled(state, task_id) {
            // 注意这里不能因为「根进程已退出」就提前返回：根往往只是个启动器
            // （UAC 提权 / 分离式启动），游戏本体还跑在被跟踪的子进程里。取消
            // 的语义是整场会话作废，必须把整棵进程树都收掉。
            terminate_session(root, root_exited, &mut tracked_handles);
            return Ok(GameSessionEnd::Cancelled);
        }

        // Check root process exit status
        if !root_exited {
            match root.probe()? {
                RootProbe::Exited(status) => {
                    root_exited = true;
                    root_status = status;
                    // When root exits, perform immediate refresh to catch any final spawned process
                    refresh_processes(&mut tracked_pids, &mut tracked_handles);
                }
                RootProbe::Running => {}
            }
        }

        // Periodic process tree and directory scan (every 1 second)
        if last_scan.elapsed() >= Duration::from_secs(1) {
            refresh_processes(&mut tracked_pids, &mut tracked_handles);
            last_scan = Instant::now();
        }

        // Prune terminated child processes!
        // Because TrackedProcessHandle keeps an open handle, Windows cannot recycle the PID.
        // Once is_alive() returns false, the handle is dropped (closing it), and pruned.
        tracked_handles.retain(|_, handle| handle.is_alive());

        // Exit evaluation:
        // When root process has exited and no non-ignored child/directory processes remain alive:
        if root_exited && tracked_handles.is_empty() {
            return Ok(GameSessionEnd::Exited(root_status));
        }

        thread::sleep(Duration::from_millis(500));
    }
}

/// 取消会话时的收尾：把整棵被跟踪的进程树停掉，并确认它们真的退出了。
///
/// 顺序是先断根本、再清子进程 —— 先杀根可以立刻阻断它继续派生新进程，之后
/// 再把已经记录在案的子进程逐个终止，避免「边杀边生」。
///
/// 等待是有界的：终止失败（例如游戏以管理员权限启动、而本进程没有对应权限）
/// 时只记日志放弃，绝不把执行线程永久挂住。残留进程的跟踪标记会随会话结束一并
/// 清除，因此宁可留下一条错误日志，也不能让「运行中」状态卡死在界面上。
///
/// 跨重启接手来的会话也走同一条路：它的根不是我们的子进程，没有 `Child` 可用，
/// 由 [`SessionRoot::terminate`] 转为按 PID 终止。
fn terminate_session(
    root: &mut SessionRoot,
    root_exited: bool,
    tracked_handles: &mut HashMap<u32, TrackedProcessHandle>,
) {
    if !root_exited {
        root.terminate();
    }
    for (&pid, handle) in tracked_handles.iter() {
        if handle.is_alive() {
            crate::services::process_service::terminate_process(pid);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        tracked_handles.retain(|_, handle| handle.is_alive());
        if tracked_handles.is_empty() || Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if !tracked_handles.is_empty() {
        let mut survivors: Vec<u32> = tracked_handles.keys().copied().collect();
        survivors.sort_unstable();
        crate::logging::error(format!(
            "取消游戏会话后仍有子进程未能终止，可能需要手动结束：pids={survivors:?}"
        ));
    }
}

/// 会话收尾：摘掉运行标记、按终态写任务记录、必要时触发自动同步。
///
/// 启动（自己 spawn）与重建（接手别人的进程）两条路径的收尾完全一致，收在这里避免
/// 两处各自演化 —— 「取消不触发云同步」这类约束最怕的就是只有一边守住了。
fn conclude_session(
    app: &AppHandle,
    game_uid: &str,
    task_id: &str,
    result: Result<GameSessionOutcome, String>,
) {
    let state = app.state::<AppState>();
    // 会话结束（无论成功或失败）先摘掉运行时标记，前端据此把「运行中」变回
    // 「启动游戏」；这一步会推送 runtime-changed。
    state.remove_runtime(game_uid);
    match result {
        Ok(GameSessionOutcome::Saved { message, summary }) => {
            TaskService::finish(
                &state,
                task_id,
                TaskStatus::Success,
                100,
                message,
                Some(summary.clone()),
                None,
            );
            if summary
                .get("created")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                if let Some(version_id) = summary.get("versionId").and_then(|v| v.as_str()) {
                    let app_data_dir = app.path().app_data_dir().ok();
                    let config = app_data_dir.as_deref().and_then(|dir| {
                        crate::repositories::BaiduConfigRepository::load(dir)
                            .ok()
                            .flatten()
                    });
                    if config.map(|c| c.auto_sync_save).unwrap_or(true) {
                        let _ =
                            crate::commands::cloud_save_commands::start_upload_save_version_task(
                                app.clone(),
                                app.state::<AppState>(),
                                game_uid.to_string(),
                                version_id.to_string(),
                            );
                    }
                }
            }
        }
        // 取消既不是成功也不是失败：用户在传输中心按下的就是「别继续了」，
        // 如实标记即可，也绝不能顺带触发一次云同步。
        Ok(GameSessionOutcome::Cancelled) => TaskService::finish(
            &state,
            task_id,
            TaskStatus::Cancelled,
            100,
            "已取消启动游戏",
            None,
            None,
        ),
        Err(error) => TaskService::finish(
            &state,
            task_id,
            TaskStatus::Failed,
            100,
            "游戏会话结束，但存档版本提交失败",
            None,
            Some(error),
        ),
    }
}

/// 会话运行期间的任务文案。两条路径共用，避免同一种状态出现两种说法。
fn running_session_message(profile_ready: bool) -> &'static str {
    if profile_ready {
        "游戏正在运行，退出后将提交存档版本"
    } else {
        "游戏正在运行，存档保护尚未设置"
    }
}

/// 找出所有「进程仍在运行、但运行时标记已随上次退出丢失」的游戏。
///
/// 只做发现，不改变任何状态；是否接手由调用方决定。刻意不依赖 `AppHandle` —— 这条
/// 最容易出错的判断（「这个游戏到底还在不在跑」）因此可以在单元测试里直接验证。
fn adoptable_sessions(state: &AppState) -> Vec<(Game, SessionRoot)> {
    let games = match state.store.lock() {
        Ok(store) => store.games.clone(),
        Err(_) => return Vec::new(),
    };
    let mut found = Vec::new();
    for game in games {
        // 只有正常状态的游戏才可能在运行；迁移 / 移除中的游戏由各自的流程负责。
        if !matches!(game.lifecycle, GameLifecycle::Active) {
            continue;
        }
        // 本次启动已经标记过运行中的，不重复接手。
        if state.runtime_of(&game.game_uid).is_some() {
            continue;
        }
        if let Some(root) = adoptable_root(&game) {
            found.push((game, root));
        }
    }
    found
}

/// 在游戏目录里找一个可以接手的进程作为会话锚点。
///
/// 命中多个是常态（启动器 + 本体，或本体带起的子进程）。挑哪个当「根」不影响正确性：
/// 会话是否结束由 [`wait_for_game_session`] 对**整个目录**的进程跟踪决定，根只是一个
/// 存活锚点，以及取消时的首要目标。
fn adoptable_root(game: &Game) -> Option<SessionRoot> {
    let managed_path = Path::new(&game.managed_path);
    if !managed_path.is_dir() {
        return None;
    }
    crate::services::process_service::find_processes_in_directory(managed_path)
        .into_iter()
        .find_map(|pid| {
            let handle = TrackedProcessHandle::open(pid)?;
            // 从扫描到打开句柄之间进程可能已经退出。持有句柄同时保证 PID 在会话存续
            // 期间不被回收，使后续按 PID 的存活探测与终止都不会打错目标。
            handle
                .is_alive()
                .then_some(SessionRoot::Adopted { pid, handle })
        })
}

/// 接手一场跨重启仍在进行的会话：恢复运行标记与任务记录，然后挂起等待线程。
fn resume_session(
    app: &AppHandle,
    state: &AppState,
    game: Game,
    mut root: SessionRoot,
) -> Result<String, String> {
    let (profile, latest) = load_session_inputs(state, &game)?;
    let pid = root.pid();
    let previous = find_interrupted_session_task(state, &game.game_uid);
    // 原会话的起点就是那条任务的创建时间，比「接手时刻」更接近事实。
    let started_at = previous
        .as_ref()
        .and_then(|task| millis_to_seconds(&task.created_at))
        .unwrap_or_else(now_iso);
    let message = running_session_message(profile.is_some());
    let task_id = match previous {
        Some(task) => {
            // 复活上次那条记录，而不是新建一条：玩家看到的应当是一条连续的会话记录
            // （从「异常中断」回到「运行中」），而不是「一条中断 + 一条新的」。
            // `update` 不落盘，磁盘上仍是 Interrupted —— 若应用再次异常退出，下次启动
            // 会再走到这里，语义自洽。
            TaskService::update(state, &task.task_id, TaskStatus::Running, 10, message, None);
            task.task_id
        }
        None => TaskService::create(
            state,
            "launch_game",
            TaskCategory::Session,
            Some(game.game_uid.clone()),
            message,
        )?,
    };
    state.set_runtime(GameRuntime {
        game_uid: game.game_uid.clone(),
        status: GameRuntimeStatus::Running,
        pid: Some(pid),
        started_at: Some(started_at),
        task_id: Some(task_id.clone()),
    });
    crate::logging::info(format!(
        "接回上次未结束的游戏会话：game_uid={} pid={pid} task_id={task_id}",
        game.game_uid
    ));
    let app_handle = app.clone();
    let game_uid = game.game_uid.clone();
    let thread_game_uid = game_uid.clone();
    let thread_task_id = task_id.clone();
    thread::spawn(move || {
        let result = finish_game_session(
            &app_handle,
            &game,
            profile.as_ref(),
            latest.as_ref(),
            &thread_task_id,
            &mut root,
        );
        conclude_session(&app_handle, &thread_game_uid, &thread_task_id, result);
    });
    Ok(game_uid)
}

/// 找回该游戏上次因应用退出而中断的会话任务。
///
/// 取最近的一条：同一游戏可能留下多条历史中断记录（每次异常退出各一条），只有最新的
/// 那条对应「刚刚仍在进行的那场会话」。`TaskService::list` 已按创建时间降序，取首个命中。
fn find_interrupted_session_task(state: &AppState, game_uid: &str) -> Option<AppTask> {
    TaskService::list(state).ok()?.into_iter().find(|task| {
        task.task_type == "launch_game"
            && task.game_uid.as_deref() == Some(game_uid)
            && matches!(task.status, TaskStatus::Interrupted)
    })
}

/// 读取提交存档所需的输入（当前生效的存档配置与最近一版存档）。
///
/// 跨重启后必须重新读：`SaveProfile` 可能已被用户改动，`latest` 决定本次快照与哪一版
/// 做差异比较 —— 沿用上次启动时的内存副本，会让快照基于错误的基准。
fn load_session_inputs(
    state: &AppState,
    game: &Game,
) -> Result<(Option<SaveProfile>, Option<SaveVersion>), String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "lock GameSaver store failed".to_string())?;
    Ok(session_inputs_from_store(&store, game))
}

/// 从 store 里挑出该游戏当前生效的存档配置与最近一版存档。
///
/// 抽成纯函数是为了让「启动」与「接手」共用同一套挑选规则 —— 两边各写一遍，迟早会出现
/// 启动时认的配置和接手时认的配置不是同一个。
fn session_inputs_from_store(
    store: &AppStore,
    game: &Game,
) -> (Option<SaveProfile>, Option<SaveVersion>) {
    let profile = store
        .save_profiles
        .iter()
        .find(|profile| {
            profile.game_uid == game.game_uid
                && game.save_profile_id.as_deref() == Some(profile.profile_id.as_str())
                && profile.enabled
        })
        .cloned();
    let latest = game
        .latest_save_version_id
        .as_ref()
        .and_then(|id| {
            store
                .save_versions
                .iter()
                .find(|version| &version.version_id == id)
        })
        .cloned();
    (profile, latest)
}

/// 任务记录用毫秒时间戳，而 `GameRuntime.started_at` 沿用秒级口径；
/// 接回会话时以原任务的创建时间作为会话起点，两者需要换算。
fn millis_to_seconds(millis: &str) -> Option<String> {
    millis
        .parse::<u64>()
        .ok()
        .map(|value| (value / 1000).to_string())
}

fn managed_executable_path(game: &Game) -> Result<PathBuf, String> {
    safe_join(
        Path::new(&game.managed_path),
        &game.launch.executable_relative_path,
    )
}

fn safe_join(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let relative = Path::new(relative);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err("启动路径包含无效的上级目录".to_string());
    }
    Ok(root.join(relative))
}

fn now_iso() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::game::{CloudStatus, LaunchConfig};
    use crate::domain::{AppStore, GameHealth};
    use std::sync::Arc;

    fn test_state(dir: &Path) -> AppState {
        AppState::new(
            AppStore::default(),
            dir.to_path_buf(),
            HashMap::new(),
            dir.join("tasks.json"),
        )
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gamesaver-launch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// 从 `root_pid` 出发展开进程树，返回第一个后代进程的 PID。
    ///
    /// 进程树的展开依赖系统快照里的父子关系，子进程并不是 spawn 完就立刻可见，
    /// 因此这里要轮询等待。
    fn wait_for_descendant(root_pid: u32) -> Option<u32> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let mut tracked = HashSet::from([root_pid]);
            let _ = crate::services::learning::extend_tracked_process_tree(&mut tracked);
            tracked.remove(&root_pid);
            if let Some(pid) = tracked.into_iter().next() {
                return Some(pid);
            }
            thread::sleep(Duration::from_millis(50));
        }
        None
    }

    /// S4 回归：取消一场会话必须停掉**整棵**被跟踪的进程树，而不是只杀根进程。
    ///
    /// 场景刻意模拟分离式启动器：根进程只是个转发层，真正的「游戏」是它的子进程。
    /// 修复前只 `child.kill()`，子进程会继续活着 —— 而 runtime 标记已经被摘掉，
    /// 于是游戏在无人跟踪的状态下继续写存档，快照出来的就是半截数据。
    #[test]
    #[cfg(target_os = "windows")]
    fn cancelling_a_session_terminates_the_tracked_process_tree() {
        let dir = temp_dir();
        let state = Arc::new(test_state(&dir));
        let task_id = TaskService::create(
            &state,
            "launch_game",
            TaskCategory::Session,
            Some("game-1".to_string()),
            "准备启动游戏",
        )
        .expect("create task");

        // cmd 只负责派生，真正的「游戏」是它拉起的 ping（跑满 30 秒，足够取消）。
        let child = Command::new("cmd")
            .args(["/C", "ping", "-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn root process");
        let root_pid = child.id();
        let child_pid = wait_for_descendant(root_pid).expect("根进程应派生出子进程");
        assert!(crate::services::process_service::is_process_running(
            child_pid
        ));

        let wait_state = Arc::clone(&state);
        let wait_task_id = task_id.clone();
        let wait_dir = dir.clone();
        let session = thread::spawn(move || {
            let mut root = SessionRoot::Spawned(child);
            wait_for_game_session(&mut root, &wait_dir, &wait_state, &wait_task_id)
        });

        // 留出至少一轮扫描，确保子进程已经进了跟踪集合。
        thread::sleep(Duration::from_millis(2000));
        TaskService::cancel(&state, &task_id).expect("cancel task");

        let end = session
            .join()
            .expect("wait thread should not panic")
            .expect("wait should succeed");
        assert!(
            matches!(&end, GameSessionEnd::Cancelled),
            "取消后应报告 Cancelled，实际：{end:?}"
        );
        assert!(
            !crate::services::process_service::is_process_running(child_pid),
            "取消后子进程不应仍然存活（pid={child_pid}）"
        );
    }

    /// 修复不能把正常路径弄坏：游戏自己退出时仍应报告 Exited，且任务保持未取消。
    #[test]
    #[cfg(target_os = "windows")]
    fn a_session_that_ends_on_its_own_reports_exited() {
        let dir = temp_dir();
        let state = test_state(&dir);
        let task_id = TaskService::create(
            &state,
            "launch_game",
            TaskCategory::Session,
            Some("game-1".to_string()),
            "准备启动游戏",
        )
        .expect("create task");

        let child = Command::new("ping")
            .args(["-n", "1", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn short lived process");

        let mut root = SessionRoot::Spawned(child);
        let end = wait_for_game_session(&mut root, &dir, &state, &task_id).expect("wait succeeds");
        assert!(
            matches!(&end, GameSessionEnd::Exited(_)),
            "自然结束应报告 Exited，实际：{end:?}"
        );
        assert!(!TaskService::is_cancelled(&state, &task_id));
    }

    /// 重建的发现逻辑：进程还在、运行时标记丢了，就应当被认出来并接手。
    ///
    /// 刻意把可执行文件复制进「游戏目录」再运行 —— 发现靠的是进程映像路径落在
    /// `managed_path` 之下，只有这样构造才真的走得到那条路径。
    #[test]
    #[cfg(target_os = "windows")]
    fn restore_finds_a_game_whose_process_outlived_the_app() {
        let dir = temp_dir();
        let system_ping = Path::new("C:/Windows/System32/ping.exe");
        if !system_ping.is_file() {
            return;
        }
        let game_dir = dir.join("game");
        std::fs::create_dir_all(&game_dir).expect("create game dir");
        let executable = game_dir.join("ping.exe");
        std::fs::copy(system_ping, &executable).expect("copy ping into game dir");

        let mut child = Command::new(&executable)
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn game process");
        let pid = child.id();

        let state = test_state(&dir);
        {
            let mut store = state.store.lock().expect("lock store");
            store.games.push(Game {
                game_uid: "game-1".to_string(),
                game_key: "game-1".to_string(),
                display_name: "样例游戏".to_string(),
                managed_path: game_dir.to_string_lossy().to_string(),
                lifecycle: GameLifecycle::Active,
                health: GameHealth::default(),
                cloud_status: CloudStatus::default(),
                launch: LaunchConfig::default(),
                cover: None,
                save_profile_id: None,
                last_played_at: None,
                latest_save_version_id: None,
                added_at: None,
            });
        }

        let found = adoptable_sessions(&state);
        assert!(
            found.iter().any(|(game, _)| game.game_uid == "game-1"),
            "进程仍在游戏目录里运行，应当被发现"
        );
        assert_eq!(found.len(), 1, "只应命中这一个游戏");
        drop(found);

        let _ = child.kill();
        let _ = child.wait();
        assert!(
            !crate::services::process_service::is_process_running(pid),
            "测试收尾应终止游戏进程（pid={pid}）"
        );
    }

    /// 接手的会话在游戏自然退出后应报告 `Exited`，且**没有退出码** ——
    /// 进程不是我们的子进程，没有可回收的退出状态。
    #[test]
    #[cfg(target_os = "windows")]
    fn an_adopted_session_reports_exited_without_an_exit_code() {
        let dir = temp_dir();
        let state = test_state(&dir);
        let task_id = TaskService::create(
            &state,
            "launch_game",
            TaskCategory::Session,
            Some("game-1".to_string()),
            "准备启动游戏",
        )
        .expect("create task");

        let mut child = Command::new("ping")
            .args(["-n", "2", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn short lived process");
        let pid = child.id();
        let handle = TrackedProcessHandle::open(pid).expect("hold handle");
        let mut root = SessionRoot::Adopted { pid, handle };

        let end = wait_for_game_session(&mut root, &dir, &state, &task_id).expect("wait succeeds");
        assert!(
            matches!(&end, GameSessionEnd::Exited(None)),
            "接手的会话自然结束应报告 Exited(None)，实际：{end:?}"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    /// 取消一场接手的会话，必须能终止那个**不是我们子进程**的游戏。
    #[test]
    #[cfg(target_os = "windows")]
    fn cancelling_an_adopted_session_terminates_the_process() {
        let dir = temp_dir();
        let state = Arc::new(test_state(&dir));
        let task_id = TaskService::create(
            &state,
            "launch_game",
            TaskCategory::Session,
            Some("game-1".to_string()),
            "准备启动游戏",
        )
        .expect("create task");

        let mut child = Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn long lived process");
        let pid = child.id();
        let handle = TrackedProcessHandle::open(pid).expect("hold handle");
        let mut root = SessionRoot::Adopted { pid, handle };

        let wait_state = Arc::clone(&state);
        let wait_task_id = task_id.clone();
        let wait_dir = dir.clone();
        let session = thread::spawn(move || {
            wait_for_game_session(&mut root, &wait_dir, &wait_state, &wait_task_id)
        });

        thread::sleep(Duration::from_millis(1000));
        TaskService::cancel(&state, &task_id).expect("cancel task");

        let end = session
            .join()
            .expect("wait thread should not panic")
            .expect("wait should succeed");
        assert!(
            matches!(&end, GameSessionEnd::Cancelled),
            "取消后应报告 Cancelled，实际：{end:?}"
        );
        assert!(
            !crate::services::process_service::is_process_running(pid),
            "取消后接手的游戏进程不应仍然存活（pid={pid}）"
        );
        // 进程已被取消流程终止，这里只是回收它的退出状态。
        let _ = child.wait();
    }
}
