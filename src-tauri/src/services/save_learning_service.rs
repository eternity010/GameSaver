use crate::domain::path_utils::{normalize_path, strip_verbatim_prefix};
use crate::domain::save_candidate::{
    is_generic_config_file, is_noise_path, is_save_candidate, is_save_container_directory,
    path_has_save_container_ancestor, path_has_segment, should_ignore_event_path, NAME_HINTS,
    RESOURCE_EXTENSIONS, SAVE_EXTENSIONS,
};
use crate::domain::{
    ActiveLearningSession, EtwCaptureHandle, FileFingerprint, Game, LearningSessionView,
    LearningStatus, SaveCandidateEvidenceLevel, SaveLearningResult, SaveRootType, SaveScope,
    SaveScopeDraft, UnknownFilePolicy, DEFAULT_EXCLUDE_DIRECTORIES, DEFAULT_EXCLUDE_PATTERNS,
    DEFAULT_MAX_FILE_BYTES,
};
use crate::services::learning::{
    collect_related_files_by_trace, extend_tracked_process_tree, stop_etw_capture,
    try_start_etw_capture, FileOperation, FileOperationKind,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::AppHandle;
use uuid::Uuid;
use walkdir::WalkDir;

/// 「这个文件像不像存档」的**判定**上限，只用来放宽候选识别的视野，
/// 不等于我们愿意管理的体积 —— 真正入 scope 的上限是 `DEFAULT_MAX_FILE_BYTES`。
/// 两者刻意分开：判定宽松才不会漏掉大存档，管理上限才是实际承诺。
const MAX_CANDIDATE_FILE_BYTES: u64 = 100 * 1024 * 1024;
/// 单个范围最多列出多少个「疑似存档但本次未变化」的文件。
///
/// 这是给用户看的清单，不是收集清单；`%APPDATA%\<游戏名>` 这类目录通常只有几十个文件，
/// 上限定这么低是为了挡住病态目录（比如误把整个安装目录当成范围根）拖慢分析。
const MAX_PROPOSED_FILES: usize = 200;
/// 只读初稿的采集模式标记。它不是一次真正的采集 —— 单独取值是为了让前端能区分文案，
/// 也让 `calculate_learning_confidence` 不给它任何 ETW 加分（`_ => 0`）。
const PREVIEW_CAPTURE_MODE: &str = "preview";
/// ETW 会话的兜底时长上限。
///
/// `logman` 原生的 `-rf`（运行指定时长）与 `-ets` 互斥 —— 本机实测报「参数"rf"不允许具有
/// 其他指定的参数」—— 所以时长上限只能在 Rust 侧兜。它是采集资源上界里**由我们掌握的那
/// 一半**：`logman -max` 也给了磁盘上界，但那条没法在本机验证（创建 ETW 会话需要管理员），
/// 而这条完全可控，且到点后 `.etl` 就不再增长。
///
/// 取 30 分钟：正常会话是「启动游戏 → 手动存一次档 → 点分析」，量级是分钟；30 分钟只会在
/// 用户忘了结束会话时触发。触发后**已采到的证据全部保留**，用户仍可正常点分析，此后的写入
/// 由快照差异兜底。
const MAX_CAPTURE_DURATION: Duration = Duration::from_secs(30 * 60);
const GENERIC_NAME_BLACKLIST: [&str; 38] = [
    "game",
    "games",
    "play",
    "player",
    "start",
    "launch",
    "launcher",
    "app",
    "application",
    "main",
    "run",
    "runner",
    "client",
    "test",
    "demo",
    "patch",
    "update",
    "edition",
    "version",
    "ver",
    "setup",
    "install",
    "installer",
    "steam",
    "epic",
    "gog",
    "final",
    "release",
    "debug",
    "unity",
    "unreal",
    "shipping",
    "win64",
    "win32",
    "windows",
    "x64",
    "x86",
    "default",
];

fn is_generic_hint(token: &str) -> bool {
    let lower = token.trim().to_ascii_lowercase();
    GENERIC_NAME_BLACKLIST.contains(&lower.as_str())
}

fn is_managed_game_asset_dir(entry_path: &Path) -> bool {
    let name = match entry_path.file_name().and_then(|s| s.to_str()) {
        Some(s) => s.to_ascii_lowercase(),
        None => return false,
    };
    matches!(
        name.as_str(),
        "assets"
            | "asset"
            | "content"
            | "movies"
            | "movie"
            | "audio"
            | "sound"
            | "sounds"
            | "textures"
            | "texture"
            | "shaders"
            | "shader"
            | "paks"
            | "pak"
            | "streamingassets"
            | "locale"
            | "locales"
            | "localization"
    )
}

pub struct SaveLearningService;

impl SaveLearningService {
    /// 只读推断一份存档范围初稿：不启动游戏、不采集 ETW、没有任何写入证据。
    ///
    /// 给「跳过学习」用 —— 用户可以先拿到一份待确认的草稿直接进审阅界面，不必先跑完
    /// 「启动游戏 → 手动存一次档 → 点分析」。草稿的证据等级一律是 `Review`。
    pub fn preview(game: &Game) -> Result<SaveLearningResult, String> {
        preview_scope_drafts(game)
    }

    pub fn start(
        app: &AppHandle,
        game: &Game,
        on_progress: impl Fn(u8, &str),
        is_cancelled: impl Fn() -> bool,
    ) -> Result<ActiveLearningSession, String> {
        Self::start_with_roots(
            app,
            game,
            discover_scan_roots(game)?,
            false,
            on_progress,
            is_cancelled,
        )
    }

    pub fn start_candidate_verification(
        app: &AppHandle,
        game: &Game,
        scopes: &[SaveScope],
        on_progress: impl Fn(u8, &str),
        is_cancelled: impl Fn() -> bool,
    ) -> Result<ActiveLearningSession, String> {
        let mut seen = HashSet::new();
        let mut roots = Vec::new();
        for scope in scopes {
            let path = PathBuf::from(&scope.root_path);
            let canonical = path
                .canonicalize()
                .map_err(|error| format!("无法验证存档目录 {}：{error}", scope.root_path))?;
            if !canonical.is_dir() {
                return Err(format!("无法验证存档目录：{} 不是目录", scope.root_path));
            }
            if seen.insert(normalize_path(&canonical)) {
                roots.push(crate::domain::ScanRoot {
                    root_type: scope.root_type,
                    physical_path: canonical,
                });
            }
        }
        if roots.is_empty() {
            return Err("至少选择一个待确认的存档范围".to_string());
        }
        Self::start_with_roots(app, game, roots, true, on_progress, is_cancelled)
    }

    fn start_with_roots(
        app: &AppHandle,
        game: &Game,
        roots: Vec<crate::domain::ScanRoot>,
        validation_only: bool,
        on_progress: impl Fn(u8, &str),
        is_cancelled: impl Fn() -> bool,
    ) -> Result<ActiveLearningSession, String> {
        let executable_path = managed_executable_path(game)?;
        if !executable_path.is_file() {
            return Err("受管游戏的启动程序不存在，请先修复游戏本体目录".to_string());
        }
        crate::logging::info(format!(
            "存档{}范围：game_uid={} roots={} paths={}",
            if validation_only {
                "再次验证"
            } else {
                "学习扫描"
            },
            game.game_uid,
            roots.len(),
            roots
                .iter()
                .map(|root| root.physical_path.display().to_string())
                .collect::<Vec<_>>()
                .join(" | ")
        ));
        on_progress(
            5,
            &format!(
                "已确定 {} 个{}范围",
                roots.len(),
                if validation_only {
                    "待确认存档"
                } else {
                    "存档扫描"
                }
            ),
        );
        on_progress(
            8,
            if validation_only {
                "正在记录待确认存档的保存前快照"
            } else {
                "正在记录保存前快照基线"
            },
        );
        let baseline = Some(collect_snapshot(
            &roots,
            |progress, message| on_progress(8 + (progress as u32 * 45 / 100) as u8, message),
            &is_cancelled,
        )?);
        let session_id = Uuid::new_v4().to_string();
        let mut etw_start_error = None;
        let etw_capture = match try_start_etw_capture(app, &session_id) {
            Ok(handle) => {
                on_progress(56, "ETW 已启动，准备记录游戏文件操作");
                Some(handle)
            }
            Err(error) => {
                etw_start_error = Some(error);
                on_progress(
                    56,
                    if validation_only {
                        "ETW 不可用，将使用候选目录快照继续验证"
                    } else {
                        "ETW 不可用，将使用快照差异继续学习"
                    },
                );
                None
            }
        };
        if etw_capture.is_some() {
            on_progress(
                57,
                if validation_only {
                    "ETW 优先验证：同时具备快照基线兜底"
                } else {
                    "ETW 优先模式：同时具备快照基线兜底"
                },
            );
        }
        // 采集期原本没有任何时长上界，`logman` 的 `-rf` 又与 `-ets` 互斥，只能在这里兜。
        arm_capture_watchdog(etw_capture.as_ref(), spawn_capture_watchdog);
        if is_cancelled() {
            if let Some(handle) = etw_capture.as_ref() {
                let _ = stop_etw_capture(&handle.trace_name);
                let _ = std::fs::remove_file(&handle.etl_path);
            }
            return Err("任务已取消".to_string());
        }
        on_progress(58, "正在启动游戏");
        let working_directory = game
            .launch
            .working_directory_relative_path
            .as_deref()
            .map(|relative| safe_join(Path::new(&game.managed_path), relative))
            .transpose()?
            .unwrap_or_else(|| {
                executable_path
                    .parent()
                    .unwrap_or(Path::new(&game.managed_path))
                    .to_path_buf()
            });
        let mut command = Command::new(&executable_path);
        command
            .args(&game.launch.arguments)
            .current_dir(&working_directory);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                if let Some(handle) = etw_capture.as_ref() {
                    let _ = stop_etw_capture(&handle.trace_name);
                    let _ = std::fs::remove_file(&handle.etl_path);
                }
                return Err(format!("启动游戏失败：{err}"));
            }
        };
        let root_pid = child.id();
        let mut tracked_pid_set = HashSet::from([root_pid]);
        if let Err(error) = extend_tracked_process_tree(&mut tracked_pid_set) {
            etw_start_error.get_or_insert(format!("进程树扩展失败：{error}"));
        }
        let managed_path = PathBuf::from(&game.managed_path);
        let dir_pids = crate::services::process_service::find_processes_in_directory(&managed_path);
        tracked_pid_set.extend(dir_pids);
        let tracked_pids = Arc::new(Mutex::new(sorted_pids(tracked_pid_set)));
        let process_tracker_stop = Arc::new(AtomicBool::new(false));
        let process_tracker_done = Arc::new(AtomicBool::new(false));
        spawn_process_tracker(
            Arc::clone(&tracked_pids),
            Arc::clone(&process_tracker_stop),
            Arc::clone(&process_tracker_done),
            Some(managed_path),
        );
        let view = LearningSessionView {
            session_id,
            game_uid: game.game_uid.clone(),
            root_pid,
            started_at: now_iso(),
            status: LearningStatus::Capturing,
        };
        on_progress(100, "游戏已启动，请在游戏内完成一次保存");
        Ok(ActiveLearningSession {
            view,
            roots,
            baseline,
            tracked_pids,
            process_tracker_stop,
            process_tracker_done,
            etw_capture,
            etw_start_error,
            validation_only,
        })
    }

    pub fn finish(
        active: &ActiveLearningSession,
        on_progress: impl Fn(u8, &str),
        is_cancelled: impl Fn() -> bool,
    ) -> Result<SaveLearningResult, String> {
        on_progress(10, "正在读取保存后的文件状态");
        active.process_tracker_stop.store(true, Ordering::Release);
        wait_for_process_tracker(&active.process_tracker_done);
        let mut tracked_pid_set = active
            .tracked_pids
            .lock()
            .map_err(|_| "读取游戏进程树失败".to_string())?
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        let _ = extend_tracked_process_tree(&mut tracked_pid_set);
        if let Some(managed_root) = active
            .roots
            .iter()
            .find(|r| r.root_type == crate::domain::SaveRootType::ManagedGame)
        {
            let dir_pids = crate::services::process_service::find_processes_in_directory(
                &managed_root.physical_path,
            );
            tracked_pid_set.extend(dir_pids);
        }
        let tracked_pids = sorted_pids(tracked_pid_set);
        let any_running = tracked_pids
            .iter()
            .any(|&pid| crate::services::process_service::is_process_running(pid));
        if any_running {
            crate::logging::info(
                "分析存档时检测到游戏进程仍在运行，等待短缓冲以确保磁盘写入完成...",
            );
            std::thread::sleep(Duration::from_millis(500));
        }
        let mut etw_files = HashSet::new();
        let mut etw_operations = Vec::new();
        let mut notes = Vec::new();
        let mut event_capture_mode = "snapshot".to_string();
        if let Some(capture) = active.etw_capture.as_ref() {
            notes.push(if active.validation_only {
                "再次验证只观察已选候选目录".to_string()
            } else {
                "ETW 优先模式：依据 ETW 写入证据生成候选范围".to_string()
            });
            match collect_related_files_by_trace(
                Some(&capture.trace_name),
                Some(&capture.etl_path.to_string_lossy()),
                &tracked_pids,
            ) {
                Ok(collection) => {
                    event_capture_mode = "etw".to_string();
                    etw_files = collection.files;
                    etw_operations = collection.operations;
                    notes.extend(collection.logs);
                }
                Err(error) => notes.push(format!("ETW 解析失败，已回退快照差异：{error}")),
            }
            let _ = std::fs::remove_file(&capture.etl_path);
        } else if let Some(error) = active.etw_start_error.as_ref() {
            notes.push(format!("ETW 未启动，已使用快照差异：{error}"));
        }
        if etw_files.is_empty() {
            event_capture_mode = "snapshot".to_string();
            notes.push("未捕获到有效 ETW 文件，已自动使用快照差异比对".to_string());
        }
        if event_capture_mode == "etw" && !active.validation_only {
            let etw_relevant_roots: Vec<_> = active
                .roots
                .iter()
                .filter(|r| {
                    r.root_type == SaveRootType::ManagedGame
                        || etw_files
                            .iter()
                            .any(|path| path_is_within_root(path, &r.physical_path))
                })
                .cloned()
                .collect();
            let fallback_files = discover_save_container_files(&etw_relevant_roots, &is_cancelled)?;
            if !fallback_files.is_empty() {
                let existing_count = etw_files.len();
                let added_count = fallback_files
                    .iter()
                    .filter(|path| !etw_files.contains(*path))
                    .count();
                if added_count > 0 {
                    notes.push(format!(
                        "已从游戏专属目录补充 {} 个常见存档容器文件（新增 {} 个）；候选仍可编辑",
                        fallback_files.len(),
                        added_count
                    ));
                    etw_files.extend(fallback_files);
                    crate::logging::info(format!(
                        "存档学习容器兜底：ETW 文件={}，补充文件={}，新增文件={}",
                        existing_count,
                        etw_files.len().saturating_sub(existing_count),
                        added_count
                    ));
                }
            } else {
                notes.push(
                    "ETW 未在已识别游戏目录找到常见存档容器，将仅使用 ETW 文件证据".to_string(),
                );
                crate::logging::info("存档学习容器兜底：未找到常见存档容器");
            }
        }
        let mut effective_roots = active.roots.clone();
        if !etw_files.is_empty() && !active.validation_only {
            let managed_path = active
                .roots
                .iter()
                .find(|r| r.root_type == SaveRootType::ManagedGame)
                .map(|r| r.physical_path.as_path());
            for etw_file in &etw_files {
                let path = Path::new(etw_file);
                if !effective_roots
                    .iter()
                    .any(|r| path_is_within_root(etw_file, &r.physical_path))
                {
                    if let Some(inferred) = infer_scan_root_for_etw_file(path, managed_path) {
                        effective_roots.push(inferred);
                    }
                }
            }
            let mut seen = HashSet::new();
            effective_roots.retain(|r| seen.insert(normalize_path(&r.physical_path)));
        }

        let baseline = active.baseline.as_ref();
        let final_snapshot = if etw_files.is_empty() {
            collect_snapshot(
                &effective_roots,
                |progress, message| on_progress(snapshot_analysis_progress(progress), message),
                &is_cancelled,
            )?
        } else {
            on_progress(45, "ETW 已定位文件，正在读取目标文件状态");
            let targeted = collect_targeted_snapshot(&effective_roots, &etw_files, &is_cancelled)?;
            if targeted.is_empty() {
                notes.push("ETW 文件无法直接读取，已回退完整快照差异".to_string());
                collect_snapshot(
                    &effective_roots,
                    |progress, message| on_progress(snapshot_analysis_progress(progress), message),
                    &is_cancelled,
                )?
            } else {
                targeted
            }
        };
        if is_cancelled() {
            return Err("任务已取消".to_string());
        }
        on_progress(92, "正在按文件夹整理存档候选");
        let mut active_effective = active.clone();
        active_effective.roots = effective_roots;
        // 事务摘要与范围证据都只看候选口径的操作；原始 `etw_operations` 只保留给
        // 「本次是否跑过采集」这个判断，不参与评分。
        let transaction_operations = transaction_evidence(&etw_operations);
        let (changed_files, scope_drafts, mut inference_notes) = infer_scope_drafts(
            &active_effective.roots,
            &final_snapshot,
            baseline,
            &etw_files,
            &transaction_operations,
        );
        notes.append(&mut inference_notes);
        let transaction_summary = (!etw_operations.is_empty() || active.etw_capture.is_some())
            .then(|| crate::services::learning::analyze_save_transactions(transaction_operations));
        let confidence = calculate_learning_confidence(
            &scope_drafts,
            &event_capture_mode,
            transaction_summary.as_ref(),
        );
        Ok(SaveLearningResult {
            session_id: active.view.session_id.clone(),
            changed_files,
            scope_drafts,
            confidence,
            notes,
            event_capture_mode,
            transaction_summary,
        })
    }
}

fn snapshot_analysis_progress(scan_progress: u8) -> u8 {
    10 + ((u16::from(scan_progress) * 4 / 5) as u8)
}

fn sorted_pids(pids: HashSet<u32>) -> Vec<u32> {
    let mut values = pids.into_iter().collect::<Vec<_>>();
    values.sort_unstable();
    values
}

#[cfg(target_os = "windows")]
fn any_process_alive(pids: &[u32]) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    const STILL_ACTIVE: u32 = 259;
    for &pid in pids {
        if pid == 0 {
            continue;
        }
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if !handle.is_null() {
                let mut exit_code = 0u32;
                let result = GetExitCodeProcess(handle, &mut exit_code);
                CloseHandle(handle);
                if result != 0 && exit_code == STILL_ACTIVE {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(not(target_os = "windows"))]
fn any_process_alive(_pids: &[u32]) -> bool {
    true
}

/// 到点后停止 ETW 会话，给 `.etl` 一个上界。
///
/// `sleep` / `stop` 做成注入点，是为了让「到点真的会去停会话」可被测试钉住 —— 否则这条
/// 防护只能靠读代码确认。`stop` 返回错误不算异常：会话已被正常结束或被取消时，看门狗到点
/// 再停一次必然拿到错误，此时只记日志。
fn run_capture_watchdog(
    trace_name: &str,
    duration: Duration,
    sleep: impl FnOnce(Duration),
    stop: impl FnOnce(&str) -> Result<(), String>,
) {
    sleep(duration);
    match stop(trace_name) {
        Ok(()) => crate::logging::info(format!(
            "ETW 会话已达时长上限（{} 分钟），已停止采集：{trace_name}",
            duration.as_secs() / 60
        )),
        Err(error) => crate::logging::info(format!(
            "ETW 会话已达时长上限，停止时返回（会话可能已结束）：{trace_name} {error}"
        )),
    }
}

/// 在后台线程里按 [`MAX_CAPTURE_DURATION`] 兜住一次采集的时长。
fn spawn_capture_watchdog(trace_name: String, duration: Duration) {
    thread::spawn(move || {
        run_capture_watchdog(&trace_name, duration, thread::sleep, stop_etw_capture);
    });
}

/// 装配采集看门狗：有句柄就按上限挂表，没有（ETW 不可用）就什么都不做。
///
/// 单独成函数是为了让「到底有没有把看门狗挂上去」可被断言。挂表动作本身只是
/// `thread::spawn`，删掉它不会有任何测试变红 —— 那样这条防护就会悄无声息地消失，
/// 而它正是 `.etl` 唯一由我们掌控的时长上界。
fn arm_capture_watchdog(capture: Option<&EtwCaptureHandle>, arm: impl FnOnce(String, Duration)) {
    if let Some(handle) = capture {
        arm(handle.trace_name.clone(), MAX_CAPTURE_DURATION);
    }
}

fn spawn_process_tracker(
    tracked_pids: Arc<Mutex<Vec<u32>>>,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    managed_path: Option<PathBuf>,
) {
    thread::spawn(move || {
        let mut iterations: usize = 0;
        loop {
            let all_exited = if let Ok(mut current) = tracked_pids.lock() {
                let alive = any_process_alive(&current);
                if alive || current.is_empty() {
                    refresh_tracked_processes(&mut current, managed_path.as_deref());
                }
                !current.is_empty() && !alive
            } else {
                false
            };
            if stop.load(Ordering::Acquire) {
                if let Ok(mut current) = tracked_pids.lock() {
                    refresh_tracked_processes(&mut current, managed_path.as_deref());
                }
                done.store(true, Ordering::Release);
                break;
            }
            iterations += 1;
            let sleep_duration = if all_exited {
                Duration::from_millis(500)
            } else if iterations < 10 {
                Duration::from_millis(500)
            } else {
                Duration::from_secs(1)
            };
            thread::sleep(sleep_duration);
        }
    });
}

fn refresh_tracked_processes(current: &mut Vec<u32>, managed_path: Option<&Path>) {
    let mut process_set = current.iter().copied().collect::<HashSet<_>>();
    let _ = extend_tracked_process_tree(&mut process_set);
    if let Some(dir) = managed_path {
        let dir_pids = crate::services::process_service::find_processes_in_directory(dir);
        process_set.extend(dir_pids);
    }
    *current = sorted_pids(process_set);
}

fn wait_for_process_tracker(done: &AtomicBool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !done.load(Ordering::Acquire) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
}

fn safe_join(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let rel_path = Path::new(relative);
    if rel_path.is_absolute()
        || rel_path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err("相对路径无效，禁止使用绝对路径或父级引用".to_string());
    }
    Ok(root.join(rel_path))
}

fn managed_executable_path(game: &Game) -> Result<PathBuf, String> {
    let root = Path::new(&game.managed_path);
    let relative = Path::new(&game.launch.executable_relative_path);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err("启动程序相对路径无效".to_string());
    }
    Ok(root.join(relative))
}

fn discover_scan_roots(game: &Game) -> Result<Vec<crate::domain::ScanRoot>, String> {
    let managed_path = strip_verbatim_prefix(
        &PathBuf::from(&game.managed_path)
            .canonicalize()
            .map_err(|err| format!("解析受管游戏目录失败：{err}"))?,
    );
    let mut roots = vec![crate::domain::ScanRoot {
        root_type: SaveRootType::ManagedGame,
        physical_path: managed_path,
    }];
    let hints = game_name_hints(game);
    for (root, root_type) in environment_scan_roots() {
        for path in find_candidate_directories(&root, &hints) {
            roots.push(crate::domain::ScanRoot {
                root_type,
                physical_path: path,
            });
        }
    }
    if let Ok(profile) = env::var("USERPROFILE") {
        let profile_root = PathBuf::from(profile);
        let documents = profile_root.join("Documents");
        if documents.is_dir() {
            for path in find_candidate_directories(&documents, &hints) {
                roots.push(crate::domain::ScanRoot {
                    root_type: SaveRootType::Documents,
                    physical_path: path,
                });
            }
        }
        let saved_games = profile_root.join("Saved Games");
        if saved_games.is_dir() {
            for path in find_candidate_directories(&saved_games, &hints) {
                roots.push(crate::domain::ScanRoot {
                    root_type: SaveRootType::SavedGames,
                    physical_path: path,
                });
            }
        }
        let local_low = profile_root.join("AppData").join("LocalLow");
        if local_low.is_dir() {
            for path in find_candidate_directories(&local_low, &hints) {
                roots.push(crate::domain::ScanRoot {
                    root_type: SaveRootType::LocalLow,
                    physical_path: path,
                });
            }
        }
    }
    if let Ok(public) = env::var("PUBLIC")
        .or_else(|_| env::var("SystemDrive").map(|d| format!(r"{d}\Users\Public")))
    {
        let public_root = PathBuf::from(public);
        let public_documents = public_root.join("Documents");
        if public_documents.is_dir() {
            for path in find_candidate_directories(&public_documents, &hints) {
                roots.push(crate::domain::ScanRoot {
                    root_type: SaveRootType::Documents,
                    physical_path: path,
                });
            }
            let steam_container = public_documents.join("Steam");
            if steam_container.is_dir() {
                for path in find_candidate_directories(&steam_container, &hints) {
                    roots.push(crate::domain::ScanRoot {
                        root_type: SaveRootType::Documents,
                        physical_path: path,
                    });
                }
            }
        }
    }
    for steam_dir in find_steam_userdata_dirs() {
        for path in find_candidate_directories(&steam_dir, &hints) {
            roots.push(crate::domain::ScanRoot {
                root_type: SaveRootType::Custom,
                physical_path: path,
            });
        }
    }
    let mut seen = HashSet::new();
    roots.retain(|root| seen.insert(normalize_path(&root.physical_path)));
    Ok(roots)
}

/// 学习会话要扫的「环境变量根」：变量名 + 根类型。
///
/// 单独列出来是为了让「扫哪些标准位置」可被断言 —— 漏掉一项就意味着那一类位置的存档
/// 无论走快照还是 ETW 都识别不出来，而症状只是「没有发现变化」，不会有任何报错。
/// `%PROGRAMDATA%` 就这么漏过一整个版本：全用户安装、老游戏、部分日系游戏把存档写在
/// `%PROGRAMDATA%\<厂商>\<游戏>`，它不受 UAC 文件虚拟化影响，是这几类游戏唯一稳定的
/// 落点。
///
/// 文档 / Saved Games / LocalLow / Public Documents / Steam `userdata` 不在这里 ——
/// 它们不是「一个环境变量直接指过去」的形态，在 `discover_scan_roots` 里单独处理。
const ENVIRONMENT_SCAN_ROOTS: [(&str, SaveRootType); 3] = [
    ("APPDATA", SaveRootType::AppData),
    ("LOCALAPPDATA", SaveRootType::LocalAppData),
    ("PROGRAMDATA", SaveRootType::ProgramData),
];

/// 把 `ENVIRONMENT_SCAN_ROOTS` 里当前机器确实存在的项解析成物理根。
fn environment_scan_roots() -> Vec<(PathBuf, SaveRootType)> {
    ENVIRONMENT_SCAN_ROOTS
        .iter()
        .filter_map(|(variable, root_type)| {
            let path = PathBuf::from(env::var_os(variable)?);
            path.is_dir().then_some((path, *root_type))
        })
        .collect()
}

fn find_steam_userdata_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut candidates = Vec::new();

    if let Some(pf86) = env::var_os("ProgramFiles(x86)") {
        candidates.push(PathBuf::from(pf86).join("Steam").join("userdata"));
    }
    if let Some(pf) = env::var_os("ProgramFiles") {
        candidates.push(PathBuf::from(pf).join("Steam").join("userdata"));
    }
    for letter in b'C'..=b'Z' {
        let drive = letter as char;
        candidates.push(PathBuf::from(format!(r"{drive}:\Steam\userdata")));
        candidates.push(PathBuf::from(format!(r"{drive}:\Games\Steam\userdata")));
    }

    let mut seen = HashSet::new();
    for candidate in candidates {
        if candidate.is_dir() {
            if let Ok(canonical) = candidate.canonicalize() {
                let norm = normalize_path(&canonical);
                if seen.insert(norm) {
                    dirs.push(strip_verbatim_prefix(&canonical));
                }
            } else if seen.insert(normalize_path(&candidate)) {
                dirs.push(candidate);
            }
        }
    }
    dirs
}

fn find_candidate_directories(root: &Path, hints: &[String]) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let mut frontier = vec![root.to_path_buf()];
    for depth in 0..3 {
        let mut next = Vec::new();
        for parent in frontier {
            let Ok(entries) = fs::read_dir(parent) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if !file_type.is_dir() || is_scan_noise_directory(&path) {
                    continue;
                }
                if hints.iter().any(|hint| directory_matches_hint(&path, hint)) {
                    candidates.push(path);
                } else if depth < 2 {
                    next.push(path);
                }
            }
        }
        frontier = next;
    }
    candidates
}

fn is_scan_noise_directory(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    [
        "temp",
        "cache",
        "logs",
        "crashdumps",
        "shadercache",
        "packages",
        "microsoft",
        "google",
        "mozilla",
        "nvidia",
        "epic games",
        "epicgameslauncher",
        "wegame",
        "tencent",
        "riot games",
        "ubisoft game launcher",
        "battle.net",
        "gog.com",
        "electronic arts",
        "ea desktop",
        "com.gamesaver.desktop",
        "com.gamesaver.next",
    ]
    .contains(&name.as_str())
}

fn infer_scan_root_for_etw_file(
    file_path: &Path,
    managed_path: Option<&Path>,
) -> Option<crate::domain::ScanRoot> {
    let norm_file = normalize_path(file_path);

    if !is_etw_candidate(&norm_file) {
        return None;
    }

    if let Some(managed) = managed_path {
        let norm_managed = normalize_path(managed);
        if norm_file.starts_with(&(norm_managed.clone() + "\\")) || norm_file == norm_managed {
            return Some(crate::domain::ScanRoot {
                root_type: SaveRootType::ManagedGame,
                physical_path: managed.to_path_buf(),
            });
        }
    }

    for ancestor in file_path.ancestors() {
        if let (Some(app_id_name), Some(parent)) = (ancestor.file_name(), ancestor.parent()) {
            if let (Some(_account_name), Some(grandparent)) = (parent.file_name(), parent.parent())
            {
                if let Some(user_data_name) = grandparent.file_name() {
                    if user_data_name
                        .to_string_lossy()
                        .eq_ignore_ascii_case("userdata")
                    {
                        let app_id_str = app_id_name.to_string_lossy();
                        if app_id_str.chars().all(|c| c.is_ascii_digit()) && app_id_str.len() >= 2 {
                            return Some(crate::domain::ScanRoot {
                                root_type: SaveRootType::Custom,
                                physical_path: ancestor.to_path_buf(),
                            });
                        }
                    }
                }
            }
        }
    }

    let profile_root = env::var_os("USERPROFILE").map(PathBuf::from)?;
    let app_data = env::var_os("APPDATA").map(PathBuf::from);
    let local_app_data = env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let program_data = env::var_os("PROGRAMDATA").map(PathBuf::from).or_else(|| {
        env::var_os("SystemDrive")
            .map(|d| PathBuf::from(format!(r"{}\ProgramData", d.to_string_lossy())))
    });
    let local_low = profile_root.join("AppData").join("LocalLow");
    let saved_games = profile_root.join("Saved Games");
    let documents = profile_root.join("Documents");
    let public_documents = env::var_os("PUBLIC")
        .or_else(|| {
            env::var_os("SystemDrive")
                .map(|d| format!(r"{}\Users\Public", d.to_string_lossy()).into())
        })
        .map(|p| PathBuf::from(p).join("Documents"));

    let bases: [(Option<PathBuf>, SaveRootType); 8] = [
        (Some(local_low), SaveRootType::LocalLow),
        (Some(saved_games), SaveRootType::SavedGames),
        (app_data, SaveRootType::AppData),
        (local_app_data, SaveRootType::LocalAppData),
        (program_data, SaveRootType::ProgramData),
        (Some(documents), SaveRootType::Documents),
        (public_documents, SaveRootType::Documents),
        (Some(profile_root.clone()), SaveRootType::UserProfile),
    ];

    for (base_opt, root_type) in bases {
        let Some(base) = base_opt else { continue };
        let norm_base = normalize_path(&base);
        if norm_file.starts_with(&(norm_base.clone() + "\\")) {
            let rel_str = norm_file[norm_base.len()..].trim_start_matches('\\');
            let components = rel_str.split('\\').collect::<Vec<_>>();
            let candidate_dir = if components.len() >= 3 && components[0] == "steam" {
                base.join(components[0])
                    .join(components[1])
                    .join(components[2])
            } else if components.len() >= 2 && components[0] == "my games" {
                base.join(components[0]).join(components[1])
            } else if root_type == SaveRootType::ProgramData && components.len() >= 3 {
                // `%PROGRAMDATA%\<厂商>\<游戏>\<文件>`：取厂商 + 游戏两级。要求至少三段，
                // 否则第二段就是文件名本身，取两级会把文件当成范围根。
                base.join(components[0]).join(components[1])
            } else if components.len() >= 2
                && (root_type == SaveRootType::LocalLow
                    || root_type == SaveRootType::LocalAppData
                    || root_type == SaveRootType::AppData)
                && components[0] != "saved"
                && components[0] != "save"
                && components[0] != "saves"
                && components[0] != "savedata"
            {
                base.join(components[0]).join(components[1])
            } else if !components.is_empty() {
                base.join(components[0])
            } else {
                base.clone()
            };
            return Some(crate::domain::ScanRoot {
                root_type,
                physical_path: candidate_dir,
            });
        }
    }
    None
}

fn game_name_hints(game: &Game) -> Vec<String> {
    let is_delimiter = |character: char| !character.is_alphanumeric();
    let mut hints = game
        .display_name
        .split(is_delimiter)
        .filter(|item| item.chars().count() >= 2 && !is_generic_hint(item))
        .map(|item| item.to_lowercase())
        .collect::<Vec<_>>();
    let compact_name = game
        .display_name
        .chars()
        .filter(|character| character.is_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    if compact_name.chars().count() >= 2
        && !is_generic_hint(&compact_name)
        && !hints.contains(&compact_name)
    {
        hints.push(compact_name);
    }
    if let Some(stem) = Path::new(&game.launch.executable_relative_path)
        .file_stem()
        .and_then(|value| value.to_str())
    {
        for part in stem.split(is_delimiter) {
            let part_lower = part.to_lowercase();
            if part_lower.chars().count() >= 2
                && !is_generic_hint(&part_lower)
                && !hints.contains(&part_lower)
            {
                hints.push(part_lower);
            }
        }
        let stem_lower = stem.to_lowercase();
        let compact_stem = stem_lower
            .chars()
            .filter(|character| character.is_alphanumeric())
            .collect::<String>();
        if stem_lower.chars().count() >= 2
            && !is_generic_hint(&stem_lower)
            && !hints.contains(&stem_lower)
        {
            hints.push(stem_lower);
        }
        if compact_stem.chars().count() >= 2
            && !is_generic_hint(&compact_stem)
            && !hints.contains(&compact_stem)
        {
            hints.push(compact_stem);
        }
    }
    let managed = Path::new(&game.managed_path);
    if managed.is_dir() {
        for entry in WalkDir::new(managed)
            .max_depth(4)
            .into_iter()
            .filter_map(Result::ok)
        {
            let file_name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if file_name == "steam_appid.txt" {
                if let Ok(content) = fs::read_to_string(entry.path()) {
                    let appid = content.trim();
                    if !appid.is_empty()
                        && appid.chars().all(|c| c.is_ascii_digit())
                        && !hints.contains(&appid.to_string())
                    {
                        hints.push(appid.to_string());
                    }
                }
            } else if file_name == "steam_emu.ini" {
                if let Ok(content) = fs::read_to_string(entry.path()) {
                    for line in content.lines() {
                        let trimmed = line.trim();
                        if trimmed.to_ascii_lowercase().starts_with("appid=") {
                            let appid = trimmed["appid=".len()..].trim();
                            if !appid.is_empty()
                                && appid.chars().all(|c| c.is_ascii_digit())
                                && !hints.contains(&appid.to_string())
                            {
                                hints.push(appid.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    hints
}

fn directory_matches_hint(path: &Path, hint: &str) -> bool {
    if is_save_container_directory(path) || is_scan_noise_directory(path) {
        return false;
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_lowercase();
    if is_generic_hint(&name) || is_generic_hint(hint) {
        return false;
    }
    let compact_name = name
        .chars()
        .filter(|character| character.is_alphanumeric())
        .collect::<String>();
    let compact_hint = hint
        .chars()
        .filter(|character| character.is_alphanumeric())
        .collect::<String>();
    if compact_hint.is_empty() {
        return false;
    }
    if name == hint || compact_name == compact_hint {
        return true;
    }
    if compact_hint.chars().count() >= 4 {
        if compact_name.starts_with(&compact_hint) {
            return true;
        }
        if name
            .split(['_', '-', '.', ' '])
            .any(|token| token == hint || token == compact_hint)
        {
            return true;
        }
        if compact_hint.chars().count() >= 5 && compact_name.contains(&compact_hint) {
            return true;
        }
    }
    false
}

fn discover_save_container_files(
    roots: &[crate::domain::ScanRoot],
    is_cancelled: &impl Fn() -> bool,
) -> Result<HashSet<String>, String> {
    let mut files = HashSet::new();
    for root in roots {
        if is_cancelled() {
            return Err("任务已取消".to_string());
        }
        if !root.physical_path.is_dir() {
            continue;
        }
        let mut containers = Vec::new();
        if is_save_container_directory(&root.physical_path) {
            containers.push(root.physical_path.clone());
        } else if root.root_type == SaveRootType::ManagedGame {
            let Ok(entries) = fs::read_dir(&root.physical_path) else {
                continue;
            };
            containers.extend(entries.filter_map(Result::ok).filter_map(|entry| {
                let path = entry.path();
                entry
                    .file_type()
                    .ok()
                    .filter(|kind| kind.is_dir())
                    .and_then(|_| is_save_container_directory(&path).then_some(path))
            }));
        } else {
            for entry in WalkDir::new(&root.physical_path)
                .follow_links(false)
                .max_depth(3)
            {
                let Ok(entry) = entry else {
                    continue;
                };
                if entry.file_type().is_dir() && is_save_container_directory(entry.path()) {
                    containers.push(entry.path().to_path_buf());
                }
            }
        }
        for container in containers {
            for entry in WalkDir::new(container).follow_links(false).max_depth(6) {
                if is_cancelled() {
                    return Err("任务已取消".to_string());
                }
                let Ok(entry) = entry else {
                    continue;
                };
                if !entry.file_type().is_file() || is_noise_path(entry.path()) {
                    continue;
                }
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };
                if metadata.len() > MAX_CANDIDATE_FILE_BYTES {
                    continue;
                }
                files.insert(normalize_path(entry.path()));
            }
        }
    }
    Ok(files)
}

fn collect_snapshot(
    roots: &[crate::domain::ScanRoot],
    on_progress: impl Fn(u8, &str),
    is_cancelled: &impl Fn() -> bool,
) -> Result<HashMap<String, FileFingerprint>, String> {
    let mut files = HashMap::new();
    for (root_index, root) in roots.iter().enumerate() {
        if !root.physical_path.is_dir() {
            continue;
        }
        let is_managed = root.root_type == SaveRootType::ManagedGame;
        let walker = if is_managed {
            WalkDir::new(&root.physical_path)
                .follow_links(false)
                .max_depth(4)
        } else {
            WalkDir::new(&root.physical_path).follow_links(false)
        };
        for entry in walker.into_iter().filter_entry(|e| {
            if is_noise_path(e.path()) {
                return false;
            }
            if is_managed && is_managed_game_asset_dir(e.path()) {
                return false;
            }
            true
        }) {
            if is_cancelled() {
                return Err("任务已取消".to_string());
            }
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    crate::logging::warn(format!("扫描跳过不可访问路径: {err}"));
                    continue;
                }
            };
            if !entry.file_type().is_file() || is_noise_path(entry.path()) {
                continue;
            }
            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if metadata.len() > MAX_CANDIDATE_FILE_BYTES {
                continue;
            }
            files.insert(
                normalize_path(entry.path()),
                FileFingerprint {
                    size: metadata.len(),
                    modified_unix: modified_unix(&metadata),
                },
            );
        }
        on_progress(
            ((root_index + 1) * 100 / roots.len().max(1)) as u8,
            &format!("已扫描第 {} 个范围", root_index + 1),
        );
    }
    Ok(files)
}

fn resolve_scope_root_and_relative(
    file_path: &Path,
    scan_root_path: &Path,
) -> Option<(PathBuf, String)> {
    let parent = file_path.parent()?;
    let mut current = parent;
    let mut chosen_root = None;
    let scan_root_norm = normalize_path(scan_root_path);

    loop {
        if is_save_container_directory(current) {
            chosen_root = Some(current.to_path_buf());
        }
        let current_norm = normalize_path(current);
        if current_norm == scan_root_norm || !current_norm.starts_with(&scan_root_norm) {
            break;
        }
        match current.parent() {
            Some(p) => current = p,
            None => break,
        }
    }

    let root = chosen_root.unwrap_or_else(|| parent.to_path_buf());
    let root_norm = normalize_path(&root);
    let file_norm = normalize_path(file_path);

    let relative = if file_norm.starts_with(&root_norm) {
        let remainder = file_norm[root_norm.len()..].trim_start_matches('\\');
        remainder.replace('\\', "/")
    } else {
        file_path.file_name()?.to_string_lossy().replace('\\', "/")
    };

    Some((root, relative))
}

/// 列出一个范围目录里「看起来是存档、但本次学习没有发生变化」的文件。
///
/// 只在范围根目录**不是**命名存档容器时使用：那种范围当前等于「学习那一刻的文件清单」，
/// 之后用户新建的档位、游戏写出的带时间戳自动存档都不会再进来，也没有任何提示。这里把
/// 目录里现存的疑似存档列出来交给用户在草稿页确认（`SaveScopeDraft::proposed_files`），
/// **由用户决定纳不纳入，而不是直接写进规则**。
///
/// 之所以不直接自动纳入：`is_save_candidate` 是启发式，误收别的存档（模拟器共享的
/// `SAVEDATA` 目录最典型）比漏收更难收拾 —— 用户会拿到一堆不属于这个游戏的版本。
fn propose_directory_saves(scope_root: &Path, confirmed: &[String]) -> Vec<String> {
    if !scope_root.is_dir() {
        return Vec::new();
    }
    let mut proposals = Vec::new();
    for entry in WalkDir::new(scope_root)
        .follow_links(false)
        .max_depth(3)
        .into_iter()
        .filter_entry(|entry| entry.depth() == 0 || !is_noise_path(entry.path()))
    {
        if proposals.len() >= MAX_PROPOSED_FILES {
            break;
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        // 提议是「确认后要不要一起管」，所以按管理上限而不是候选上限来筛：
        // 超过 `DEFAULT_MAX_FILE_BYTES` 的文件即便列出来也无从纳入。
        if metadata.len() > DEFAULT_MAX_FILE_BYTES {
            continue;
        }
        if !is_save_candidate(&normalize_path(entry.path())) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(scope_root) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if confirmed
            .iter()
            .any(|value| value.eq_ignore_ascii_case(&relative))
        {
            continue;
        }
        proposals.push(relative);
    }
    proposals.sort();
    proposals.dedup();
    proposals
}

/// 把「一次采集里变化的文件」归组成候选存档范围草稿。
///
/// 参数刻意收成 `roots` 而不是 `&ActiveLearningSession`：这个函数**从来只用得到
/// `active.roots`**，而只读初稿（`preview_scope_drafts`，没有会话）需要复用同一份归组与
/// 排除规则。如果为了初稿另写一份，两边的启发式必然随时间漂移 —— 正是评审 R2 教训里
/// 「同一个判定有两份实现」那类坑。
fn infer_scope_drafts(
    roots: &[crate::domain::ScanRoot],
    final_snapshot: &HashMap<String, FileFingerprint>,
    baseline: Option<&HashMap<String, FileFingerprint>>,
    etw_files: &HashSet<String>,
    etw_operations: &[FileOperation],
) -> (Vec<String>, Vec<SaveScopeDraft>, Vec<String>) {
    let mut changed = Vec::new();
    if let Some(baseline) = baseline {
        if etw_files.is_empty() {
            for (path, fingerprint) in final_snapshot {
                if baseline.get(path) != Some(fingerprint) {
                    changed.push(path.clone());
                }
            }
            for path in baseline.keys() {
                if !final_snapshot.contains_key(path) {
                    changed.push(path.clone());
                }
            }
        } else {
            for path in etw_files {
                if final_snapshot.contains_key(path) {
                    changed.push(path.clone());
                }
            }
        }
    } else {
        for path in etw_files {
            if final_snapshot.contains_key(path) {
                changed.push(path.clone());
            }
        }
    }
    changed.sort();
    changed.dedup();

    struct ScopeGroup {
        physical_path: PathBuf,
        count: usize,
        files: Vec<String>,
        noise_exact: Vec<String>,
        root_type: SaveRootType,
    }

    let mut groups: BTreeMap<String, ScopeGroup> = BTreeMap::new();
    let mut unhandled_noise: Vec<(String, &FileFingerprint)> = Vec::new();
    let mut oversize_skipped = 0usize;
    let ignored_system_noise = etw_files
        .iter()
        .filter(|path| is_noise_path(Path::new(path)))
        .count();

    struct CachedScanRoot<'a> {
        root: &'a crate::domain::ScanRoot,
        norm_path: String,
        components_count: usize,
    }
    let cached_roots: Vec<CachedScanRoot> = roots
        .iter()
        .map(|root| CachedScanRoot {
            root,
            norm_path: normalize_path(&root.physical_path),
            components_count: root.physical_path.components().count(),
        })
        .collect();

    for path in &changed {
        if !etw_files.is_empty() && !etw_files.contains(path) {
            continue;
        }
        let fingerprint = final_snapshot
            .get(path)
            .or_else(|| baseline.and_then(|b| b.get(path)));
        let Some(fingerprint) = fingerprint else {
            continue;
        };

        let candidate_by_etw = !etw_files.is_empty() && is_etw_candidate(path);
        let candidate_by_snapshot = etw_files.is_empty() && is_save_candidate(path);
        let is_candidate = fingerprint.size <= MAX_CANDIDATE_FILE_BYTES
            && (candidate_by_etw || candidate_by_snapshot);

        let Some(scan_root) = cached_roots
            .iter()
            .filter(|cr| {
                path == &cr.norm_path
                    || (path.starts_with(&cr.norm_path)
                        && path.as_bytes().get(cr.norm_path.len()) == Some(&b'\\'))
            })
            .max_by_key(|cr| cr.components_count)
            .map(|cr| cr.root)
        else {
            continue;
        };

        let file_path = Path::new(path);
        let Some((scope_root, relative)) =
            resolve_scope_root_and_relative(file_path, &scan_root.physical_path)
        else {
            continue;
        };

        if is_candidate {
            let key = normalize_path(&scope_root);
            let entry = groups.entry(key).or_insert_with(|| ScopeGroup {
                physical_path: scope_root,
                count: 0,
                files: Vec::new(),
                noise_exact: Vec::new(),
                root_type: scan_root.root_type,
            });
            entry.count += 1;
            if final_snapshot.contains_key(path) {
                // 超过管理上限的文件不能留在 confirmed_files 里：留下就是「看着受保护，
                // 实际既不进版本库也不受保护」的静默缺口。这里剔除并计数，稍后统一告知。
                if fingerprint.size > DEFAULT_MAX_FILE_BYTES {
                    oversize_skipped += 1;
                } else {
                    entry.files.push(relative);
                }
            }
        } else {
            unhandled_noise.push((path.clone(), fingerprint));
        }
    }

    for (noise_path_str, _) in unhandled_noise {
        let noise_path = Path::new(&noise_path_str);
        let noise_norm = normalize_path(noise_path);
        for group in groups.values_mut() {
            let group_norm = normalize_path(&group.physical_path);
            if noise_norm.starts_with(&group_norm) {
                let rel = noise_norm[group_norm.len()..]
                    .trim_start_matches('\\')
                    .replace('\\', "/");
                if !rel.is_empty() && !group.files.contains(&rel) {
                    group.noise_exact.push(rel);
                }
            }
        }
    }

    let mut drafts = Vec::new();
    for (_, mut group) in groups {
        group.files.sort();
        group.files.dedup();
        group.noise_exact.sort();
        group.noise_exact.dedup();

        let protects_container = is_save_container_directory(&group.physical_path);
        if group.files.is_empty() && !protects_container {
            continue;
        }

        let exclude_patterns: Vec<String> = DEFAULT_EXCLUDE_PATTERNS
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut exclude_directories: Vec<String> = DEFAULT_EXCLUDE_DIRECTORIES
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut exclude_exact: Vec<String> = Vec::new();

        for noise in group.noise_exact {
            if group.files.contains(&noise) {
                continue;
            }
            if let Some((dir, _)) = noise.split_once('/') {
                let dir_norm = dir.to_ascii_lowercase();
                // 同理：若已确认是存档的文件就住在这个子目录里，说明该目录不是纯噪音目录，
                // 排除整目录会连真存档一起挡掉。
                let dir_prefix = format!("{dir_norm}/");
                let holds_confirmed_file = group
                    .files
                    .iter()
                    .any(|file| file.to_ascii_lowercase().starts_with(&dir_prefix));
                if !holds_confirmed_file
                    && !exclude_directories
                        .iter()
                        .any(|d| d.to_ascii_lowercase() == dir_norm)
                {
                    exclude_directories.push(dir.to_string());
                }
            }
            // 刻意**不**按噪音文件的扩展名注入 `*.ext`：
            // `is_excluded` 同时喂收集侧和 `scope_matches_entry_*`（恢复归组），一旦出现
            // 一个 `settings.json` 就把 `*.json` 整族排除，同组里真存档若也是 `.json`
            // 会既不被备份、又让恢复找不到范围。宁可多留几个噪音文件（由精确路径与
            // 目录排除兜住），也不冒丢存档的风险。
            if !exclude_exact.contains(&noise) {
                exclude_exact.push(noise);
            }
        }

        // 策略不再按「是不是容器」分化：容器与否改由收集侧那道门从 `root_path` 现推
        // （见 `save_repository::scope_admits_directory_file`），这个字段从此只当**用户开关**用。
        //
        // 默认 Protect = 前端徽章上的「自动保护新存档」，也正是 R2b 补上的那一半：学习**之后**
        // 才出现的档位、游戏写出的带时间戳自动存档，会按候选启发式自动纳入版本库。用户想收窄成
        // 「仅保护已确认文件」，在向导里把这个范围切到 Ignore 即可。
        let unknown_file_policy = UnknownFilePolicy::Protect;

        let physical_root_str = strip_verbatim_prefix(&group.physical_path)
            .to_string_lossy()
            .replace('/', "\\");

        let (evidence_level, evidence_reason, confidence) = classify_scope_evidence(
            &group.physical_path,
            group.count,
            etw_files.is_empty(),
            etw_operations,
        );
        // 提议是 **Ignore 档的兜底**：Protect 档下这些文件已经由收集侧自动纳入了，前端不再展示
        // 这份清单；但数据照常算出来 —— 用户在向导里把范围切回 Ignore 时立刻可用，不必重跑学习。
        let proposed_files = if protects_container {
            Vec::new()
        } else {
            propose_directory_saves(&group.physical_path, &group.files)
        };
        drafts.push(SaveScopeDraft {
            scope: SaveScope {
                root_type: group.root_type,
                root_path: physical_root_str,
                confirmed_files: group.files.clone(),
                // 所有范围都开目录级收集。容器范围整目录收（行为不变）；普通目录（典型是
                // `%APPDATA%\<游戏名>`）只收「像存档的」—— 由 `scope_admits_directory_file`
                // 那道门把关，收集侧与恢复侧共用同一份判定。
                include_directories: vec![".".to_string()],
                exclude_exact,
                exclude_patterns,
                exclude_directories,
                unknown_file_policy,
                max_file_bytes: Some(DEFAULT_MAX_FILE_BYTES),
            },
            changed_files: group.files,
            proposed_files,
            confidence,
            evidence_level,
            evidence_reason,
        });
    }

    let limit_mb = DEFAULT_MAX_FILE_BYTES / (1024 * 1024);
    let mut notes = vec![(if etw_files.is_empty() {
        "当前使用快照差异按文件夹归类，已自动注入标准排除规则与伴生噪音过滤。"
    } else {
        "当前优先使用 ETW 写入证据按文件夹归类，已自动注入标准排除规则与伴生噪音过滤。"
    })
    .to_string()];
    if drafts.is_empty() {
        notes.push(
            "没有发现符合存档特征的变化，请确认游戏内完成了一次保存，或手动添加存档目录。"
                .to_string(),
        );
    } else {
        notes.push(format!(
            "默认只保护 {limit_mb} MB 以内的存档文件，大文件、日志与噪音缓存已自动排除。"
        ));
    }
    if oversize_skipped > 0 {
        notes.push(format!(
            "已跳过 {oversize_skipped} 个超过 {limit_mb} MB 的文件：它们不会进入版本库，恢复时也不受保护。"
        ));
    }
    if ignored_system_noise > 0 {
        notes.push(format!(
            "已忽略 {ignored_system_noise} 项系统噪声文件，不会纳入存档候选。"
        ));
    }
    let proposed_total: usize = drafts.iter().map(|draft| draft.proposed_files.len()).sum();
    if proposed_total > 0 {
        notes.push(format!(
            "目录里还有 {proposed_total} 个疑似存档、本次没有变化：它们不会被自动纳入，确认后可在对应范围里勾选。"
        ));
    }
    (changed, drafts, notes)
}

/// 从「目录现状」推断候选范围初稿 —— 不启动游戏、不采集 ETW、没有任何写入证据。
///
/// 这是评审 A1 第 4 项「允许跳过学习」的落地：容器名启发式 + 扩展名白名单已经能给出可用的
/// 初稿，用户不该被强制走完「启动游戏 → 手动存一次档 → 点分析」才能进到确认界面。
///
/// **证据口径必须诚实**：这条路完全没有写入证据，所以草稿一律降为 `Review`，并且不谎称
/// 「本次学习有文件变化」。宁可让用户觉得「这只是一份草稿」，也不能让一份猜出来的范围看起来
/// 像被证据确认过。
fn preview_drafts_from_roots(
    roots: &[crate::domain::ScanRoot],
    snapshot: &HashMap<String, FileFingerprint>,
) -> (Vec<SaveScopeDraft>, Vec<String>) {
    // 空 baseline + 空 etw_files ⇒ `infer_scope_drafts` 把快照里的每个文件都当「待判定」，
    // 逐文件过 `is_save_candidate`（**快照口径**，不是 ETW 口径）。这正是只读初稿的语义：
    // 「这些目录里哪些文件看起来像存档」。若走 ETW 口径，反而会把没有写入证据的文件判成候选。
    let empty_baseline = HashMap::new();
    let etw_files = HashSet::new();
    let (_, mut drafts, notes) =
        infer_scope_drafts(roots, snapshot, Some(&empty_baseline), &etw_files, &[]);

    for draft in &mut drafts {
        draft.evidence_level = SaveCandidateEvidenceLevel::Review;
        draft.evidence_reason =
            "只读初稿：没有启动游戏、没有任何写入证据，仅按目录名与文件特征推断。请确认内容，或改回完整识别。"
                .to_string();
    }
    (drafts, notes)
}

/// 只读推断一份存档范围初稿（不启动游戏、不采集 ETW）。
fn preview_scope_drafts(game: &Game) -> Result<SaveLearningResult, String> {
    let roots = discover_scan_roots(game)?;
    if roots.is_empty() {
        return Err("没有推断出可扫描的存档目录，请改用完整识别。".to_string());
    }
    let snapshot = collect_snapshot(&roots, |_, _| {}, &|| false)?;
    let (drafts, mut notes) = preview_drafts_from_roots(&roots, &snapshot);
    notes.insert(
        0,
        format!(
            "只读初稿：没有启动游戏，也没有记录任何写入证据，仅按 {} 个候选目录的目录名与文件特征推断。",
            roots.len()
        ),
    );
    if drafts.is_empty() {
        notes.push("初稿没有推断出候选范围，建议改用完整识别，或手动添加存档目录。".to_string());
    }
    let confidence = calculate_learning_confidence(&drafts, PREVIEW_CAPTURE_MODE, None);
    Ok(SaveLearningResult {
        session_id: String::new(),
        // 没有观察任何变化，就不能报「N 个文件发生变化」。
        changed_files: Vec::new(),
        scope_drafts: drafts,
        confidence,
        notes,
        event_capture_mode: PREVIEW_CAPTURE_MODE.to_string(),
        transaction_summary: None,
    })
}

fn classify_scope_evidence(
    scope_root: &Path,
    file_count: usize,
    snapshot_only: bool,
    operations: &[FileOperation],
) -> (SaveCandidateEvidenceLevel, String, u8) {
    if snapshot_only {
        return (
            SaveCandidateEvidenceLevel::Review,
            "仅有候选目录快照变化，建议再次保存确认。".to_string(),
            if file_count >= 2 { 70 } else { 60 },
        );
    }

    let mut has_write = false;
    let mut has_close = false;
    let mut has_rename = false;
    let mut has_create = false;
    for operation in operations {
        if !path_is_within_root(&operation.path, scope_root) {
            continue;
        }
        match operation.operation {
            FileOperationKind::Write => has_write = true,
            FileOperationKind::Close => has_close = true,
            FileOperationKind::Rename => has_rename = true,
            FileOperationKind::Create => has_create = true,
            FileOperationKind::Delete | FileOperationKind::Unknown => {}
        }
    }

    if (has_write && (has_close || has_rename)) || has_rename {
        return (
            SaveCandidateEvidenceLevel::Strong,
            "ETW 已确认写入并完成关闭或重命名。".to_string(),
            if has_rename { 92 } else { 88 },
        );
    }
    if has_write || has_close || has_rename || has_create {
        return (
            SaveCandidateEvidenceLevel::Review,
            "检测到 ETW 文件操作，但缺少完整保存提交证据。".to_string(),
            if file_count >= 2 { 75 } else { 68 },
        );
    }
    (
        SaveCandidateEvidenceLevel::Review,
        "文件命中候选范围，但未关联到可确认的保存操作。".to_string(),
        if file_count >= 2 { 65 } else { 58 },
    )
}

fn calculate_learning_confidence(
    scope_drafts: &[SaveScopeDraft],
    event_capture_mode: &str,
    transaction_summary: Option<&crate::domain::SaveTransactionSummary>,
) -> u8 {
    if scope_drafts.is_empty() {
        return 0;
    }

    let max_draft_confidence = scope_drafts
        .iter()
        .map(|draft| draft.confidence)
        .max()
        .unwrap_or(65);

    let has_named_save_container = scope_drafts
        .iter()
        .any(|draft| is_save_container_directory(Path::new(&draft.scope.root_path)));
    let container_bonus = if has_named_save_container { 5 } else { 0 };

    let mode_adjustment: i16 = match (event_capture_mode, transaction_summary) {
        ("etw", Some(txn)) if txn.status == "completed" => {
            let txn_ratio = (txn.confidence as f32 / 100.0).clamp(0.0, 1.0);
            10 + (txn_ratio * 5.0).round() as i16
        }
        ("etw", Some(txn)) if txn.status == "candidate" => 5,
        ("etw", _) => 2,
        _ => 0,
    };

    let total = (max_draft_confidence as i16) + (container_bonus as i16) + mode_adjustment;
    total.clamp(30, 95) as u8
}

fn collect_targeted_snapshot(
    roots: &[crate::domain::ScanRoot],
    etw_files: &HashSet<String>,
    is_cancelled: &impl Fn() -> bool,
) -> Result<HashMap<String, FileFingerprint>, String> {
    let mut files = HashMap::new();
    let norm_roots: Vec<String> = roots
        .iter()
        .map(|root| normalize_path(&root.physical_path))
        .collect();

    for path in etw_files {
        if is_cancelled() {
            return Err("任务已取消".to_string());
        }
        let is_in_roots = norm_roots.iter().any(|root_path| {
            path == root_path
                || (path.starts_with(root_path)
                    && path.as_bytes().get(root_path.len()) == Some(&b'\\'))
        });
        if !is_in_roots {
            continue;
        }
        let candidate = Path::new(path);
        if !candidate.is_file() || is_noise_path(candidate) {
            continue;
        }
        let Ok(metadata) = fs::metadata(candidate) else {
            continue;
        };
        files.insert(
            path.clone(),
            FileFingerprint {
                size: metadata.len(),
                modified_unix: modified_unix(&metadata),
            },
        );
    }
    Ok(files)
}

fn is_etw_candidate(path: &str) -> bool {
    let path_obj = Path::new(path);
    if is_noise_path(path_obj) {
        return false;
    }
    let path_lower = path.to_ascii_lowercase();
    if path_lower.contains("\\analytics\\") || path_lower.contains("/analytics/") {
        return false;
    }
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let extension = extension.to_ascii_lowercase();
    if extension == "log" || RESOURCE_EXTENSIONS.contains(&extension.as_str()) {
        return false;
    }

    let file_stem = path_obj
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let has_file_hint = NAME_HINTS.iter().any(|hint| file_stem.contains(hint));
    let has_save_container = path_obj
        .parent()
        .is_some_and(path_has_save_container_ancestor);
    let has_save_path_hint = [
        "savedata",
        "savegame",
        "savegames",
        "userdata",
        "profiles",
        "remote",
    ]
    .iter()
    .any(|hint| path_has_segment(&path_lower, hint));

    if is_generic_config_file(
        path_obj,
        &path_lower,
        has_save_container,
        has_save_path_hint,
    ) {
        return false;
    }

    if has_save_container || has_save_path_hint || has_file_hint {
        return true;
    }

    // Generic configuration formats are too common in caches and launchers.
    SAVE_EXTENSIONS.contains(&extension.as_str())
}

/// 事务评分与范围证据只应看见「候选口径」的文件操作。
///
/// 两条采集路径的过滤强度本来就不一样：原生 ETL 在「是否为变更操作」判定**之前**就把
/// 操作推进列表（`:152-160` vs `:161-168`），因此多含 `Delete` 与非 write-like 的
/// `Close`；CSV 回退更松，连 `should_ignore_event_path` 都没过。于是 `\logs\`、
/// `\cache\` 里的一次「写入 + 关闭」就能自成一组拿到 80 分 → `status = completed`
/// → `calculate_learning_confidence` 白送 +10~15。用户看到的「事务 3 个」里有两个
/// 是日志和截图。
///
/// 这里统一收口一次，口径与 `infer_scope_drafts` 判定候选时用的 `is_etw_candidate`
/// 对齐。唯一例外是原子保存的中间产物：`.tmp`/`.temp`/`.bak` 本身不是存档文件
/// （它们在草稿里落到 `noise_exact` → `exclude_exact`），但「写临时文件再重命名」
/// 正是最常见的保存动作；连同它们一起丢掉，会让真实的原子保存在事务摘要里退化成
/// 「证据不足」，反而丢掉它应得的置信度。噪音目录优先于这条例外——`\logs\x.tmp`
/// 仍然被 `should_ignore_event_path` 挡在前面。
fn transaction_evidence(operations: &[FileOperation]) -> Vec<FileOperation> {
    operations
        .iter()
        .filter(|operation| is_transaction_evidence_path(&operation.path))
        .cloned()
        .collect()
}

fn is_transaction_evidence_path(path: &str) -> bool {
    if path.is_empty() || should_ignore_event_path(Path::new(path)) {
        return false;
    }
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default();
    if matches!(extension.as_str(), "tmp" | "temp" | "bak") {
        return true;
    }
    is_etw_candidate(path)
}

fn path_is_within_root(path: &str, root: &Path) -> bool {
    let normalized_path = normalize_path(Path::new(path));
    let normalized_root = normalize_path(root);
    normalized_path == normalized_root
        || (normalized_path.starts_with(&normalized_root)
            && normalized_path.as_bytes().get(normalized_root.len()) == Some(&b'\\'))
}

fn modified_unix(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_secs())
        .unwrap_or_default()
}

fn now_iso() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::{
        arm_capture_watchdog, calculate_learning_confidence, classify_scope_evidence,
        directory_matches_hint, discover_save_container_files, infer_scope_drafts,
        is_etw_candidate, is_transaction_evidence_path, path_is_within_root,
        preview_drafts_from_roots, propose_directory_saves, run_capture_watchdog,
        snapshot_analysis_progress, transaction_evidence, MAX_CANDIDATE_FILE_BYTES,
        MAX_CAPTURE_DURATION,
    };
    use crate::domain::path_utils::normalize_path;
    use crate::domain::save_candidate::{
        is_noise_path, is_save_candidate, is_save_container_directory,
    };
    use crate::domain::{
        ActiveLearningSession, EtwCaptureHandle, FileFingerprint, LearningSessionView,
        LearningStatus, SaveCandidateEvidenceLevel, SaveRootType, SaveScope, SaveScopeDraft,
        SaveTransactionSummary, ScanRoot, UnknownFilePolicy, DEFAULT_MAX_FILE_BYTES,
    };
    use crate::services::learning::{analyze_save_transactions, FileOperation, FileOperationKind};
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    use std::sync::{atomic::AtomicBool, Arc, Mutex};
    use std::time::Duration;

    fn operation(path: &str, operation: FileOperationKind) -> FileOperation {
        FileOperation {
            path: path.to_string(),
            operation,
            timestamp_ms: Some(1_000),
            pid: 42,
            file_object_id: None,
        }
    }

    #[test]
    fn snapshot_progress_does_not_overflow_at_completion() {
        assert_eq!(snapshot_analysis_progress(0), 10);
        assert_eq!(snapshot_analysis_progress(64), 61);
        assert_eq!(snapshot_analysis_progress(100), 90);
    }

    /// 采集看门狗是 `.etl` 唯一的**可控**时长上界（`logman -rf` 与 `-ets` 互斥），
    /// 所以「到点真的会去停那个会话」这件事必须有断言钉住，不能只靠读代码。
    #[test]
    fn capture_watchdog_stops_the_session_when_the_deadline_passes() {
        let mut slept = None;
        let mut stopped = None;
        run_capture_watchdog(
            "GameSaverTrace_deadline",
            Duration::from_secs(60),
            |duration| slept = Some(duration),
            |name| {
                stopped = Some(name.to_string());
                Ok(())
            },
        );
        assert_eq!(slept, Some(Duration::from_secs(60)), "必须先等满上限");
        assert_eq!(
            stopped.as_deref(),
            Some("GameSaverTrace_deadline"),
            "必须停的是这次采集的会话名"
        );
    }

    /// 会话已被正常结束或被取消时，看门狗到点再停一次必然报错 —— 只记日志，不得 panic。
    #[test]
    fn capture_watchdog_tolerates_a_session_that_is_already_gone() {
        let mut attempts = 0;
        run_capture_watchdog(
            "GameSaverTrace_gone",
            Duration::from_secs(1),
            |_| {},
            |_| {
                attempts += 1;
                Err("找不到数据收集器集".to_string())
            },
        );
        assert_eq!(attempts, 1, "即便会报错也要尝试停一次");
    }

    /// 挂表动作本身只是 `thread::spawn`，删掉它不会有别的测试变红 —— 所以必须单独钉住
    /// 「会话启动时确实按上限挂了看门狗」，否则这条防护会悄无声息地消失。
    #[test]
    fn capture_watchdog_is_armed_for_an_active_session() {
        let handle = EtwCaptureHandle {
            trace_name: "GameSaverTrace_armed".to_string(),
            etl_path: PathBuf::from(r"C:\logs\armed.etl"),
        };
        let mut armed = None;
        arm_capture_watchdog(Some(&handle), |name, duration| {
            armed = Some((name, duration))
        });
        assert_eq!(
            armed,
            Some(("GameSaverTrace_armed".to_string(), MAX_CAPTURE_DURATION)),
            "必须用这次采集的会话名和统一上限挂表"
        );
    }

    #[test]
    fn capture_watchdog_is_not_armed_without_a_session() {
        let mut armed = false;
        arm_capture_watchdog(None, |_, _| armed = true);
        assert!(!armed, "ETW 不可用时没有会话可停，不该挂表");
    }

    #[test]
    fn etw_candidates_accept_non_resource_files() {
        assert!(is_etw_candidate(r"C:\GameSaver\save\slot.xml"));
        assert!(is_etw_candidate(r"C:\GameSaver\Data\profile.bin"));
    }

    #[test]
    fn etw_candidates_reject_resources_and_noise() {
        assert!(!is_etw_candidate(r"C:\GameSaver\Game.exe"));
        assert!(!is_etw_candidate(r"C:\GameSaver\cache\profile.bin"));
        assert!(!is_etw_candidate(r"C:\GameSaver\logs\session.dat"));
        assert!(!is_etw_candidate(
            r"C:\Users\Player\AppData\Roaming\com.gamesaver.next\events\trace.etl"
        ));
        assert!(!is_etw_candidate(r"C:\GameSaver\GPUCache\data_0"));
        assert!(!is_etw_candidate(r"C:\GameSaver\D3DSCache\cache.bin"));
        assert!(!is_etw_candidate(r"C:\GameSaver\blob_storage\entry"));
    }

    #[test]
    fn transaction_evidence_keeps_only_candidate_paths() {
        let operations = vec![
            operation(r"C:\GameSaver\Saves\slot.sav", FileOperationKind::Write),
            operation(r"C:\GameSaver\logs\session.log", FileOperationKind::Write),
            operation(r"C:\GameSaver\cache\blob.bin", FileOperationKind::Write),
            operation(r"C:\GameSaver\screenshot.png", FileOperationKind::Write),
            operation(r"C:\GameSaver\Game.exe", FileOperationKind::Write),
            operation(
                r"C:\Users\Player\AppData\Local\Temp\scratch.dat",
                FileOperationKind::Write,
            ),
        ];
        let kept = transaction_evidence(&operations);
        assert_eq!(
            kept.iter()
                .map(|item| item.path.as_str())
                .collect::<Vec<_>>(),
            vec![r"C:\GameSaver\Saves\slot.sav"]
        );
    }

    #[test]
    fn transaction_evidence_keeps_atomic_save_intermediates() {
        // `.tmp`/`.temp`/`.bak` 不是候选存档文件，但「写临时文件再重命名」是最常见的
        // 保存动作，丢掉它们会让真实的原子保存退化成「证据不足」。
        let operations = vec![
            operation(r"C:\GameSaver\Saves\slot.tmp", FileOperationKind::Write),
            operation(r"C:\GameSaver\Saves\slot.temp", FileOperationKind::Write),
            operation(r"C:\GameSaver\Saves\slot.bak", FileOperationKind::Delete),
            // 噪音目录优先于这条例外。
            operation(r"C:\GameSaver\logs\scratch.tmp", FileOperationKind::Write),
        ];
        let kept = transaction_evidence(&operations);
        assert_eq!(
            kept.iter()
                .map(|item| item.path.as_str())
                .collect::<Vec<_>>(),
            vec![
                r"C:\GameSaver\Saves\slot.tmp",
                r"C:\GameSaver\Saves\slot.temp",
                r"C:\GameSaver\Saves\slot.bak",
            ]
        );
    }

    #[test]
    fn noise_writes_no_longer_reach_the_transaction_scorer() {
        let operations = vec![
            operation(r"C:\GameSaver\logs\session.log", FileOperationKind::Write),
            operation(r"C:\GameSaver\logs\session.log", FileOperationKind::Close),
        ];
        // 收口之前，同一目录下的一次「写入 + 关闭」自成一组拿满 80 分 → completed，
        // 于是 calculate_learning_confidence 白送 +10~15。这条断言记录的正是那个缺口。
        assert_eq!(
            analyze_save_transactions(operations.clone()).status,
            "completed"
        );
        let filtered = analyze_save_transactions(transaction_evidence(&operations));
        assert_eq!(filtered.status, "insufficient_evidence");
        assert_eq!(filtered.transaction_count, 0);
        assert!(filtered.affected_files.is_empty());
    }

    #[test]
    fn transaction_evidence_path_matches_candidate_or_temp() {
        assert!(is_transaction_evidence_path(r"C:\GameSaver\Saves\slot.sav"));
        assert!(is_transaction_evidence_path(r"C:\GameSaver\Saves\slot.TMP"));
        assert!(is_transaction_evidence_path(r"C:\GameSaver\Saves\slot.bak"));
        assert!(!is_transaction_evidence_path(r"C:\GameSaver\Game.exe"));
        assert!(!is_transaction_evidence_path(r"C:\GameSaver\logs\slot.sav"));
        assert!(!is_transaction_evidence_path(""));
    }

    #[test]
    fn write_and_close_in_scope_is_strong_evidence() {
        let operations = vec![
            FileOperation {
                path: r"C:\Users\Player\SaveData\slot.sav".to_string(),
                operation: FileOperationKind::Write,
                timestamp_ms: Some(1_000),
                pid: 42,
                file_object_id: None,
            },
            FileOperation {
                path: r"C:\Users\Player\SaveData\slot.sav".to_string(),
                operation: FileOperationKind::Close,
                timestamp_ms: Some(1_200),
                pid: 42,
                file_object_id: None,
            },
        ];
        let (level, _, confidence) = classify_scope_evidence(
            Path::new(r"C:\Users\Player\SaveData"),
            1,
            false,
            &operations,
        );
        assert_eq!(level, SaveCandidateEvidenceLevel::Strong);
        assert!(confidence >= 88);
    }

    #[test]
    fn snapshot_only_scope_stays_review_candidate() {
        let (level, _, confidence) =
            classify_scope_evidence(Path::new(r"C:\Users\Player\SaveData"), 1, true, &[]);
        assert_eq!(level, SaveCandidateEvidenceLevel::Review);
        assert_eq!(confidence, 60);
    }

    #[test]
    fn etw_candidates_reject_system_cache_and_telemetry_writes() {
        assert!(!is_etw_candidate(
            r"C:\Users\Player\AppData\Local\NVIDIA\dxcache\ee70a938b19726ed.nvph"
        ));
        assert!(!is_etw_candidate(
            r"C:\Users\Player\AppData\Local\NVIDIA\glcache\422b51b2ed600db2.bin"
        ));
        assert!(!is_etw_candidate(
            r"C:\Users\Player\AppData\Local\Shinjinmao_02\saved\config\crashreportclient\ue4cc.ini"
        ));
        assert!(!is_etw_candidate(
            r"C:\Users\Player\AppData\LocalLow\Tencent\WetType\mm_tip_20260905.xlog"
        ));
    }

    #[test]
    fn etw_candidates_require_save_semantics_for_generic_files() {
        assert!(is_etw_candidate(
            r"C:\Users\Player\Saved Games\Game\save.dat"
        ));
        assert!(is_etw_candidate(
            r"C:\Users\Player\AppData\Local\Game\profile.bin"
        ));
        assert!(is_etw_candidate(
            r"C:\Users\Player\AppData\Local\Game\SaveData\blob"
        ));
        assert!(!is_etw_candidate(
            r"C:\Users\Player\AppData\Local\Game\engine.ini"
        ));
        assert!(!is_etw_candidate(
            r"C:\Users\Player\AppData\Local\Game\data.dat"
        ));
        assert!(!is_etw_candidate(
            r"C:\Users\Player\AppData\Local\Game\data.json"
        ));
        assert!(!is_etw_candidate(
            r"C:\Users\Player\AppData\Local\Game\random.bin"
        ));
        assert!(!is_etw_candidate(
            r"C:\Users\Player\AppData\Local\Game\telemetry.xlog"
        ));
    }

    #[test]
    fn generic_unreal_config_files_are_not_save_candidates() {
        let device_profiles =
            r"C:\Users\Player\AppData\Local\Game\Saved\Config\WindowsNoEditor\deviceprofiles.ini";
        assert!(!is_etw_candidate(device_profiles));
        assert!(!is_save_candidate(device_profiles));
        assert!(is_etw_candidate(
            r"C:\Users\Player\AppData\Local\Game\Saved\Savegames\save.sav"
        ));
        assert!(is_etw_candidate(
            r"C:\Users\Player\AppData\Local\Game\Saved\Config\WindowsNoEditor\savegame.ini"
        ));
    }

    #[test]
    fn recognizes_common_save_container_names() {
        assert!(is_save_container_directory(Path::new(
            r"C:\\Users\\Player\\SaveData"
        )));
        assert!(is_save_container_directory(Path::new(
            r"C:\\Users\\Player\\Save Games"
        )));
        assert!(!is_save_container_directory(Path::new(
            r"C:\\Users\\Player\\Analytics"
        )));
    }

    #[test]
    fn scan_root_matching_does_not_match_arbitrary_parent_path_text() {
        assert!(directory_matches_hint(
            Path::new(r"C:\\Users\\Player\\AppData\\LocalLow\\ApplePie\\MonsterBlackMarket"),
            "blackmarket"
        ));
        assert!(!directory_matches_hint(
            Path::new(r"C:\\Users\\Player\\AppData\\LocalLow\\BlackMarket\\UnrelatedPublisher"),
            "blackmarket"
        ));
    }

    #[test]
    fn fallback_collects_files_from_a_game_specific_save_container() {
        let root = std::env::current_dir()
            .expect("resolve test working directory")
            .join(format!("gamesaver-save-container-{}", uuid::Uuid::new_v4()));
        let save_data = root.join("SaveData");
        fs::create_dir_all(&save_data).expect("create SaveData directory");
        fs::write(save_data.join("PlayerData0.sav"), b"save").expect("write save file");
        let large_file =
            fs::File::create(save_data.join("large-resource.bin")).expect("create oversized file");
        large_file
            .set_len(MAX_CANDIDATE_FILE_BYTES + 1)
            .expect("set oversized file len");
        drop(large_file);
        fs::write(root.join("Player.log"), b"log").expect("write loose log");

        let files = discover_save_container_files(
            &[ScanRoot {
                root_type: SaveRootType::LocalAppData,
                physical_path: root.clone(),
            }],
            &|| false,
        )
        .expect("discover save container files");

        assert_eq!(files.len(), 1);
        assert!(files
            .iter()
            .any(|path| path.ends_with(r"\savedata\playerdata0.sav")));
        let _ = fs::remove_dir_all(root);
    }

    /// R2：只认「学习那一刻的文件清单」的范围，必须把目录里**已经存在**的疑似存档列成
    /// 提议交给用户确认 —— 否则用户新建的档位、游戏写出的带时间戳自动存档永远不会被
    /// 发现，而且没有任何提示。
    ///
    /// 提议只收「像存档」的文件：`settings.ini` 与 `screenshot.png` 不在默认排除模式里
    /// （默认只挡 `*.log`/`*.tmp` 一类），但它们不是存档，不该被摆到用户面前。
    #[test]
    fn proposal_lists_only_unconfirmed_save_like_files() {
        let root = std::env::current_dir()
            .expect("resolve test working directory")
            .join(format!("gamesaver-proposal-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).expect("create root");
        fs::write(root.join("slot1.sav"), b"confirmed").expect("write confirmed save");
        fs::write(root.join("autosave_2026.sav"), b"new slot").expect("write new slot");
        fs::write(root.join("settings.ini"), b"[cfg]").expect("write config");
        fs::write(root.join("screenshot.png"), b"png").expect("write screenshot");

        let proposals = propose_directory_saves(&root, &["slot1.sav".to_string()]);

        assert_eq!(proposals, vec!["autosave_2026.sav".to_string()]);
        let _ = fs::remove_dir_all(&root);
    }

    fn preview_fixture(
        label: &str,
        files: &[(&str, &[u8])],
    ) -> (
        std::path::PathBuf,
        Vec<ScanRoot>,
        HashMap<String, FileFingerprint>,
    ) {
        let root = std::env::current_dir()
            .expect("resolve test working directory")
            .join(format!("gamesaver-{label}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).expect("create root");
        let mut snapshot = HashMap::new();
        for (relative, bytes) in files {
            let path = root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent");
            }
            fs::write(&path, bytes).expect("write fixture file");
            snapshot.insert(
                normalize_path(&path),
                FileFingerprint {
                    size: bytes.len() as u64,
                    modified_unix: 0,
                },
            );
        }
        let roots = vec![ScanRoot {
            root_type: SaveRootType::AppData,
            physical_path: root.clone(),
        }];
        (root, roots, snapshot)
    }

    /// A1「跳过学习」的只读初稿：不启动游戏、不采集 ETW，只按目录名与文件特征推断。
    ///
    /// 两条断言分别钉住「能推出范围」和「证据口径诚实」—— 后者更重要：一份没有任何写入证据
    /// 的草稿，绝不能看起来像被证据确认过，否则用户会把猜出来的范围当成已确认的。
    #[test]
    fn preview_drafts_are_review_only_and_skip_non_save_files() {
        let (root, roots, snapshot) = preview_fixture(
            "preview",
            &[
                ("SAVEDATA/slot1.sav", b"save"),
                ("settings.ini", b"[cfg]"),
                ("screenshot.png", b"png"),
            ],
        );

        let (drafts, _notes) = preview_drafts_from_roots(&roots, &snapshot);

        assert_eq!(drafts.len(), 1, "只该推出 SAVEDATA 这一个候选范围");
        let draft = &drafts[0];
        assert!(
            normalize_path(Path::new(&draft.scope.root_path)).ends_with(r"\savedata"),
            "范围根应落在命名存档容器上，实际 {}",
            draft.scope.root_path
        );
        assert_eq!(draft.scope.confirmed_files, vec!["slot1.sav".to_string()]);
        // 没有任何写入证据 ⇒ 证据等级必须是「待确认」，且说明里要写清这是只读推断。
        assert_eq!(draft.evidence_level, SaveCandidateEvidenceLevel::Review);
        assert!(
            draft.evidence_reason.contains("只读初稿"),
            "说明必须写清这是只读推断，实际：{}",
            draft.evidence_reason
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 目录里没有像存档的文件时初稿必须是空的 —— 不能凭目录名硬凑一个范围出来。
    #[test]
    fn preview_drafts_stay_empty_without_save_like_files() {
        let (root, roots, snapshot) = preview_fixture(
            "preview-empty",
            &[("settings.ini", b"[cfg]"), ("readme.txt", b"hi")],
        );

        let (drafts, _notes) = preview_drafts_from_roots(&roots, &snapshot);

        assert!(
            drafts.is_empty(),
            "没有候选文件就不该有范围，实际推出 {} 个",
            drafts.len()
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 提议只该出现在「只认历史清单」的范围上：容器命名的范围本来就整目录收集
    /// （`include_directories = ["."]`），再列一遍提议是噪音。
    #[test]
    fn only_non_container_drafts_carry_proposals() {
        let root = std::env::current_dir()
            .expect("resolve test working directory")
            .join(format!("gamesaver-proposal-scope-{}", uuid::Uuid::new_v4()));
        let plain_dir = root.join("Game");
        let container_dir = root.join("SaveData");
        fs::create_dir_all(&plain_dir).expect("create plain directory");
        fs::create_dir_all(&container_dir).expect("create container directory");
        let changed_plain = plain_dir.join("slot1.sav");
        let changed_container = container_dir.join("slot1.sav");
        fs::write(&changed_plain, b"changed").expect("write changed save");
        fs::write(plain_dir.join("slot2.sav"), b"existing").expect("write existing save");
        fs::write(&changed_container, b"changed").expect("write changed container save");
        fs::write(container_dir.join("slot2.sav"), b"existing").expect("write existing");

        let active = ActiveLearningSession {
            view: LearningSessionView {
                session_id: "test-sess".to_string(),
                game_uid: "game-1".to_string(),
                root_pid: 100,
                started_at: "0".to_string(),
                status: LearningStatus::Capturing,
            },
            roots: vec![ScanRoot {
                root_type: SaveRootType::AppData,
                physical_path: root.clone(),
            }],
            baseline: None,
            tracked_pids: Arc::new(Mutex::new(vec![100])),
            process_tracker_stop: Arc::new(AtomicBool::new(false)),
            process_tracker_done: Arc::new(AtomicBool::new(false)),
            etw_capture: None,
            etw_start_error: None,
            validation_only: false,
        };

        // 只有 slot1 在本次变化里，slot2 是目录里早已存在、没被这次学习看到的文件。
        let mut final_snapshot = HashMap::new();
        final_snapshot.insert(
            normalize_path(&changed_plain),
            FileFingerprint {
                size: 16,
                modified_unix: 1,
            },
        );
        final_snapshot.insert(
            normalize_path(&changed_container),
            FileFingerprint {
                size: 16,
                modified_unix: 1,
            },
        );

        let etw_empty = HashSet::new();
        let (_, drafts, _) = infer_scope_drafts(
            &active.roots,
            &final_snapshot,
            Some(&HashMap::new()),
            &etw_empty,
            &[],
        );

        let find_draft = |leaf: &str| {
            drafts
                .iter()
                .find(|draft| draft.scope.root_path.to_ascii_lowercase().ends_with(leaf))
                .unwrap_or_else(|| panic!("找不到以 {leaf} 结尾的范围草稿"))
        };
        assert_eq!(
            find_draft("\\game").proposed_files,
            vec!["slot2.sav".to_string()],
            "只认历史清单的范围必须把目录里现存的疑似存档列出来"
        );
        assert!(
            find_draft("\\savedata").proposed_files.is_empty(),
            "容器范围本来就是整目录收集，不该再列提议"
        );

        // R2b：非容器范围也必须开目录级收集，并把策略默认成「自动保护新存档」。
        // 两件事缺一不可 —— 只加 `include_directories` 而策略仍是 Ignore，收集侧那道门
        // 第二层就把候选文件全拒了，R2b 会静默失效。
        let plain = find_draft("\\game");
        assert_eq!(plain.scope.include_directories, vec![".".to_string()]);
        assert_eq!(plain.scope.unknown_file_policy, UnknownFilePolicy::Protect);
        // 容器范围的行为保持不变：整目录收。
        let container = find_draft("\\savedata");
        assert_eq!(container.scope.include_directories, vec![".".to_string()]);
        assert_eq!(
            container.scope.unknown_file_policy,
            UnknownFilePolicy::Protect
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn path_matching_is_case_insensitive_for_windows_roots() {
        assert!(path_is_within_root(
            r"c:\users\player\gamesaver\save.dat",
            Path::new(r"C:\Users\Player\GameSaver")
        ));
        assert!(!path_is_within_root(
            r"c:\users\player\gamesaver-old\save.dat",
            Path::new(r"C:\Users\Player\GameSaver")
        ));
    }

    #[test]
    fn saved_games_root_type_serializes_and_matches() {
        assert_eq!(
            serde_json::to_string(&SaveRootType::SavedGames).unwrap(),
            "\"saved_games\""
        );
        let deserialized: SaveRootType = serde_json::from_str("\"saved_games\"").unwrap();
        assert_eq!(deserialized, SaveRootType::SavedGames);
    }

    #[test]
    fn local_low_root_type_serializes_and_matches() {
        assert_eq!(
            serde_json::to_string(&SaveRootType::LocalLow).unwrap(),
            "\"local_low\""
        );
        let deserialized: SaveRootType = serde_json::from_str("\"local_low\"").unwrap();
        assert_eq!(deserialized, SaveRootType::LocalLow);
    }

    #[test]
    fn learning_confidence_returns_zero_when_no_drafts() {
        assert_eq!(calculate_learning_confidence(&[], "etw", None), 0);
        assert_eq!(calculate_learning_confidence(&[], "snapshot", None), 0);
    }

    #[test]
    fn learning_confidence_scales_with_evidence_and_transactions() {
        let dummy_scope = SaveScope {
            root_type: SaveRootType::AppData,
            root_path: r"C:\Users\Player\AppData\LocalLow\GameStudio".to_string(),
            confirmed_files: vec!["profile.sav".to_string()],
            include_directories: vec![],
            exclude_exact: vec![],
            exclude_patterns: vec![],
            exclude_directories: vec![],
            unknown_file_policy: UnknownFilePolicy::Protect,
            max_file_bytes: Some(10_000_000),
        };
        let single_file_draft = SaveScopeDraft {
            scope: dummy_scope.clone(),
            changed_files: vec!["profile.sav".to_string()],
            proposed_files: vec![],
            confidence: 65,
            evidence_level: SaveCandidateEvidenceLevel::Review,
            evidence_reason: "test".to_string(),
        };
        // 纯快照单文件 -> 65%
        assert_eq!(
            calculate_learning_confidence(&[single_file_draft.clone()], "snapshot", None),
            65
        );

        let container_scope = SaveScope {
            root_type: SaveRootType::SavedGames,
            root_path: r"C:\Users\Player\Saved Games\MyGame\SaveData".to_string(),
            confirmed_files: vec!["slot1.sav".to_string(), "slot2.sav".to_string()],
            include_directories: vec![".".to_string()],
            exclude_exact: vec![],
            exclude_patterns: vec![],
            exclude_directories: vec![],
            unknown_file_policy: UnknownFilePolicy::Protect,
            max_file_bytes: Some(10_000_000),
        };
        let container_draft = SaveScopeDraft {
            scope: container_scope,
            changed_files: vec!["slot1.sav".to_string(), "slot2.sav".to_string()],
            proposed_files: vec![],
            confidence: 80,
            evidence_level: SaveCandidateEvidenceLevel::Strong,
            evidence_reason: "test".to_string(),
        };
        // 快照模式多文件 + SaveData 容器命名奖励 -> 85%
        assert_eq!(
            calculate_learning_confidence(&[container_draft.clone()], "snapshot", None),
            85
        );

        let completed_txn = SaveTransactionSummary {
            status: "completed".to_string(),
            confidence: 90,
            transaction_count: 1,
            affected_files: vec!["slot1.sav".to_string()],
            affected_directories: vec!["SaveData".to_string()],
            started_at: Some("1000".to_string()),
            ended_at: Some("1500".to_string()),
            operation_count: 3,
            notes: vec![],
        };
        // ETW 完整事务 + 命名容器 -> 95% (顶格封顶)
        assert_eq!(
            calculate_learning_confidence(&[container_draft], "etw", Some(&completed_txn)),
            95
        );
    }

    #[test]
    fn bidirectional_diff_captures_deleted_and_created_files() {
        let active = ActiveLearningSession {
            view: LearningSessionView {
                session_id: "test-sess".to_string(),
                game_uid: "game-1".to_string(),
                root_pid: 100,
                started_at: "0".to_string(),
                status: LearningStatus::Capturing,
            },
            roots: vec![ScanRoot {
                root_type: SaveRootType::SavedGames,
                physical_path: PathBuf::from(r"c:\users\player\saved games\mygame"),
            }],
            baseline: None,
            tracked_pids: Arc::new(Mutex::new(vec![100])),
            process_tracker_stop: Arc::new(AtomicBool::new(false)),
            process_tracker_done: Arc::new(AtomicBool::new(false)),
            etw_capture: None,
            etw_start_error: None,
            validation_only: false,
        };

        let old_save = r"c:\users\player\saved games\mygame\save_old.sav".to_string();
        let new_save = r"c:\users\player\saved games\mygame\save_new.sav".to_string();

        let mut baseline = HashMap::new();
        baseline.insert(
            old_save.clone(),
            FileFingerprint {
                size: 1024,
                modified_unix: 100,
            },
        );

        let mut final_snapshot = HashMap::new();
        final_snapshot.insert(
            new_save.clone(),
            FileFingerprint {
                size: 2048,
                modified_unix: 200,
            },
        );

        let etw_empty = HashSet::new();
        let (changed, drafts, _) = infer_scope_drafts(
            &active.roots,
            &final_snapshot,
            Some(&baseline),
            &etw_empty,
            &[],
        );

        // 双向 diff 必须同时捕获被删除的 old_save 和新建的 new_save
        assert_eq!(changed.len(), 2);
        assert!(changed.contains(&old_save));
        assert!(changed.contains(&new_save));

        // 生成的 scope draft confirmed_files 只包含当前实际存活的 new_save
        assert_eq!(drafts.len(), 1);
        assert_eq!(
            drafts[0].scope.confirmed_files,
            vec!["save_new.sav".to_string()]
        );
        // 自动注入了标准排除规则
        assert!(drafts[0]
            .scope
            .exclude_patterns
            .contains(&"*.log".to_string()));
        assert!(drafts[0]
            .scope
            .exclude_patterns
            .contains(&"*.tmp".to_string()));
        assert!(drafts[0]
            .scope
            .exclude_directories
            .contains(&"logs".to_string()));
    }

    /// 学习产出的 scope 必须用「管理上限」而不是「候选判定上限」。
    ///
    /// 超过管理上限的文件留在 confirmed_files 里就是「看着受保护、实际既不进版本库
    /// 也不受保护」的静默缺口，所以必须被剔除，并且要有一条明确的告知。
    #[test]
    fn learning_draft_drops_oversize_files_and_reports_them() {
        let active = ActiveLearningSession {
            view: LearningSessionView {
                session_id: "test-sess-oversize".to_string(),
                game_uid: "game-oversize".to_string(),
                root_pid: 100,
                started_at: "0".to_string(),
                status: LearningStatus::Capturing,
            },
            roots: vec![ScanRoot {
                root_type: SaveRootType::SavedGames,
                physical_path: PathBuf::from(r"c:\users\player\saved games\mygame"),
            }],
            baseline: None,
            tracked_pids: Arc::new(Mutex::new(vec![100])),
            process_tracker_stop: Arc::new(AtomicBool::new(false)),
            process_tracker_done: Arc::new(AtomicBool::new(false)),
            etw_capture: None,
            etw_start_error: None,
            validation_only: false,
        };

        let normal = r"c:\users\player\saved games\mygame\slot1.sav".to_string();
        let oversize = r"c:\users\player\saved games\mygame\slot2.sav".to_string();

        let mut final_snapshot = HashMap::new();
        final_snapshot.insert(
            normal,
            FileFingerprint {
                size: 4096,
                modified_unix: 200,
            },
        );
        final_snapshot.insert(
            oversize,
            FileFingerprint {
                size: DEFAULT_MAX_FILE_BYTES + 1,
                modified_unix: 200,
            },
        );

        let etw_empty = HashSet::new();
        let (_, drafts, notes) = infer_scope_drafts(
            &active.roots,
            &final_snapshot,
            Some(&HashMap::new()),
            &etw_empty,
            &[],
        );

        assert_eq!(drafts.len(), 1, "同目录的两个候选应聚成一个范围");
        assert_eq!(
            drafts[0].scope.max_file_bytes,
            Some(DEFAULT_MAX_FILE_BYTES),
            "学习产出的管理上限必须与领域默认一致，不能再用候选判定上限"
        );
        assert_eq!(
            drafts[0].scope.confirmed_files,
            vec!["slot1.sav".to_string()],
            "超限文件不得留在 confirmed_files 里"
        );
        assert!(
            notes.iter().any(|note| note.contains("已跳过 1 个超过")),
            "超限文件必须明确告知，不能静默消失：{notes:?}"
        );
    }

    /// 噪音文件的排除规则不得扩到真存档头上。
    ///
    /// 旧实现会按噪音的扩展名注入 `*.png` 这类整族排除，并把噪音所在的整个子目录
    /// 加进 `exclude_directories`。`is_excluded` 同时喂收集侧与恢复归组，一旦误伤，
    /// 真存档既不被备份、恢复时还可能找不到范围。现在两个方向都要被挡住。
    #[test]
    fn noise_files_never_widen_exclusions_onto_confirmed_saves() {
        let active = ActiveLearningSession {
            view: LearningSessionView {
                session_id: "test-sess-noise".to_string(),
                game_uid: "game-noise".to_string(),
                root_pid: 100,
                started_at: "0".to_string(),
                status: LearningStatus::Capturing,
            },
            roots: vec![ScanRoot {
                root_type: SaveRootType::SavedGames,
                physical_path: PathBuf::from(r"c:\users\player\saved games\mygame"),
            }],
            baseline: None,
            tracked_pids: Arc::new(Mutex::new(vec![100])),
            process_tracker_stop: Arc::new(AtomicBool::new(false)),
            process_tracker_done: Arc::new(AtomicBool::new(false)),
            etw_capture: None,
            etw_start_error: None,
            validation_only: false,
        };

        let real_save = r"c:\users\player\saved games\mygame\savedata\slot1\save.dat".to_string();
        let noisy_image =
            r"c:\users\player\saved games\mygame\savedata\slot1\avatar.png".to_string();

        let mut final_snapshot = HashMap::new();
        final_snapshot.insert(
            real_save,
            FileFingerprint {
                size: 2048,
                modified_unix: 200,
            },
        );
        final_snapshot.insert(
            noisy_image,
            FileFingerprint {
                size: 512,
                modified_unix: 200,
            },
        );

        let etw_empty = HashSet::new();
        let (_, drafts, _) = infer_scope_drafts(
            &active.roots,
            &final_snapshot,
            Some(&HashMap::new()),
            &etw_empty,
            &[],
        );

        assert_eq!(drafts.len(), 1);
        let scope = &drafts[0].scope;
        assert_eq!(
            scope.confirmed_files,
            vec!["slot1/save.dat".to_string()],
            "真存档必须留在 confirmed_files 里"
        );
        assert!(
            !scope.exclude_patterns.contains(&"*.png".to_string()),
            "噪音的扩展名不得被放大成整族排除：{:?}",
            scope.exclude_patterns
        );
        assert!(
            !scope
                .exclude_directories
                .iter()
                .any(|dir| dir.eq_ignore_ascii_case("slot1")),
            "住着真存档的子目录不得被排除：{:?}",
            scope.exclude_directories
        );
        assert!(
            scope
                .exclude_exact
                .contains(&"slot1/avatar.png".to_string()),
            "噪音自身仍要被精确排除：{:?}",
            scope.exclude_exact
        );
    }

    #[test]
    fn smart_container_lift_clusters_nested_slots_and_injects_preset_exclusions() {
        let active = ActiveLearningSession {
            view: LearningSessionView {
                session_id: "test-sess-lift".to_string(),
                game_uid: "game-lift".to_string(),
                root_pid: 100,
                started_at: "0".to_string(),
                status: LearningStatus::Capturing,
            },
            roots: vec![ScanRoot {
                root_type: SaveRootType::LocalLow,
                physical_path: PathBuf::from(r"C:\Users\Player\AppData\LocalLow\Company\MyGame"),
            }],
            baseline: None,
            tracked_pids: Arc::new(Mutex::new(vec![100])),
            process_tracker_stop: Arc::new(AtomicBool::new(false)),
            process_tracker_done: Arc::new(AtomicBool::new(false)),
            etw_capture: None,
            etw_start_error: None,
            validation_only: false,
        };

        let slot1 =
            r"c:\users\player\appdata\locallow\company\mygame\savedata\slot1\save.dat".to_string();
        let slot2 =
            r"c:\users\player\appdata\locallow\company\mygame\savedata\slot2\save.dat".to_string();
        let log_file =
            r"c:\users\player\appdata\locallow\company\mygame\savedata\player.log".to_string();

        let mut final_snapshot = HashMap::new();
        final_snapshot.insert(
            slot1.clone(),
            FileFingerprint {
                size: 1024,
                modified_unix: 200,
            },
        );
        final_snapshot.insert(
            slot2.clone(),
            FileFingerprint {
                size: 1024,
                modified_unix: 200,
            },
        );
        final_snapshot.insert(
            log_file.clone(),
            FileFingerprint {
                size: 512,
                modified_unix: 200,
            },
        );

        let etw_empty = HashSet::new();
        let (_, drafts, _) = infer_scope_drafts(
            &active.roots,
            &final_snapshot,
            Some(&HashMap::new()),
            &etw_empty,
            &[],
        );

        // 两个槽位必须自动聚类提升至同一个 SaveData 顶层容器
        assert_eq!(drafts.len(), 1);
        let draft = &drafts[0];
        assert_eq!(
            normalize_path(Path::new(&draft.scope.root_path)),
            r"c:\users\player\appdata\locallow\company\mygame\savedata"
        );
        assert_eq!(
            draft.scope.confirmed_files,
            vec!["slot1/save.dat".to_string(), "slot2/save.dat".to_string()]
        );
        assert_eq!(draft.scope.include_directories, vec![".".to_string()]);
        assert_eq!(draft.scope.unknown_file_policy, UnknownFilePolicy::Protect);

        // 同目录发现的 Player.log 伴生噪音必须自动转为精确排除
        assert!(draft
            .scope
            .exclude_exact
            .contains(&"player.log".to_string()));
        // 必须自动带有通用排除模式
        assert!(draft.scope.exclude_patterns.contains(&"*.tmp".to_string()));
        assert!(draft.scope.exclude_patterns.contains(&"*.log".to_string()));
    }

    #[test]
    fn unicode_game_name_hints_extracts_cjk_words() {
        let game = crate::domain::Game {
            game_uid: "test-cjk".to_string(),
            display_name: "黑神话：悟空".to_string(),
            game_key: "black_myth".to_string(),
            managed_path: "D:/Games/b1".to_string(),
            lifecycle: crate::domain::GameLifecycle::Active,
            health: crate::domain::GameHealth::Ready,
            cloud_status: crate::domain::game::CloudStatus::LocalOnly,
            launch: crate::domain::game::LaunchConfig {
                executable_relative_path: "b1/Binaries/Win64/b1-Win64-Shipping.exe".to_string(),
                working_directory_relative_path: None,
                arguments: Vec::new(),
            },
            cover: None,
            save_profile_id: None,
            last_played_at: None,
            latest_save_version_id: None,
            added_at: None,
        };

        let hints = super::game_name_hints(&game);
        assert!(hints.contains(&"黑神话".to_string()));
        assert!(hints.contains(&"悟空".to_string()));
        assert!(hints.contains(&"黑神话悟空".to_string()));
    }

    #[test]
    fn infer_scan_root_for_etw_file_resolves_standard_locations() {
        let profile =
            std::env::var("USERPROFILE").unwrap_or_else(|_| r"C:\Users\Player".to_string());
        let managed = Path::new(r"D:\Games\GameSaverGames\games\game-1");

        // 1. LocalLow under Publisher\Game
        let local_low_file = PathBuf::from(&profile)
            .join("AppData")
            .join("LocalLow")
            .join("miHoYo")
            .join("Genshin")
            .join("save.dat");
        let root = super::infer_scan_root_for_etw_file(&local_low_file, Some(managed));
        assert!(root.is_some());
        let root = root.unwrap();
        assert_eq!(root.root_type, SaveRootType::LocalLow);
        assert!(normalize_path(&root.physical_path).contains("genshin"));

        // 2. Saved Games
        let saved_games_file = PathBuf::from(&profile)
            .join("Saved Games")
            .join("Hades")
            .join("Profile1.sav");
        let root = super::infer_scan_root_for_etw_file(&saved_games_file, Some(managed));
        assert!(root.is_some());
        let root = root.unwrap();
        assert_eq!(root.root_type, SaveRootType::SavedGames);
        assert!(normalize_path(&root.physical_path).contains("hades"));

        // 3. Managed Game
        let managed_file = managed.join("Game_Data").join("slot.bin");
        let root = super::infer_scan_root_for_etw_file(&managed_file, Some(managed));
        assert!(root.is_some());
        let root = root.unwrap();
        assert_eq!(root.root_type, SaveRootType::ManagedGame);

        // 4. Public Documents Steam emulator container
        let public_doc = std::env::var_os("PUBLIC")
            .or_else(|| {
                std::env::var_os("SystemDrive")
                    .map(|d| format!(r"{}\Users\Public", d.to_string_lossy()).into())
            })
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Users\Public"))
            .join("Documents");
        let steam_file = public_doc
            .join("Steam")
            .join("RUNE")
            .join("2456740")
            .join("remote")
            .join("slot0.json");
        let root = super::infer_scan_root_for_etw_file(&steam_file, Some(managed));
        assert!(root.is_some());
        let root = root.unwrap();
        assert_eq!(root.root_type, SaveRootType::Documents);
        assert!(is_save_container_directory(&root.physical_path));
        assert_eq!(
            normalize_path(&root.physical_path),
            normalize_path(&public_doc.join("Steam").join("RUNE").join("2456740"))
        );
    }

    /// R1：`%PROGRAMDATA%\<厂商>\<游戏>\<文件>` 必须回推出「厂商\游戏」两级范围根。
    ///
    /// 旧实现的 `bases` 没有 ProgramData 这一项，函数会一路走到末尾 `None` —— 也就是
    /// 说即使 ETW 已经证明游戏写了这个文件，`infer_scope_drafts` 也会因为找不到范围而
    /// 丢掉这份证据，用户只看到「没有发现变化」。
    #[test]
    fn infer_scan_root_for_etw_file_resolves_program_data() {
        let program_data = std::env::var_os("PROGRAMDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));

        let nested = program_data
            .join("Vendor")
            .join("ClassicGame")
            .join("save")
            .join("slot0.dat");
        let root = super::infer_scan_root_for_etw_file(&nested, None).expect("应回推出范围");
        assert_eq!(root.root_type, SaveRootType::ProgramData);
        assert_eq!(
            normalize_path(&root.physical_path),
            normalize_path(&program_data.join("Vendor").join("ClassicGame")),
            "应取厂商 + 游戏两级，而不是文件所在的 save 子目录"
        );

        // 少一段时只能取一级：把第二段（其实是文件名）也拼进去，范围根就成了文件路径。
        let shallow = program_data.join("ClassicGame").join("save.dat");
        let root = super::infer_scan_root_for_etw_file(&shallow, None).expect("应回推出范围");
        assert_eq!(root.root_type, SaveRootType::ProgramData);
        assert_eq!(
            normalize_path(&root.physical_path),
            normalize_path(&program_data.join("ClassicGame"))
        );
    }

    /// R1 的覆盖守卫：三个环境变量根都必须被解析出来。
    ///
    /// 这不是逻辑测试而是覆盖断言 —— 少一项就意味着那一类位置的存档永久漏识别。把
    /// `ENVIRONMENT_SCAN_ROOTS` 里的 `PROGRAMDATA` 那一行删掉，这条会立刻失败。
    #[test]
    #[cfg(windows)]
    fn environment_scan_roots_include_program_data() {
        let roots = super::environment_scan_roots();
        assert!(roots.iter().any(|(_, kind)| *kind == SaveRootType::AppData));
        assert!(roots
            .iter()
            .any(|(_, kind)| *kind == SaveRootType::LocalAppData));
        let program_data = roots
            .iter()
            .find(|(_, kind)| *kind == SaveRootType::ProgramData)
            .map(|(path, _)| path.clone())
            .expect("PROGRAMDATA 必须被解析为扫描根；缺失会让 ProgramData 下的存档永久漏识别");
        assert_eq!(
            program_data,
            PathBuf::from(std::env::var_os("PROGRAMDATA").expect("Windows 上 PROGRAMDATA 必有值"))
        );
    }

    #[test]
    fn noise_filtering_rejects_unity_and_engine_logs() {
        assert!(is_noise_path(Path::new(
            r"C:\Users\User\AppData\LocalLow\Game\Player.log"
        )));
        assert!(is_noise_path(Path::new(
            r"C:\Users\User\AppData\LocalLow\Game\Player-prev.log"
        )));
        assert!(is_noise_path(Path::new(
            r"C:\Users\User\AppData\LocalLow\Game\test.tmp"
        )));
        assert!(is_noise_path(Path::new(
            r"C:\Users\User\AppData\LocalLow\Game\crash.dmp"
        )));
        assert!(!is_noise_path(Path::new(
            r"C:\Users\User\AppData\LocalLow\Game\save01.dat"
        )));
        assert!(!is_noise_path(Path::new(
            r"C:\Users\Public\Documents\Steam\RUNE\2456740\remote\slot0.json"
        )));
    }

    #[test]
    fn noise_filtering_rejects_system_generated_paths() {
        assert!(is_noise_path(Path::new(
            r"C:\Users\User\AppData\Local\NVIDIA\dxcache\cache.nvph"
        )));
        assert!(is_noise_path(Path::new(
            r"C:\Users\User\AppData\Local\Game\crashreportclient\settings.ini"
        )));
        assert!(is_noise_path(Path::new(
            r"C:\Users\User\AppData\LocalLow\Tencent\WetType\mm_tip.xlog"
        )));
    }

    #[test]
    fn generic_executable_stems_are_not_extracted_as_hints() {
        let game = crate::domain::Game {
            game_uid: "test-generic-stem".to_string(),
            display_name: "My RPG Game".to_string(),
            game_key: "my_rpg".to_string(),
            managed_path: "D:/Games/my_rpg".to_string(),
            lifecycle: crate::domain::GameLifecycle::Active,
            health: crate::domain::GameHealth::Ready,
            cloud_status: crate::domain::game::CloudStatus::LocalOnly,
            launch: crate::domain::game::LaunchConfig {
                executable_relative_path: "Game.exe".to_string(),
                working_directory_relative_path: None,
                arguments: Vec::new(),
            },
            cover: None,
            save_profile_id: None,
            last_played_at: None,
            latest_save_version_id: None,
            added_at: None,
        };

        let hints = super::game_name_hints(&game);
        assert!(!hints.contains(&"game".to_string()));
        assert!(hints.contains(&"rpg".to_string()));
    }

    #[test]
    fn directory_matches_hint_rejects_save_containers_and_noise() {
        assert!(!super::directory_matches_hint(
            Path::new(r"C:\Users\Player\AppData\Local\BrightMemoryInfinite\Saved\SaveGames"),
            "game"
        ));
        assert!(!super::directory_matches_hint(
            Path::new(r"C:\Users\Player\AppData\Local\EpicGamesLauncher"),
            "game"
        ));
        assert!(!super::directory_matches_hint(
            Path::new(r"C:\Users\Player\AppData\Local\SaveData"),
            "save"
        ));
    }

    #[test]
    fn is_save_container_directory_rejects_appdata_user_data() {
        assert!(!is_save_container_directory(Path::new(
            r"C:\Users\Player\AppData\Local\rmmz-game\User Data"
        )));
        assert!(is_save_container_directory(Path::new(
            r"C:\Program Files (x86)\Steam\userdata"
        )));
        assert!(is_save_container_directory(Path::new(
            r"C:\Program Files (x86)\Steam\userdata\123456\2456740"
        )));
    }

    #[test]
    fn noise_filtering_rejects_chromium_user_data_cache() {
        assert!(is_noise_path(Path::new(
            r"C:\Users\Player\AppData\Local\rmmz-game\User Data\Default\Cookies"
        )));
        assert!(is_noise_path(Path::new(
            r"C:\Users\Player\AppData\Local\rmmz-game\User Data\Default\Code Cache\js\123_0"
        )));
        assert!(is_noise_path(Path::new(
            r"C:\Users\Player\AppData\Local\rmmz-game\User Data\crashpadmetrics-active.pma"
        )));
    }

    #[test]
    fn infer_scan_root_for_etw_file_resolves_steam_userdata() {
        let steam_file =
            Path::new(r"C:\Program Files (x86)\Steam\userdata\123456\2456740\remote\slot0.json");
        let root = super::infer_scan_root_for_etw_file(steam_file, None);
        assert!(root.is_some());
        let root = root.unwrap();
        assert_eq!(root.root_type, SaveRootType::Custom);
        assert_eq!(
            super::normalize_path(&root.physical_path),
            r"c:\program files (x86)\steam\userdata\123456\2456740"
        );
    }

    #[test]
    fn classify_scope_evidence_recognizes_atomic_rename_as_strong() {
        use crate::services::learning::{FileOperation, FileOperationKind};
        let scope_root = Path::new(r"C:\Users\Player\AppData\Local\MyGame\Saves");
        let operations = vec![
            FileOperation {
                path: r"C:\Users\Player\AppData\Local\MyGame\Saves\slot.tmp".to_string(),
                operation: FileOperationKind::Write,
                file_object_id: Some("0x1234".to_string()),
                pid: 1234,
                timestamp_ms: Some(1000),
            },
            FileOperation {
                path: r"C:\Users\Player\AppData\Local\MyGame\Saves\slot.sav".to_string(),
                operation: FileOperationKind::Rename,
                file_object_id: Some("0x1234".to_string()),
                pid: 1234,
                timestamp_ms: Some(1050),
            },
        ];
        let (level, reason, confidence) =
            super::classify_scope_evidence(scope_root, 1, false, &operations);
        assert_eq!(level, SaveCandidateEvidenceLevel::Strong);
        assert_eq!(confidence, 92);
        assert!(reason.contains("重命名"));
    }
}
