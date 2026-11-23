use crate::domain::{
    known_folders::{documents_root, public_documents_root},
    path_utils::{normalize_path, strip_verbatim_prefix},
};
use crate::domain::save_candidate::{
    is_automatic_save_file, is_generic_config_file, is_noise_path, is_save_candidate, is_save_container_directory,
    path_has_save_container_ancestor, path_has_segment, should_ignore_event_path, NAME_HINTS,
    RESOURCE_EXTENSIONS, SAVE_EXTENSIONS,
};
use crate::domain::{
    EtwCaptureHandle, FileFingerprint, Game, LearningSessionView,
    LearningStatus, SaveCandidateEvidenceLevel, SaveLearningResult, SaveRootType, SaveScope,
    SaveScopeDraft, UnknownFilePolicy, DEFAULT_EXCLUDE_DIRECTORIES, DEFAULT_EXCLUDE_PATTERNS,
    DEFAULT_MAX_FILE_BYTES,
};
use crate::services::learning::{
    collect_related_files_by_trace, stop_etw_capture, try_start_etw_capture, FileOperationKind,
    FileOperationTrace,
};
#[cfg(test)]
use crate::services::learning::FileOperation;
use crate::repositories::save_repository::matching_exclusion;
use crate::services::process_service::LearningProcessTracker;
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

/// 单个范围最多列出多少个「疑似存档但本次未变化」的文件。
///
/// 这是给用户看的清单，不是收集清单；`%APPDATA%\<游戏名>` 这类目录通常只有几十个文件，
/// 上限定这么低是为了挡住病态目录（比如误把整个安装目录当成范围根）拖慢分析。
const MAX_PROPOSED_FILES: usize = 200;
const MAX_PROPOSAL_SCAN_ENTRIES: usize = 8_000;
/// 只读初稿的采集模式标记。它不是一次真正的采集 —— 单独取值是为了让前端能区分文案，
/// 也让 `calculate_learning_confidence` 不给它任何 ETW 加分（`_ => 0`）。
const PREVIEW_CAPTURE_MODE: &str = "preview";
const PREVIEW_DISCOVERY_DEPTH: usize = 5;
const PREVIEW_DISCOVERY_ENTRIES_PER_LOCATION: usize = 6_000;
const PREVIEW_SNAPSHOT_DEPTH: usize = 8;
const PREVIEW_SNAPSHOT_ENTRIES: usize = 24_000;
const PREVIEW_SNAPSHOT_ENTRIES_PER_ROOT: usize = 8_000;
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
const PROCESS_HANDOFF_GRACE: Duration = Duration::from_secs(10);
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

#[derive(Debug, Clone)]
pub(crate) struct ActiveLearningSession {
    pub view: LearningSessionView,
    pub roots: Vec<crate::domain::ScanRoot>,
    pub baseline: Option<HashMap<String, FileFingerprint>>,
    pub tracked_processes: Arc<Mutex<LearningProcessTracker>>,
    pub process_tracker_stop: Arc<AtomicBool>,
    pub process_tracker_done: Arc<AtomicBool>,
    pub etw_capture: Option<EtwCaptureHandle>,
    pub etw_start_error: Option<String>,
    pub validation_only: bool,
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

    pub(crate) fn start(
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

    pub(crate) fn start_candidate_verification(
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
        Self::ensure_game_exited(Path::new(&game.managed_path), None)?;
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
        let session_id = Uuid::new_v4().to_string();
        let mut etw_start_error = None;
        on_progress(8, "正在启动 ETW 存档事件采集");
        let etw_capture = match try_start_etw_capture(app, &session_id) {
            Ok(handle) => {
                on_progress(56, "ETW 已启动，准备记录游戏文件操作");
                Some(handle)
            }
            Err(error) => {
                etw_start_error = Some(error);
                on_progress(10, "ETW 不可用，正在建立快照回退基线");
                None
            }
        };

        let baseline = if etw_capture.is_some() {
            None
        } else {
            Some(collect_snapshot(
                &roots,
                |progress, message| on_progress(12 + (progress as u32 * 41 / 100) as u8, message),
                &is_cancelled,
            )?)
        };

        if etw_capture.is_some() {
            on_progress(
                57,
                if validation_only {
                    "ETW 优先验证：跳过启动前全量 baseline"
                } else {
                    "ETW 优先模式：跳过启动前全量 baseline"
                },
            );
        } else {
            on_progress(
                56,
                if validation_only {
                    "快照回退基线已建立，准备验证候选范围"
                } else {
                    "快照回退基线已建立，准备启动游戏"
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
        let managed_path = PathBuf::from(&game.managed_path);
        let mut tracker = LearningProcessTracker::from_child(&child);
        if let Err(error) = tracker.refresh(Some(&managed_path)) {
            crate::logging::warn(format!("首次读取存档学习进程树失败：{error}"));
        }
        let tracked_processes = Arc::new(Mutex::new(tracker));
        let process_tracker_stop = Arc::new(AtomicBool::new(false));
        let process_tracker_done = Arc::new(AtomicBool::new(false));
        spawn_process_tracker(
            Arc::clone(&tracked_processes),
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
            tracked_processes,
            process_tracker_stop,
            process_tracker_done,
            etw_capture,
            etw_start_error,
            validation_only,
        })
    }

    pub(crate) fn ensure_game_exited(
        managed_path: &Path,
        tracked_processes: Option<&Mutex<LearningProcessTracker>>,
    ) -> Result<(), String> {
        if let Some(tracked_processes) = tracked_processes {
            let mut tracker = tracked_processes.lock().map_err(|_| "读取游戏进程树失败")?;
            return ensure_tracked_game_exited(&mut tracker, managed_path);
        }
        ensure_tracked_game_exited(&mut LearningProcessTracker::default(), managed_path)
    }

    pub(crate) fn finish(
        active: &ActiveLearningSession,
        game: &Game,
        on_progress: impl Fn(u8, &str),
        is_cancelled: impl Fn() -> bool,
    ) -> Result<SaveLearningResult, String> {
        on_progress(10, "正在读取保存后的文件状态");
        active.process_tracker_stop.store(true, Ordering::Release);
        wait_for_process_tracker(&active.process_tracker_done);
        let mut etw_files = HashSet::new();
        let mut etw_operations = Vec::new();
        let mut notes = Vec::new();
        let mut etw_incomplete = false;
        if let Some(capture) = active.etw_capture.as_ref() {
            notes.push(if active.validation_only {
                "再次验证只观察已选候选目录".to_string()
            } else {
                "ETW 优先模式：依据 ETW 写入证据生成候选范围".to_string()
            });
            let mut tracker = active.tracked_processes.lock()
                .map_err(|_| "读取游戏进程树失败".to_string())?;
            let managed = active.roots.iter()
                .find(|root| root.root_type == SaveRootType::ManagedGame)
                .map(|root| root.physical_path.as_path());
            if let Err(error) = tracker.refresh(managed) {
                notes.push(format!("最后一次刷新游戏进程树失败：{error}"));
            }
            match collect_related_files_by_trace(
                capture,
                move |capture_end| tracker.lifetimes(capture_end),
            ) {
                Ok(collection) => {
                    etw_files = collection.files.iter().map(|path| normalize_path(Path::new(path))).collect();
                    etw_operations = collection.operations;
                    etw_incomplete = collection.incomplete;
                    notes.extend(collection.logs);
                }
                Err(error) => notes.push(format!("无法使用 ETW 证据：{error}")),
            }
            let _ = std::fs::remove_file(&capture.etl_path);
        } else if let Some(error) = active.etw_start_error.as_ref() {
            notes.push(format!("ETW 未启动，已使用快照差异：{error}"));
        }
        let etw_write_count = etw_files.len();
        let trace = FileOperationTrace::new(etw_operations);
        filter_etw_candidate_files(&mut etw_files, &trace);
        if etw_write_count > 0 {
            notes.push(format!(
                "ETW 候选筛选：写入路径 {etw_write_count} 个，保留 {} 个，因缺少存档线索、完整写入或命中噪音规则排除 {} 个",
                etw_files.len(),
                etw_write_count - etw_files.len(),
            ));
        }
        let baseline = active.baseline.as_ref();
        let mut event_capture_mode = learning_capture_mode(baseline.is_some(), !etw_files.is_empty(), etw_incomplete);
        if etw_files.is_empty() {
            if active.baseline.is_some() {
                notes.push("未捕获到有效 ETW 文件，已自动使用快照差异比对".to_string());
            } else {
                notes.push(
                    "没有可用的 ETW 存档写入证据，也没有保存前快照，无法执行快照差异比对。可重新识别或手动添加存档目录。"
                        .to_string(),
                );
            }
        }
        let mut effective_roots = active.roots.clone();
        if !etw_files.is_empty() && !active.validation_only {
            effective_roots = etw_effective_roots(&active.roots, &etw_files);
        }
        let final_snapshot = if etw_files.is_empty() {
            if baseline.is_some() {
                collect_snapshot(
                    &effective_roots,
                    |progress, message| on_progress(snapshot_analysis_progress(progress), message),
                    &is_cancelled,
                )?
            } else {
                HashMap::new()
            }
        } else {
            on_progress(45, "ETW 已定位文件，正在读取目标文件状态");
            let targeted = collect_targeted_snapshot(&effective_roots, &etw_files, &is_cancelled)?;
            notes.push(format!(
                "ETW 目标文件状态：候选 {} 个，当前范围内可用 {} 个",
                etw_files.len(),
                targeted.len(),
            ));
            if targeted.is_empty() {
                if baseline.is_some() {
                    event_capture_mode = "snapshot";
                    etw_files.clear();
                    effective_roots.clone_from(&active.roots);
                    notes.push("ETW 文件无法直接读取，已回退完整快照差异".to_string());
                    collect_snapshot(
                        &effective_roots,
                        |progress, message| {
                            on_progress(snapshot_analysis_progress(progress), message)
                        },
                        &is_cancelled,
                    )?
                } else {
                    event_capture_mode = "insufficient";
                    notes.push(
                        "ETW 文件无法直接读取，且没有保存前 baseline，无法执行快照差异回退"
                            .to_string(),
                    );
                    HashMap::new()
                }
            } else {
                targeted
            }
        };
        if is_cancelled() {
            return Err("任务已取消".to_string());
        }
        on_progress(92, "正在按文件夹整理存档候选");
        let analysis = matches!(event_capture_mode, "etw" | "etw_partial").then(|| {
            trace.analyze(
                |path| is_candidate_transaction_event(path, &etw_files),
                is_etw_write_source,
                |path| etw_files.contains(path) && final_snapshot.get(path)
                    .is_some_and(|file| file.size <= DEFAULT_MAX_FILE_BYTES),
            )
        });
        let no_completed_paths = HashMap::new();
        let completed_paths = analysis.as_ref().map(|result| &result.completed_paths)
            .unwrap_or(&no_completed_paths);
        let (changed_files, mut scope_drafts, mut inference_notes) = if event_capture_mode == "insufficient" {
            (Vec::new(), Vec::new(), Vec::new())
        } else {
            infer_scope_drafts_with_mode(
                &effective_roots, &final_snapshot, baseline, &etw_files, completed_paths,
                ScopeInferenceMode { preview: false, validation: active.validation_only },
            )
        };
        if is_cancelled() {
            return Err("任务已取消".to_string());
        }
        notes.append(&mut inference_notes);
        let mut transaction_summary = analysis.map(|result| result.summary);
        let mut confidence = calculate_learning_confidence(
            &scope_drafts,
            event_capture_mode,
            transaction_summary.as_ref(),
        );
        if event_capture_mode == "etw_partial" {
            notes.push("ETW 采集不完整，现有记录仅作候选，不视为已完整验证。".into());
            confidence = confidence.min(64);
            for draft in &mut scope_drafts {
                draft.evidence_level = SaveCandidateEvidenceLevel::Review;
                draft.evidence_reason = "采集不完整，请核对候选路径或重新识别。".into();
                draft.confidence = draft.confidence.min(64);
                draft.scope.unknown_file_policy = UnknownFilePolicy::Ignore;
            }
            if let Some(summary) = &mut transaction_summary {
                if summary.status == "completed" {
                    summary.status = "candidate".into();
                }
                summary.confidence = summary.confidence.min(64);
            }
        }
        let registry_save_key = if scope_drafts.is_empty() && !active.validation_only {
            managed_executable_path(game)
                .ok()
                .and_then(|path| crate::services::learning::detect_unity_player_prefs(&path))
        } else {
            None
        };
        Ok(SaveLearningResult {
            session_id: active.view.session_id.clone(),
            changed_files,
            scope_drafts,
            confidence,
            notes,
            event_capture_mode: event_capture_mode.to_string(),
            transaction_summary,
            registry_save_key,
        })
    }
}

fn learning_capture_mode(has_baseline: bool, has_etw_evidence: bool, partial: bool) -> &'static str {
    match (has_baseline, has_etw_evidence, partial) {
        (_, true, true) => "etw_partial",
        (_, true, false) => "etw",
        (true, false, _) => "snapshot",
        (false, false, _) => "insufficient",
    }
}

fn snapshot_analysis_progress(scan_progress: u8) -> u8 {
    10 + ((u16::from(scan_progress) * 4 / 5) as u8)
}

/// 到点后停止 ETW 会话，给 `.etl` 一个上界。
///
/// `sleep` / `stop` 做成注入点，是为了让「到点真的会去停会话」可被测试钉住 —— 否则这条
/// 防护只能靠读代码确认。`stop` 返回错误不算异常：会话已被正常结束或被取消时，看门狗到点
/// 再停一次必然拿到错误，此时只记日志。
fn run_capture_watchdog(
    trace_name: &str,
    duration: Duration,
    timed_out: &AtomicBool,
    sleep: impl FnOnce(Duration),
    stop: impl FnOnce(&str) -> Result<(), String>,
) {
    sleep(duration);
    match stop(trace_name) {
        Ok(()) => {
            timed_out.store(true, Ordering::Release);
            crate::logging::info(format!(
                "ETW 会话已达时长上限（{} 分钟），已停止采集：{trace_name}",
                duration.as_secs() / 60
            ));
        }
        Err(error) => crate::logging::info(format!(
            "ETW 会话已达时长上限，停止时返回（会话可能已结束）：{trace_name} {error}"
        )),
    }
}

/// 在后台线程里按 [`MAX_CAPTURE_DURATION`] 兜住一次采集的时长。
fn spawn_capture_watchdog(trace_name: String, duration: Duration, timed_out: Arc<AtomicBool>) {
    thread::spawn(move || {
        run_capture_watchdog(&trace_name, duration, &timed_out, thread::sleep, stop_etw_capture);
    });
}

/// 装配采集看门狗：有句柄就按上限挂表，没有（ETW 不可用）就什么都不做。
///
/// 单独成函数是为了让「到底有没有把看门狗挂上去」可被断言。挂表动作本身只是
/// `thread::spawn`，删掉它不会有任何测试变红 —— 那样这条防护就会悄无声息地消失，
/// 而它正是 `.etl` 唯一由我们掌控的时长上界。
fn arm_capture_watchdog(
    capture: Option<&EtwCaptureHandle>,
    arm: impl FnOnce(String, Duration, Arc<AtomicBool>),
) {
    if let Some(handle) = capture {
        arm(handle.trace_name.clone(), MAX_CAPTURE_DURATION, Arc::clone(&handle.timed_out));
    }
}

fn spawn_process_tracker(
    tracked_processes: Arc<Mutex<LearningProcessTracker>>,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    managed_path: Option<PathBuf>,
) {
    thread::spawn(move || {
        let mut iterations: usize = 0;
        let mut idle_since = None;
        loop {
            let all_exited = if let Ok(mut current) = tracked_processes.lock() {
                let alive = current.any_alive();
                if should_refresh_tracked_processes(
                    &mut idle_since,
                    Instant::now(),
                    alive,
                    current.is_empty(),
                ) {
                    let _ = current.refresh(managed_path.as_deref());
                }
                !current.is_empty() && !alive
            } else {
                false
            };
            if stop.load(Ordering::Acquire) {
                if let Ok(mut current) = tracked_processes.lock() {
                    let _ = current.refresh(managed_path.as_deref());
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

fn should_refresh_tracked_processes(
    idle_since: &mut Option<Instant>,
    now: Instant,
    any_alive: bool,
    no_tracked_pids: bool,
) -> bool {
    if any_alive || no_tracked_pids {
        *idle_since = None;
        return true;
    }
    now.saturating_duration_since(*idle_since.get_or_insert(now)) < PROCESS_HANDOFF_GRACE
}

fn ensure_tracked_game_exited(
    tracker: &mut LearningProcessTracker,
    managed_path: &Path,
) -> Result<(), String> {
    if !tracker.any_alive() {
        tracker.refresh(Some(managed_path))?;
    }
    if tracker.any_alive() {
        return Err("游戏或其子进程仍在运行，请完全退出游戏后再试。".into());
    }
    Ok(())
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
    discover_scan_roots_with(game, false, &mut ScanCoverage::default())
}

#[derive(Default)]
struct ScanCoverage {
    unreadable: usize,
    limited: bool,
}

fn discover_scan_roots_with(
    game: &Game,
    preview: bool,
    coverage: &mut ScanCoverage,
) -> Result<Vec<crate::domain::ScanRoot>, String> {
    let managed_path = strip_verbatim_prefix(
        &PathBuf::from(&game.managed_path)
            .canonicalize()
            .map_err(|err| format!("解析受管游戏目录失败：{err}"))?,
    );
    let mut roots = vec![crate::domain::ScanRoot {
        root_type: SaveRootType::ManagedGame,
        physical_path: managed_path,
    }];
    let hints = if preview {
        preview_name_hints(game)
    } else {
        game_name_hints(game)
    };
    let mut locations = environment_scan_roots();
    if let Some(documents) = documents_root() {
        locations.push((documents, SaveRootType::Documents));
    }
    if let Some(profile) = env::var_os("USERPROFILE") {
        let profile_root = PathBuf::from(profile);
        locations.push((profile_root.join("Saved Games"), SaveRootType::SavedGames));
        locations.push((profile_root.join("AppData/LocalLow"), SaveRootType::LocalLow));
    }
    if let Some(public_documents) = public_documents_root() {
        locations.push((public_documents.join("Steam"), SaveRootType::Documents));
        locations.push((public_documents, SaveRootType::Documents));
    }
    locations.extend(find_steam_userdata_dirs().into_iter().map(|path| (path, SaveRootType::Custom)));
    let mut seen_locations = HashSet::new();
    for (root, root_type) in locations {
        if !seen_locations.insert(normalize_path(&root)) {
            continue;
        }
        for path in discover_named_directories(&root, &hints, preview, coverage) {
            roots.push(crate::domain::ScanRoot {
                root_type,
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
            Some((PathBuf::from(env::var_os(variable)?), *root_type))
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

/// 目录发现时的提示词匹配器：把「与具体提示词无关」的检查与规范化**每个目录只做一次**。
///
/// 起因（2026-09-13 实测）：`discover_scan_roots` 在一次添加游戏里要 116ms–1.9s，其中
/// `%APPDATA%` / `%LOCALAPPDATA%` / `%PROGRAMDATA%` 三处目录探测占绝大部分。原来的写法把
/// 「噪声目录 / 存档容器目录 / 目录名规范化 / `is_generic_hint` 黑名单查询」全放在
/// `hints.iter().any(..)` 的**逐 hint 调用里**，而这几件事与 hint 毫无关系 —— 于是同一份
/// 工作被重复了「提示词个数」遍。
struct DirectoryHintMatcher {
    /// 目录名的紧凑形式：去掉非字母数字并小写。
    compact_name: String,
    /// 目录名小写形式。
    name: String,
}

impl DirectoryHintMatcher {
    /// 返回 `None` 表示这个目录**在当前提示词集合下不可能命中任何一项**。
    ///
    /// **注意 `hints` 必须是调用方预先剔除通用词的切片**（见 `find_candidate_directories`）
    /// —— 这里**不**自己过滤、更不 `cloned()` 收集。第一版就是在这里收了一份
    /// `Vec<String>`，于是**每个被检视的目录都克隆一遍全部提示词**（真实机器上约 1 万多个
    /// 目录），整函数实测反而从 185.7ms 涨到 213.0ms。教训：这条热路径上**一次多余分配 ×
    /// 一万次 = 明显回退**，与「提出重复工作」的初衷正好相反。
    fn new(path: &Path, hints: &[String]) -> Option<Self> {
        let raw_name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        // `is_generic_hint` 自带 `trim`，所以对原始名判一次即可（涵盖空名的情况）。
        if is_generic_hint(raw_name) {
            return None;
        }
        let name = raw_name.to_lowercase();
        let compact_name = name
            .chars()
            .filter(|character| character.is_alphanumeric())
            .collect::<String>();
        // 空提示词集合 ⇒ 任何目录都不可能命中，直接省掉后续全部比较。
        if hints.is_empty() {
            return None;
        }
        Some(Self { compact_name, name })
    }

    /// `hints` 由调用方传入**已剔除通用词**的切片（不在这里过滤，避免每目录一次分配）。
    fn matches_any(&self, hints: &[String], exact: bool) -> bool {
        hints.iter().any(|hint| self.matches(hint, exact))
    }

    fn matches(&self, hint: &str, exact: bool) -> bool {
        let compact_hint = hint
            .chars()
            .filter(|character| character.is_alphanumeric())
            .collect::<String>();
        if compact_hint.is_empty() {
            return false;
        }
        if self.name == hint || self.compact_name == compact_hint {
            return true;
        }
        if !exact && compact_hint.chars().count() >= 4 {
            if self.compact_name.starts_with(&compact_hint) {
                return true;
            }
            if self
                .name
                .split(['_', '-', '.', ' '])
                .any(|token| token == hint || token == compact_hint)
            {
                return true;
            }
            if compact_hint.chars().count() >= 5 && self.compact_name.contains(&compact_hint) {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
fn find_candidate_directories(root: &Path, hints: &[String]) -> Vec<PathBuf> {
    discover_named_directories(root, hints, false, &mut ScanCoverage::default())
}

fn discover_named_directories(
    root: &Path,
    hints: &[String],
    preview: bool,
    coverage: &mut ScanCoverage,
) -> Vec<PathBuf> {
    // 通用词提示词对**任何**目录都不匹配（原逻辑在逐 hint 调用里判），所以在这里、
    // 每个根只剔除一次。这个切片随后被所有目录共享，不产生逐目录分配。
    let usable_hints: Vec<String> = hints
        .iter()
        .filter(|hint| !is_generic_hint(hint))
        .cloned()
        .collect();
    let mut candidates = Vec::new();
    if usable_hints.is_empty() {
        return candidates;
    }
    let mut frontier = vec![root.to_path_buf()];
    let max_depth = if preview { PREVIEW_DISCOVERY_DEPTH } else { 3 };
    let mut visited = 0;
    for depth in 0..max_depth {
        let mut next = Vec::new();
        for parent in frontier {
            let in_packages = parent
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("Packages"));
            let entries = match fs::read_dir(parent) {
                Ok(entries) => entries,
                Err(error) if depth == 0 && error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => {
                    coverage.unreadable += 1;
                    continue;
                }
            };
            for entry in entries {
                if preview && visited >= PREVIEW_DISCOVERY_ENTRIES_PER_LOCATION {
                    coverage.limited = true;
                    return candidates;
                }
                visited += 1;
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(_) => {
                        coverage.unreadable += 1;
                        continue;
                    }
                };
                let path = entry.path();
                let Ok(file_type) = entry.file_type() else {
                    coverage.unreadable += 1;
                    continue;
                };
                if !file_type.is_dir() {
                    continue;
                }
                // 这两项与提示词无关，判一次就够（原先在逐 hint 调用里被重复判断）。
                if is_scan_noise_directory(&path) {
                    continue;
                }
                if in_packages {
                    if package_matches_game(&entry.file_name().to_string_lossy(), &usable_hints) {
                        candidates.push(path);
                    }
                    continue;
                }
                let is_container = is_save_container_directory(&path)
                    || entry.file_name().eq_ignore_ascii_case("Packages");
                if !is_container
                    && DirectoryHintMatcher::new(&path, &usable_hints)
                        .is_some_and(|matcher| matcher.matches_any(&usable_hints, preview))
                {
                    candidates.push(path);
                } else if depth + 1 < max_depth {
                    next.push(path);
                } else {
                    coverage.limited = true;
                }
            }
        }
        frontier = next;
    }
    candidates
}

fn package_matches_game(name: &str, hints: &[String]) -> bool {
    let Some((identity, _publisher)) = name.rsplit_once('_') else {
        return false;
    };
    let product = identity.rsplit('.').next().unwrap_or(identity);
    let compact = product.chars().filter(|character| character.is_alphanumeric())
        .collect::<String>().to_lowercase();
    !compact.is_empty() && hints.iter().any(|hint| {
        hint.chars().filter(|character| character.is_alphanumeric()).eq(compact.chars())
    })
}

fn preview_name_hints(game: &Game) -> Vec<String> {
    let executable = Path::new(&game.launch.executable_relative_path)
        .file_stem()
        .and_then(|stem| stem.to_str());
    std::iter::once(game.display_name.as_str())
        .chain(executable)
        .filter(|name| !is_generic_hint(name))
        .map(|name| {
            name.chars().filter(|character| character.is_alphanumeric())
                .collect::<String>().to_lowercase()
        })
        .filter(|name| name.chars().count() >= 2 && !is_generic_hint(name))
        .collect()
}

/// 目录发现是否**只**找到了游戏安装目录本身。
///
/// 评审 A3：`find_candidate_directories` 靠「目录名里出现游戏名」来找扫描根。目录名不含游戏名时
/// （日系 / 老游戏常见的 `%APPDATA%\<厂商>\<游戏>`，或与游戏名毫无关系的目录名），普通权限下
/// 一个根都找不到 —— 症状只是「没有发现变化」，用户既不知道是发现环节出的问题，也很容易误以为
/// 「改用完整识别就能解决」（其实不能：完整识别走的是同一份发现逻辑）。
///
/// **这里刻意不猜。** 自动兜底只能靠「目录名像存档容器」，而 `%APPDATA%` 下同时存在好几个这样的
/// 目录（别人的存档），猜错就是把别人的存档收进版本库 —— 误收比漏收更难收拾。所以只把失败说清楚、
/// 把出路指明白，猜的工作留给用户。
///
/// 除 ManagedGame 外的根**全部**来自 `find_candidate_directories` 的命中，所以「一个非
/// ManagedGame 根都没有」等价于「提示词一处都没命中」。
fn discovered_only_the_install_dir(roots: &[crate::domain::ScanRoot]) -> bool {
    !roots
        .iter()
        .any(|root| root.root_type != SaveRootType::ManagedGame)
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
    documents: Option<&Path>,
    public_documents: Option<&Path>,
) -> Option<crate::domain::ScanRoot> {
    let norm_file = normalize_path(file_path);

    if !is_automatic_save_file(file_path) {
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
    let bases: [(Option<PathBuf>, SaveRootType); 8] = [
        (Some(local_low), SaveRootType::LocalLow),
        (Some(saved_games), SaveRootType::SavedGames),
        (app_data, SaveRootType::AppData),
        (local_app_data, SaveRootType::LocalAppData),
        (program_data, SaveRootType::ProgramData),
        (documents.map(Path::to_path_buf), SaveRootType::Documents),
        (public_documents.map(Path::to_path_buf), SaveRootType::Documents),
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
            } else if components.len() >= 3
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

/// ETW-first 结束分析只保留受管游戏目录和 ETW 实际命中的外部目录。
///
/// 启动前按名称发现的目录仍用于快照回退，但 ETW 已经给出事实证据后，不能再把
/// 所有自动发现目录带入容器兜底扫描，否则其他游戏的 AppData/LocalLow 也会被加入候选。
fn etw_effective_roots(
    active_roots: &[crate::domain::ScanRoot],
    etw_files: &HashSet<String>,
) -> Vec<crate::domain::ScanRoot> {
    let mut roots = Vec::new();
    let documents = documents_root();
    let public_documents = public_documents_root();
    let managed_path = active_roots
        .iter()
        .find(|root| root.root_type == SaveRootType::ManagedGame)
        .map(|root| root.physical_path.as_path());

    if let Some(managed_root) = active_roots
        .iter()
        .find(|root| root.root_type == SaveRootType::ManagedGame)
    {
        roots.push(managed_root.clone());
    }

    for etw_file in etw_files {
        if !is_automatic_save_file(Path::new(etw_file)) {
            continue;
        }

        if let Some(root) = active_roots
            .iter()
            .filter(|root| path_is_within_root(etw_file, &root.physical_path))
            .max_by_key(|root| root.physical_path.components().count())
        {
            roots.push(root.clone());
        } else if let Some(inferred) =
            infer_scan_root_for_etw_file(
                Path::new(etw_file), managed_path, documents.as_deref(), public_documents.as_deref(),
            )
        {
            roots.push(inferred);
        }
    }

    let mut seen = HashSet::new();
    roots.retain(|root| seen.insert(normalize_path(&root.physical_path)));
    roots
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

fn collect_snapshot(
    roots: &[crate::domain::ScanRoot],
    on_progress: impl Fn(u8, &str),
    is_cancelled: &impl Fn() -> bool,
) -> Result<HashMap<String, FileFingerprint>, String> {
    collect_snapshot_with_coverage(roots, on_progress, is_cancelled, None)
}

fn collect_snapshot_with_coverage(
    roots: &[crate::domain::ScanRoot],
    on_progress: impl Fn(u8, &str),
    is_cancelled: &impl Fn() -> bool,
    mut preview: Option<&mut ScanCoverage>,
) -> Result<HashMap<String, FileFingerprint>, String> {
    let mut files = HashMap::new();
    let mut remaining = PREVIEW_SNAPSHOT_ENTRIES;
    'roots: for (root_index, root) in roots.iter().enumerate() {
        if let Some(coverage) = &mut preview {
            if remaining == 0 {
                coverage.limited = true;
                break;
            }
        }
        if !root.physical_path.is_dir() {
            if let Some(coverage) = &mut preview {
                coverage.unreadable += 1;
            }
            continue;
        }
        let is_managed = root.root_type == SaveRootType::ManagedGame;
        let max_depth = match (is_managed, preview.is_some()) {
            (true, _) => Some(4),
            (false, true) => Some(PREVIEW_SNAPSHOT_DEPTH),
            (false, false) => None,
        };
        let mut walker = WalkDir::new(&root.physical_path).follow_links(false);
        if let Some(max_depth) = max_depth {
            walker = walker.max_depth(max_depth);
        }
        let mut root_remaining = PREVIEW_SNAPSHOT_ENTRIES_PER_ROOT;
        let mut entries = walker.into_iter();
        while let Some(entry) = entries.next() {
            if is_cancelled() {
                return Err("任务已取消".to_string());
            }
            if let Some(coverage) = &mut preview {
                if remaining == 0 {
                    coverage.limited = true;
                    break 'roots;
                }
                if root_remaining == 0 {
                    coverage.limited = true;
                    break;
                }
                remaining -= 1;
                root_remaining -= 1;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    if let Some(coverage) = &mut preview {
                        coverage.unreadable += 1;
                    }
                    crate::logging::warn(format!("扫描跳过不可访问路径: {err}"));
                    continue;
                }
            };
            if is_noise_path(entry.path()) || (is_managed && is_managed_game_asset_dir(entry.path())) {
                if entry.file_type().is_dir() {
                    entries.skip_current_dir();
                }
                continue;
            }
            if let Some(coverage) = &mut preview {
                if entry.file_type().is_dir() && max_depth == Some(entry.depth()) {
                    coverage.limited = true;
                }
            }
            if !entry.file_type().is_file() {
                continue;
            }
            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(_) => {
                    if let Some(coverage) = &mut preview {
                        coverage.unreadable += 1;
                    }
                    continue;
                }
            };
            let path = normalize_path(entry.path());
            if metadata.len() > DEFAULT_MAX_FILE_BYTES
                && (preview.is_some() || !is_save_candidate(&path))
            {
                continue;
            }
            files.insert(
                path,
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
    mode: ScopeInferenceMode,
    shared_scan_root: bool,
) -> Option<(PathBuf, String)> {
    let parent = file_path.parent()?;
    let scan_root_norm = normalize_path(scan_root_path);
    let root = if mode.validation {
        scan_root_path.to_path_buf()
    } else {
        parent.ancestors()
            .take_while(|current| path_is_within_root(&normalize_path(current), scan_root_path))
            .find(|current| {
                is_save_container_directory(current)
                    && (!shared_scan_root || normalize_path(current) != scan_root_norm)
                    && (*current == parent || is_slot_branch(file_path, current))
            })
            .unwrap_or(parent)
            .to_path_buf()
    };
    let root_norm = normalize_path(&root);
    let file_norm = normalize_path(file_path);
    let relative = file_norm.strip_prefix(&root_norm)?.strip_prefix('\\')?.replace('\\', "/");
    Some((root, relative))
}

fn is_potentially_shared_save_root(root: &crate::domain::ScanRoot) -> bool {
    root.root_type != SaveRootType::ManagedGame
        && is_save_container_directory(&root.physical_path)
        && !root.physical_path.file_name().is_some_and(|name| {
            name.to_string_lossy().bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn is_slot_branch(file: &Path, container: &Path) -> bool {
    let Some(branch) = file.strip_prefix(container).ok()
        .and_then(|relative| relative.components().next())
    else {
        return false;
    };
    let name = branch.as_os_str().to_string_lossy();
    let compact = name.bytes().filter(u8::is_ascii_alphanumeric)
        .map(|byte| byte.to_ascii_lowercase()).collect::<Vec<_>>();
    [b"slot".as_slice(), b"save", b"profile"].iter().any(|prefix| {
        compact.strip_prefix(*prefix)
            .is_some_and(|suffix| !suffix.is_empty() && suffix.iter().all(u8::is_ascii_digit))
    })
}

/// 只在实际候选范围内查找未观察到变化的疑似存档，不向外扩扫。
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
        .take(MAX_PROPOSAL_SCAN_ENTRIES)
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
#[cfg(test)]
fn infer_scope_drafts(
    roots: &[crate::domain::ScanRoot],
    final_snapshot: &HashMap<String, FileFingerprint>,
    baseline: Option<&HashMap<String, FileFingerprint>>,
    etw_files: &HashSet<String>,
    etw_operations: &[FileOperation],
) -> (Vec<String>, Vec<SaveScopeDraft>, Vec<String>) {
    let analysis = crate::services::learning::analyze_save_transactions_for(etw_operations.to_vec(), |path| {
        etw_files.contains(path) && final_snapshot.get(path)
            .is_some_and(|file| file.size <= DEFAULT_MAX_FILE_BYTES)
    });
    infer_scope_drafts_with_mode(
        roots, final_snapshot, baseline, etw_files, &analysis.completed_paths, ScopeInferenceMode::default(),
    )
}

#[derive(Clone, Copy, Default)]
struct ScopeInferenceMode {
    preview: bool,
    validation: bool,
}

fn infer_scope_drafts_with_mode(
    roots: &[crate::domain::ScanRoot],
    final_snapshot: &HashMap<String, FileFingerprint>,
    baseline: Option<&HashMap<String, FileFingerprint>>,
    etw_files: &HashSet<String>,
    completed_paths: &HashMap<String, FileOperationKind>,
    mode: ScopeInferenceMode,
) -> (Vec<String>, Vec<SaveScopeDraft>, Vec<String>) {
    let preview = mode.preview;
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
        files: Vec<String>,
        noise_exact: Vec<String>,
        root_type: SaveRootType,
        explicit_only: bool,
    }

    let mut groups: BTreeMap<String, ScopeGroup> = BTreeMap::new();
    let mut unhandled_noise: Vec<(String, &FileFingerprint)> = Vec::new();
    let mut oversize_skipped = 0usize;
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

        let candidate_by_etw = !etw_files.is_empty()
            && is_automatic_save_file(Path::new(path))
            && (is_etw_candidate(path) || completed_paths.contains_key(path));
        let candidate_by_snapshot = etw_files.is_empty() && is_save_candidate(path);
        let is_candidate = candidate_by_etw || candidate_by_snapshot;

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
        let shared_scan_root = is_potentially_shared_save_root(scan_root);
        let Some((scope_root, relative)) =
            resolve_scope_root_and_relative(file_path, &scan_root.physical_path, mode, shared_scan_root)
        else {
            continue;
        };

        if is_candidate {
            if !final_snapshot.contains_key(path) {
                continue;
            }
            if fingerprint.size > DEFAULT_MAX_FILE_BYTES {
                oversize_skipped += 1;
                continue;
            }
            let key = normalize_path(&scope_root);
            let explicit_only = shared_scan_root && key == normalize_path(&scan_root.physical_path);
            let entry = groups.entry(key).or_insert_with(|| ScopeGroup {
                physical_path: scope_root,
                files: Vec::new(),
                noise_exact: Vec::new(),
                root_type: scan_root.root_type,
                explicit_only,
            });
            entry.explicit_only |= explicit_only;
            entry.files.push(relative);
        } else {
            unhandled_noise.push((path.clone(), fingerprint));
        }
    }

    for (noise_path, _) in unhandled_noise {
        let noise_norm = normalize_path(Path::new(&noise_path));
        let mut path = noise_norm.as_str();
        while let Some((parent, _)) = path.rsplit_once('\\') {
            if let Some(group) = groups.get_mut(parent) {
                let rel = noise_norm[parent.len() + 1..].replace('\\', "/");
                if !rel.is_empty() && !group.files.contains(&rel) {
                    group.noise_exact.push(rel);
                }
            }
            path = parent;
        }
    }

    let mut drafts = Vec::new();
    for (_, mut group) in groups {
        group.files.sort();
        group.files.dedup();
        group.noise_exact.sort();
        group.noise_exact.dedup();

        let protects_container = is_save_container_directory(&group.physical_path);
        let exclude_patterns: Vec<String> = DEFAULT_EXCLUDE_PATTERNS
            .iter()
            .map(|s| s.to_string())
            .collect();
        let exclude_directories: Vec<String> = DEFAULT_EXCLUDE_DIRECTORIES
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut exclude_exact: Vec<String> = Vec::new();

        for noise in group.noise_exact {
            if group.files.contains(&noise) {
                continue;
            }
            // 刻意**不**按噪音文件的扩展名注入 `*.ext`：
            // `is_excluded` 同时喂收集侧和 `scope_matches_entry_*`（恢复归组），一旦出现
            // 一个 `settings.json` 就把 `*.json` 整族排除，同组里真存档若也是 `.json`
            // 会既不被备份、又让恢复找不到范围。伴生噪音只排除其精确路径。
            if !exclude_exact.contains(&noise) {
                exclude_exact.push(noise);
            }
        }

        // 默认 Protect = 前端徽章上的「自动保护新存档」，也正是 R2b 补上的那一半：学习**之后**
        // 才出现的档位、游戏写出的带时间戳自动存档，会按候选启发式自动纳入版本库。用户想收窄成
        // 「仅保护已确认文件」，在向导里把这个范围切到 Ignore 即可。
        let has_unknown_format = !etw_files.is_empty() && group.files.iter().any(|file| {
            !is_etw_candidate(&normalize_path(&group.physical_path.join(file)))
        });
        let unknown_file_policy = if preview || group.explicit_only || has_unknown_format {
            UnknownFilePolicy::Ignore
        } else {
            UnknownFilePolicy::Protect
        };

        let physical_root_str = strip_verbatim_prefix(&group.physical_path)
            .to_string_lossy()
            .replace('/', "\\");

        let (evidence_level, evidence_reason, confidence) = classify_scope_evidence(
            &group.physical_path,
            &group.files,
            etw_files.is_empty(),
            completed_paths,
        );
        let mut scope = SaveScope {
            root_type: group.root_type,
            root_path: physical_root_str,
            confirmed_files: group.files.clone(),
            include_directories: vec![".".to_string()],
            exclude_exact,
            exclude_patterns,
            exclude_directories,
            unknown_file_policy,
            max_file_bytes: Some(DEFAULT_MAX_FILE_BYTES),
        };
        for file in &group.files {
            while let Some(rule) = matching_exclusion(file, &scope).map(str::to_owned) {
                scope.exclude_exact.retain(|value| value != &rule);
                scope.exclude_patterns.retain(|value| value != &rule);
                scope.exclude_directories.retain(|value| value != &rule);
            }
        }
        let mut proposed_files = if preview || mode.validation || group.explicit_only
            || (etw_files.is_empty() && protects_container)
        {
            Vec::new()
        } else {
            propose_directory_saves(&group.physical_path, &group.files)
        };
        proposed_files.retain(|file| matching_exclusion(file, &scope).is_none());
        if !etw_files.is_empty() && !proposed_files.is_empty() {
            scope.unknown_file_policy = UnknownFilePolicy::Ignore;
        }
        drafts.push(SaveScopeDraft {
            scope,
            changed_files: group.files,
            proposed_files,
            confidence,
            evidence_level,
            evidence_reason,
        });
    }

    let limit_mb = DEFAULT_MAX_FILE_BYTES / (1024 * 1024);
    let mut notes = vec![(if preview {
        "初稿默认仅保护列表中的文件，不会自动纳入其他档位。"
    } else if etw_files.is_empty() {
        "当前使用快照差异按文件夹归类，已自动注入标准排除规则与伴生噪音过滤。"
    } else {
        "当前优先使用 ETW 写入证据按文件夹归类，已自动注入标准排除规则与伴生噪音过滤。"
    })
    .to_string()];
    if drafts.is_empty() && !preview && oversize_skipped == 0 {
        if discovered_only_the_install_dir(roots) {
            // A3：这时候「换个方式再识别一次」是无效建议 —— 完整识别走的是同一份发现逻辑。
            notes.push(
                "没有发现符合存档特征的变化，而且目录发现只找到了游戏安装目录：存档目录的名字里没有出现游戏名时，普通权限发现不了它（改用完整识别也是同一份发现逻辑，同样找不到）。请在确认界面手动添加存档目录，或以管理员身份重试。"
                    .to_string(),
            );
        } else {
            notes.push(
                "没有发现符合存档特征的变化，请确认游戏内完成了一次保存，或手动添加存档目录。"
                    .to_string(),
            );
        }
    } else if !drafts.is_empty() {
        notes.push(format!(
            "默认只保护 {limit_mb} MB 以内的存档文件，大文件、日志与噪音缓存已自动排除。"
        ));
    }
    let observed_unknown_files = drafts.iter().map(|draft| {
        draft.scope.confirmed_files.iter().filter(|file| {
            !is_etw_candidate(&normalize_path(&Path::new(&draft.scope.root_path).join(file)))
        }).count()
    }).sum::<usize>();
    if !etw_files.is_empty() && observed_unknown_files > 0 {
        notes.push(format!(
            "ETW 已记录 {observed_unknown_files} 个未知格式候选的完整写入，请核对文件。启用自动保护后，也可保护同目录中扩展名和编号命名规则一致的新档位。"
        ));
    }
    if oversize_skipped > 0 {
        notes.push(format!(
            "已跳过 {oversize_skipped} 个超过 {limit_mb} MB 的文件：它们不会进入版本库，恢复时也不受保护。"
        ));
        if drafts.is_empty() {
            notes.push("本次观察到存档候选，但没有符合大小上限的可保护文件。".to_string());
        }
    }
    let proposed_total: usize = drafts.iter()
        .filter(|draft| draft.scope.unknown_file_policy == UnknownFilePolicy::Ignore)
        .map(|draft| draft.proposed_files.len()).sum();
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
    let (_, mut drafts, notes) = infer_scope_drafts_with_mode(
        roots, snapshot, Some(&empty_baseline), &etw_files, &HashMap::new(),
        ScopeInferenceMode { preview: true, validation: false },
    );

    for draft in &mut drafts {
        draft.evidence_level = SaveCandidateEvidenceLevel::Review;
        draft.evidence_reason =
            "只读初稿：没有启动游戏、没有任何写入证据，仅按目录名与文件特征推断。请确认内容，或改回完整识别。"
                .to_string();
    }
    (drafts, notes)
}

/// 组装只读初稿的返回结构。
///
/// 从 `preview_scope_drafts` 里抽出来，是为了让「证据诚实」这条**能被单元测试钉住**：初稿没有
/// 观察过任何变化，所以 `changed_files` 必须为空、模式必须标成 `preview`、也谈不上事务摘要。
/// 这三件事只要错一件，界面就会把一份猜出来的范围渲染成「有写入证据」的样子。
fn preview_result(
    drafts: Vec<SaveScopeDraft>,
    mut notes: Vec<String>,
    root_count: usize,
    coverage: &ScanCoverage,
) -> SaveLearningResult {
    let mut summary = if coverage.unreadable > 0 || coverage.limited {
        "只读初稿：扫描未覆盖所有位置。".to_string()
    } else {
        format!("只读初稿：已检查 {root_count} 个候选目录，没有记录游戏写入。")
    };
    if coverage.unreadable > 0 {
        summary.push_str(&format!("有 {} 项无法读取。", coverage.unreadable));
    }
    if coverage.limited {
        summary.push_str("部分位置达到扫描深度或数量上限。");
    }
    if drafts.is_empty() {
        summary.push_str("已检查的位置没有推断出候选范围；仍可启动识别或手动添加存档目录。");
    }
    notes.insert(0, summary);
    let confidence = calculate_learning_confidence(&drafts, PREVIEW_CAPTURE_MODE, None);
    SaveLearningResult {
        session_id: String::new(),
        // 没有观察任何变化，就不能报「N 个文件发生变化」。
        changed_files: Vec::new(),
        scope_drafts: drafts,
        confidence,
        notes,
        event_capture_mode: PREVIEW_CAPTURE_MODE.to_string(),
        transaction_summary: None,
        registry_save_key: None,
    }
}

/// 只读推断一份存档范围初稿（不启动游戏、不采集 ETW）。
fn preview_scope_drafts(game: &Game) -> Result<SaveLearningResult, String> {
    let mut coverage = ScanCoverage::default();
    let roots = discover_scan_roots_with(game, true, &mut coverage)?;
    let snapshot = collect_snapshot_with_coverage(&roots, |_, _| {}, &|| false, Some(&mut coverage))?;
    let (drafts, notes) = preview_drafts_from_roots(&roots, &snapshot);
    Ok(preview_result(drafts, notes, roots.len(), &coverage))
}

fn classify_scope_evidence(
    scope_root: &Path,
    confirmed_files: &[String],
    snapshot_only: bool,
    completed_paths: &HashMap<String, FileOperationKind>,
) -> (SaveCandidateEvidenceLevel, String, u8) {
    let file_count = confirmed_files.len();
    if snapshot_only {
        return (
            SaveCandidateEvidenceLevel::Review,
            "仅有候选目录快照变化，建议再次保存确认。".to_string(),
            if file_count >= 2 { 70 } else { 60 },
        );
    }

    if confirmed_files.iter().any(|file| {
        let path = normalize_path(&scope_root.join(file));
        !is_etw_candidate(&path) && completed_paths.contains_key(&path)
    }) {
        return (
            SaveCandidateEvidenceLevel::Review,
            "ETW 已关联到文件的完整写入，但缺少常见存档特征，请确认这些文件是否为存档。".to_string(),
            72,
        );
    }

    let mut completed = confirmed_files.iter()
        .filter_map(|file| completed_paths.get(&normalize_path(&scope_root.join(file))));
    match completed.next() {
        Some(first) if *first == FileOperationKind::Rename || completed.any(|kind| *kind == FileOperationKind::Rename) => (
            SaveCandidateEvidenceLevel::Strong, "ETW 已关联到写入后重命名为该存档的文件。".to_string(), 92,
        ),
        Some(_) => (
            SaveCandidateEvidenceLevel::Strong, "ETW 已关联到同一存档的写入与关闭。".to_string(), 88,
        ),
        None => (
            SaveCandidateEvidenceLevel::Review,
            "检测到候选文件变更，但没有足够证据确认同一存档完成写入并关闭或重命名。".to_string(),
            if file_count >= 2 { 75 } else { 68 },
        ),
    }
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
    let ceiling = if scope_drafts.iter().any(|draft| draft.evidence_level == SaveCandidateEvidenceLevel::Strong) { 95 } else { 84 };
    total.clamp(30, ceiling) as u8
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
/// 对齐；已通过完整写入证据进入候选的未知格式也保留。原子保存的中间产物：
/// `.tmp`/`.temp`/`.bak` 本身不是存档文件
/// （它们在草稿里落到 `noise_exact` → `exclude_exact`），但「写临时文件再重命名」
/// 正是最常见的保存动作；连同它们一起丢掉，会让真实的原子保存在事务摘要里退化成
/// 「证据不足」，反而丢掉它应得的置信度。噪音目录优先于这条例外——`\logs\x.tmp`
/// 仍然被 `should_ignore_event_path` 挡在前面。
fn filter_etw_candidate_files(files: &mut HashSet<String>, trace: &FileOperationTrace) {
    let completed_paths: HashSet<_> = trace.completed_writes()
        .filter(|(source, target)| {
            is_etw_write_source(&source.path) && is_automatic_save_file(Path::new(&target.path))
        })
        .map(|(_, target)| normalize_path(Path::new(&target.path)))
        .collect();
    files.retain(|path| is_etw_candidate(path) || completed_paths.contains(path));
}

fn is_etw_write_source(path: &str) -> bool {
    is_automatic_save_file(Path::new(path)) || is_transaction_evidence_path(path)
}

fn is_candidate_transaction_event(path: &str, candidates: &HashSet<String>) -> bool {
    is_transaction_evidence_path(path) || candidates.contains(&normalize_path(Path::new(path)))
}

#[cfg(test)]
fn transaction_evidence(operations: &[FileOperation], candidates: &HashSet<String>) -> Vec<FileOperation> {
    operations
        .iter()
        .filter(|operation| is_candidate_transaction_event(&operation.path, candidates))
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
        arm_capture_watchdog, calculate_learning_confidence, classify_scope_evidence, ActiveLearningSession,
        collect_snapshot, collect_targeted_snapshot,
        discovered_only_the_install_dir, etw_effective_roots, find_candidate_directories,
        infer_scope_drafts, is_etw_candidate, is_transaction_evidence_path, path_is_within_root,
        preview_drafts_from_roots, preview_result, propose_directory_saves, run_capture_watchdog,
        snapshot_analysis_progress, transaction_evidence,
        MAX_CAPTURE_DURATION,
    };
    use crate::domain::path_utils::normalize_path;
    use crate::domain::save_candidate::{
        is_noise_path, is_save_candidate, is_save_container_directory,
    };
    use crate::domain::{
        EtwCaptureHandle, FileFingerprint, LearningSessionView,
        LearningStatus, SaveCandidateEvidenceLevel, SaveRootType, SaveScope, SaveScopeDraft,
        SaveTransactionSummary, ScanRoot, UnknownFilePolicy, DEFAULT_MAX_FILE_BYTES,
    };
    use crate::services::learning::{analyze_save_transactions, FileOperation, FileOperationKind};
    use crate::test_support::TempWorkspace;
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    use std::sync::{atomic::AtomicBool, Arc, Mutex};
    use std::time::Duration;

    /// 建一棵带 `Drop` 清理的测试目录树，理由见 `crate::test_support::TempWorkspace`。
    ///
    /// 与 `save_repository::tests::temp_root` 同源：两处夹具都要真实目录树、都不能建在
    /// `%TEMP%` 下（那里被 `is_noise_path` 当噪音挡掉），于是都建在 crate 工作目录里。
    /// 清理挂在 `Drop` 上，失败路径与成功路径共用同一条逻辑，不再依赖测试末尾那行
    /// `remove_dir_all`（`assert!` 一失败就执行不到，目录会永久堆积）。
    fn temp_root(label: &str) -> TempWorkspace {
        TempWorkspace::new(label)
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn learning_rejects_analysis_until_the_tracked_process_exits() {
        let workspace = temp_root("learning-game-must-exit");
        let managed_path = workspace.join("managed");
        let mut child = std::process::Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let tracker = Mutex::new(super::LearningProcessTracker::from_child(&child));
        let blocked = super::SaveLearningService::ensure_game_exited(&managed_path, Some(&tracker));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(blocked.unwrap_err().contains("仍在运行"));
        super::SaveLearningService::ensure_game_exited(&managed_path, Some(&tracker)).unwrap();
    }

    #[test]
    fn learned_scope_keeps_confirmed_csv_and_ignores_noise_in_a_sibling_directory() {
        let workspace = temp_root("learn-save-exclusion-boundary");
        let root = workspace.join("game");
        let roots = [ScanRoot {
            root_type: SaveRootType::ManagedGame,
            physical_path: root.clone(),
        }];
        let snapshot = [
            "Saves/slot.csv",
            "Saves/shots/screenshot.png",
            "Saves_backup/screenshot.png",
        ]
        .into_iter()
        .map(|relative| {
            (
                normalize_path(&root.join(relative)),
                FileFingerprint { size: 8, modified_unix: 1 },
            )
        })
        .collect::<HashMap<_, _>>();
        let files = snapshot.keys().cloned().collect::<HashSet<_>>();
        let (_, drafts, _) = infer_scope_drafts(&roots, &snapshot, None, &files, &[]);
        assert_eq!(drafts.len(), 1);
        let scope = &drafts[0].scope;
        assert_eq!(scope.confirmed_files, ["slot.csv"]);
        assert_eq!(super::matching_exclusion("slot.csv", scope), None);
        assert!(scope.exclude_patterns.iter().any(|rule| rule == "*.log"));
        assert!(!scope.exclude_directories.iter().any(|rule| rule == "shots"));
        assert!(!scope.exclude_directories.iter().any(|rule| rule == "_backup"));
        assert_eq!(scope.exclude_exact, ["shots/screenshot.png"]);
    }

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
    fn process_tracker_keeps_scanning_after_the_launcher_exits() {
        use super::{should_refresh_tracked_processes, PROCESS_HANDOFF_GRACE};
        use std::time::Instant;

        let start = Instant::now();
        let mut idle_since = None;
        assert!(should_refresh_tracked_processes(&mut idle_since, start, true, false));

        let launcher_exit = start + Duration::from_millis(500);
        assert!(should_refresh_tracked_processes(&mut idle_since, launcher_exit, false, false));
        let successor = launcher_exit + Duration::from_secs(2);
        assert!(should_refresh_tracked_processes(&mut idle_since, successor, false, false));
        assert!(should_refresh_tracked_processes(&mut idle_since, successor, true, false));
        assert_eq!(idle_since, None);

        let successor_exit = successor + Duration::from_secs(15);
        assert!(should_refresh_tracked_processes(&mut idle_since, successor_exit, false, false));
        assert!(!should_refresh_tracked_processes(
            &mut idle_since,
            successor_exit + PROCESS_HANDOFF_GRACE,
            false,
            false,
        ));
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
        let timed_out = AtomicBool::new(false);
        run_capture_watchdog(
            "GameSaverTrace_deadline",
            Duration::from_secs(60),
            &timed_out,
            |duration| slept = Some(duration),
            |name| {
                stopped = Some(name.to_string());
                Ok(())
            },
        );
        assert!(timed_out.load(std::sync::atomic::Ordering::Acquire));
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
        let timed_out = AtomicBool::new(false);
        run_capture_watchdog(
            "GameSaverTrace_gone",
            Duration::from_secs(1),
            &timed_out,
            |_| {},
            |_| {
                attempts += 1;
                Err("找不到数据收集器集".to_string())
            },
        );
        assert!(!timed_out.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(attempts, 1, "即便会报错也要尝试停一次");
    }

    /// 挂表动作本身只是 `thread::spawn`，删掉它不会有别的测试变红 —— 所以必须单独钉住
    /// 「会话启动时确实按上限挂了看门狗」，否则这条防护会悄无声息地消失。
    #[test]
    fn capture_watchdog_is_armed_for_an_active_session() {
        let handle = EtwCaptureHandle {
            trace_name: "GameSaverTrace_armed".to_string(),
            etl_path: PathBuf::from(r"C:\logs\armed.etl"),
            timed_out: Default::default(),
        };
        let mut armed = None;
        arm_capture_watchdog(Some(&handle), |name, duration, flag| {
            assert!(Arc::ptr_eq(&handle.timed_out, &flag));
            armed = Some((name, duration));
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
        arm_capture_watchdog(None, |_, _, _| armed = true);
        assert!(!armed, "ETW 不可用时没有会话可停，不该挂表");
    }

    #[test]
    fn etw_without_baseline_never_claims_snapshot_fallback() {
        assert_eq!(super::learning_capture_mode(false, false, false), "insufficient");
        assert_eq!(super::learning_capture_mode(false, false, true), "insufficient");
        assert_eq!(super::learning_capture_mode(true, false, true), "snapshot");
        assert_eq!(super::learning_capture_mode(false, true, true), "etw_partial");
        assert_eq!(super::learning_capture_mode(false, true, false), "etw");
    }

    #[test]
    fn etw_candidates_accept_non_resource_files() {
        assert!(is_etw_candidate(r"C:\GameSaver\save\slot.xml"));
        assert!(is_etw_candidate(r"C:\GameSaver\Data\profile.bin"));
    }

    #[test]
    fn unknown_save_formats_do_not_require_path_heuristics() {
        let root = r"C:\Users\Player\AppData\LocalLow\TiramisuLovesNtr\Ntraholic";
        for name in [
            "ntr1.dontdeletethisdata",
            "chapter1.never_seen_format",
            "user_settings.DONTDELETETHISDATA",
            "record001",
        ] {
            let path = format!(r"{root}\{name}");
            assert!(!is_etw_candidate(&path), "unexpected path heuristic: {name}");
            assert!(!is_save_candidate(&path), "unexpected snapshot heuristic: {name}");
            let operations = vec![
                operation(&path, FileOperationKind::Write),
                operation(&path, FileOperationKind::Close),
            ];
            let mut files = HashSet::from([normalize_path(Path::new(&path))]);
            super::filter_etw_candidate_files(&mut files, &super::FileOperationTrace::new(operations.clone()));
            assert_eq!(files.len(), 1, "completed unknown format: {name}");
            assert_eq!(transaction_evidence(&operations, &files).len(), 2);
        }
    }

    #[test]
    fn ntraholic_etw_writes_reach_review_scope_without_baseline() {
        let (root, mut roots, _) = preview_fixture(
            "ntraholic-etw",
            &[
                ("ntr1.dontdeletethisdata", b"save"),
                ("user_settings.dontdeletethisdata", b"settings"),
                ("Player.log", b"log"),
            ],
        );
        roots[0].root_type = SaveRootType::LocalLow;
        let names = ["ntr1.dontdeletethisdata", "user_settings.dontdeletethisdata"];
        let mut files = HashSet::new();
        let mut operations = Vec::new();
        for (index, name) in names.iter().enumerate() {
            let path = normalize_path(&root.join(name));
            files.insert(path.clone());
            for (kind, timestamp) in [
                (FileOperationKind::Write, 1_000),
                (FileOperationKind::Close, 1_100),
            ] {
                operations.push(FileOperation {
                    path: path.clone(),
                    operation: kind,
                    timestamp_ms: Some(timestamp),
                    pid: 10,
                    file_object_id: Some(format!("object-{index}")),
                });
            }
        }
        let trace = super::FileOperationTrace::new(operations);
        super::filter_etw_candidate_files(&mut files, &trace);
        let effective = etw_effective_roots(&roots, &files);
        let snapshot = collect_targeted_snapshot(&effective, &files, &|| false).unwrap();
        assert_eq!(snapshot.len(), 2);
        let analysis = trace.analyze(
            |path| super::is_candidate_transaction_event(path, &files),
            super::is_etw_write_source,
            |path| snapshot.contains_key(path),
        );
        assert_eq!(analysis.completed_paths.len(), 2);
        let (_, drafts, _) = super::infer_scope_drafts_with_mode(
            &effective, &snapshot, None, &files, &analysis.completed_paths, Default::default(),
        );
        assert_eq!(drafts.len(), 1);
        let draft = &drafts[0];
        assert_eq!(normalize_path(Path::new(&draft.scope.root_path)), normalize_path(&root));
        assert_eq!(draft.scope.root_type, SaveRootType::LocalLow);
        assert_eq!(draft.scope.confirmed_files, names);
        assert_eq!(draft.evidence_level, SaveCandidateEvidenceLevel::Review);
        assert!(draft.evidence_reason.contains("完整写入"));
        assert_eq!(draft.scope.unknown_file_policy, UnknownFilePolicy::Ignore);
    }

    #[test]
    fn unknown_format_preview_does_not_guess_without_write_evidence() {
        let (_root, roots, snapshot) = preview_fixture(
            "ntraholic-preview",
            &[("ntr1.dontdeletethisdata", b"save")],
        );
        let (drafts, _) = preview_drafts_from_roots(&roots, &snapshot);
        assert!(drafts.is_empty());
    }

    #[test]
    fn unknown_formats_require_completed_writes_and_keep_noise_filters() {
        let root = r"C:\Users\Player\AppData\LocalLow\Studio\Game";
        for (name, kinds) in [
            ("logs/record.xyz", vec![FileOperationKind::Write, FileOperationKind::Close]),
            ("cache/record.xyz", vec![FileOperationKind::Write, FileOperationKind::Close]),
            ("record.png", vec![FileOperationKind::Write, FileOperationKind::Close]),
            ("record.xyz", vec![FileOperationKind::Write]),
            ("record.xyz", vec![FileOperationKind::Close]),
            ("record.xyz", vec![FileOperationKind::Create, FileOperationKind::Close]),
        ] {
            let path = format!(r"{root}\{}", name.replace('/', "\\"));
            let operations = kinds.into_iter().map(|kind| operation(&path, kind)).collect::<Vec<_>>();
            let mut files = HashSet::from([normalize_path(Path::new(&path))]);
            super::filter_etw_candidate_files(&mut files, &super::FileOperationTrace::new(operations));
            assert!(files.is_empty(), "unexpected unknown-format candidate: {name}");
        }
    }

    #[test]
    fn unknown_format_atomic_rename_keeps_intermediate_evidence() {
        let target = r"C:\Users\Player\AppData\LocalLow\Studio\Game\record.novel_format";
        for intermediate in ["record.tmp", "91ac_4b7", "91ac_4b7.staging"] {
            let source = Path::new(target).with_file_name(intermediate);
            let mut write = operation(source.to_str().unwrap(), FileOperationKind::Write);
            write.file_object_id = Some("object".into());
            let mut rename = operation(target, FileOperationKind::Rename);
            rename.file_object_id = Some("object".into());
            let trace = super::FileOperationTrace::new(vec![write, rename]);
            let target_path = normalize_path(Path::new(target));
            let mut files = HashSet::from([normalize_path(&source), target_path.clone()]);
            super::filter_etw_candidate_files(&mut files, &trace);
            assert_eq!(files, HashSet::from([target_path.clone()]));
            let analysis = trace.analyze(
                |path| super::is_candidate_transaction_event(path, &files),
                super::is_etw_write_source,
                |path| files.contains(path),
            );
            assert!(matches!(
                analysis.completed_paths.get(&target_path),
                Some(FileOperationKind::Rename),
            ));
            assert_eq!(analysis.summary.operation_count, 2);
        }
    }

    #[test]
    fn unknown_format_rename_cannot_borrow_noise_or_reused_object_writes() {
        let target = r"C:\Game\record.novel_format";
        let target_path = normalize_path(Path::new(target));
        let mut rename = operation(target, FileOperationKind::Rename);
        rename.file_object_id = Some("object".into());

        let mut noise = operation(r"C:\Game\cache\record.tmp", FileOperationKind::Write);
        noise.file_object_id = rename.file_object_id.clone();
        let mut write = operation(r"C:\Game\original.unknown", FileOperationKind::Write);
        write.file_object_id = rename.file_object_id.clone();
        let mut reuse = operation(r"C:\Game\new_object.dll", FileOperationKind::Create);
        reuse.file_object_id = rename.file_object_id.clone();

        for operations in [vec![noise, rename.clone()], vec![write, reuse, rename]] {
            let mut files = HashSet::from([target_path.clone()]);
            let trace = super::FileOperationTrace::new(operations);
            super::filter_etw_candidate_files(&mut files, &trace);
            assert!(files.is_empty());
        }
    }

    #[test]
    fn unknown_format_writes_cannot_borrow_another_process_or_objects_close() {
        let path = r"C:\Users\Player\AppData\LocalLow\Studio\Game\record.unknown";
        for mismatch in ["pid", "object", "timestamp", "missing_timestamp"] {
            let mut write = operation(path, FileOperationKind::Write);
            write.file_object_id = Some("object".into());
            let mut close = operation(path, FileOperationKind::Close);
            close.file_object_id = Some("object".into());
            match mismatch {
                "pid" => close.pid += 1,
                "object" => close.file_object_id = Some("other-object".into()),
                "timestamp" => close.timestamp_ms = Some(4_000),
                _ => close.timestamp_ms = None,
            }
            let mut files = HashSet::from([normalize_path(Path::new(path))]);
            super::filter_etw_candidate_files(&mut files, &super::FileOperationTrace::new(vec![write, close]));
            assert!(files.is_empty(), "incorrect completion: {mismatch}");
        }
    }

    #[test]
    fn etw_unknown_format_can_infer_its_standard_external_root() {
        let profile = PathBuf::from(std::env::var_os("USERPROFILE").expect("user profile"));
        let directory = profile.join("AppData/LocalLow/Studio/Game");
        let path = normalize_path(&directory.join("record.unknown_format"));
        let mut files = HashSet::from([path.clone()]);
        let operations = vec![
            operation(&path, FileOperationKind::Write),
            operation(&path, FileOperationKind::Close),
        ];
        super::filter_etw_candidate_files(&mut files, &super::FileOperationTrace::new(operations));
        let effective = etw_effective_roots(&[], &files);
        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0].root_type, SaveRootType::LocalLow);
        assert_eq!(normalize_path(&effective[0].physical_path), normalize_path(&directory));
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
        let kept = transaction_evidence(&operations, &HashSet::new());
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
        let kept = transaction_evidence(&operations, &HashSet::new());
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
        let filtered = analyze_save_transactions(transaction_evidence(&operations, &HashSet::new()));
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
            &["slot.sav".to_string()],
            false,
            &crate::services::learning::analyze_save_transactions_for(operations, |_| true).completed_paths,
        );
        assert_eq!(level, SaveCandidateEvidenceLevel::Strong);
        assert!(confidence >= 88);
    }

    #[test]
    fn snapshot_only_scope_stays_review_candidate() {
        let (level, _, confidence) = classify_scope_evidence(
            Path::new(r"C:\Users\Player\SaveData"), &["slot.sav".to_string()], true, &HashMap::new(),
        );
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

    /// 2026-09-15 真实误报的回归护栏：一次建档里 ETW 追加了两个错误范围
    /// （LocalLow 的资源索引库、Steam 模拟器的状态文件），用户只保留了第三个。
    ///
    /// 路径原样取自那次会话，审计依据 `docs/save-recognition-audit-2026-09-15.md`。
    #[test]
    fn real_world_index_and_emulator_state_files_are_not_save_candidates() {
        // 成因 A：`%LOCALAPPDATA%`→`LocalLow` 下游戏自己的数据目录。这里是资源归档索引
        // （archiveList.sqlite）与安装级配置库（<uuid>.sqlite），名字与路径都没有任何线索，
        // 改动前仅凭 `.sqlite` / `.db` / `.vdf` 扩展名进候选。
        let local_low = r"C:\Users\Player\AppData\LocalLow\5 shrimps\Ancient Cultivatrix";
        for name in [
            "archiveList.sqlite",
            "b0476da758b34407915359dc20f8050a.sqlite",
            "settings.db",
            "telemetry.vdf",
        ] {
            let path = format!(r"{local_low}\{name}");
            assert!(!is_etw_candidate(&path), "不应是候选：{path}");
            assert!(!is_save_candidate(&path), "不应是候选：{path}");
        }

        // 对照组：库 / 配置扩展名**另有依据**时仍然成立，不能误伤真存档。
        assert!(is_etw_candidate(&format!(r"{local_low}\save.db")));
        assert!(is_etw_candidate(&format!(r"{local_low}\profile.sqlite")));
        assert!(is_etw_candidate(&format!(
            r"{local_low}\SaveData\blob.sqlite"
        )));

        // 成因 B：Steam 模拟器 appid 目录里的模拟器状态文件（RUNE 的成就 / 排行榜各 32 字节）。
        let rune = r"C:\Users\Public\Documents\Steam\RUNE\1903340";
        for name in [
            "achievements.ini",
            "leaderboards.ini",
            "steam_emu.ini",
            "steam_appid.txt",
        ] {
            let path = format!(r"{rune}\{name}");
            assert!(!is_etw_candidate(&path), "不应是候选：{path}");
            assert!(!is_save_candidate(&path), "不应是候选：{path}");
        }

        // 对照组：模拟器里真正的存档（Goldberg / RUNE 的 remote）必须仍然算候选。
        assert!(is_etw_candidate(&format!(r"{rune}\remote\slot1.sav")));
        assert!(is_save_candidate(&format!(r"{rune}\remote\slot1.sav")));

        // 那次会话里唯一正确的范围：347 字节的 JSON 靠 `save` 容器成立（扩展名不在白名单里）。
        assert!(is_etw_candidate(
            r"E:\GameSaverGames\games\564f9729-8f6f-4f01-9bd9-c6b6f461b597\Ancient Cultivatrix_Data\save\_settingData.json"
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

    /// 匹配只认**目录名自身**，不认父路径文本 —— 否则 `BlackMarket\UnrelatedPublisher`
    /// 会因为父目录叫 BlackMarket 而把子目录也当成候选。
    #[test]
    fn scan_root_matching_does_not_match_arbitrary_parent_path_text() {
        let root = temp_root("scan-parent-text");
        let hit = root.join("ApplePie").join("MonsterBlackMarket");
        let miss = root.join("BlackMarket").join("UnrelatedPublisher");
        fs::create_dir_all(&hit).expect("create hit dir");
        fs::create_dir_all(&miss).expect("create miss dir");

        let hints = vec!["blackmarket".to_string()];
        let found = find_candidate_directories(&root, &hints);
        let paths: Vec<String> = found.iter().map(|p| p.display().to_string()).collect();

        // 注意：命中是在第二层找到的（父目录 ApplePie 不含提示词，所以先下探）。
        assert!(
            found.iter().any(|p| p.ends_with("MonsterBlackMarket")),
            "目录名含提示词时应当命中：{paths:?}"
        );
        assert!(
            !found.iter().any(|p| p.ends_with("UnrelatedPublisher")),
            "父目录名含提示词不该让子目录命中：{paths:?}"
        );
    }

    #[test]
    fn etw_supplements_only_real_hit_scopes_and_keeps_old_files_as_proposals() {
        let root = temp_root("save-container");
        let save_data = root.join("SaveData");
        let other = root.join("OtherGame").join("SaveData");
        fs::create_dir_all(&save_data).expect("create SaveData directory");
        fs::create_dir_all(&other).expect("create unrelated container");
        let real = save_data.join("slot0.sav");
        fs::write(&real, b"save").expect("write save file");
        fs::write(save_data.join("slot1.sav"), b"old").expect("write old save");
        fs::write(other.join("slot2.sav"), b"other").expect("write unrelated save");
        let files = HashSet::from([normalize_path(&real), normalize_path(&other.join("missing.sav"))]);
        let roots = [ScanRoot { root_type: SaveRootType::LocalAppData, physical_path: root.to_path_buf() }];
        let snapshot = collect_targeted_snapshot(&roots, &files, &|| false).expect("targeted snapshot");
        let (changed, drafts, _) = infer_scope_drafts(&roots, &snapshot, None, &files, &[]);
        assert_eq!(changed, vec![normalize_path(&real)]);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].scope.confirmed_files, ["slot0.sav"]);
        assert_eq!(drafts[0].proposed_files, ["slot1.sav"]);
        assert_eq!(drafts[0].scope.unknown_file_policy, UnknownFilePolicy::Ignore);
        let missing = HashSet::from([normalize_path(&other.join("missing.sav"))]);
        let snapshot = collect_targeted_snapshot(&roots, &missing, &|| false).expect("missing target snapshot");
        assert!(snapshot.is_empty());
        assert!(infer_scope_drafts(&roots, &snapshot, None, &missing, &[]).1.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn candidate_grouping_keeps_shared_save_root_out_of_automatic_protection() {
        let parent = Path::new(r"c:\users\player\appdata\roaming\vendor");
        let shared = parent.join("saves");
        let game = shared.join("game-a");
        let nested = game.join("savedata");
        for (scan, folder) in [(&shared, &game), (&shared, &nested), (&parent.to_path_buf(), &game)] {
            let file = normalize_path(&folder.join("slot.sav"));
            let roots = [ScanRoot { root_type: SaveRootType::AppData, physical_path: scan.clone() }];
            let snapshot = HashMap::from([(file.clone(), FileFingerprint { size: 5, modified_unix: 1 })]);
            let (_, drafts, _) = infer_scope_drafts(&roots, &snapshot, None, &HashSet::from([file]), &[]);
            assert_eq!(drafts.len(), 1);
            assert_eq!(normalize_path(Path::new(&drafts[0].scope.root_path)), normalize_path(folder));
        }
        let file = normalize_path(&shared.join("game-a.sav"));
        let roots = [ScanRoot { root_type: SaveRootType::AppData, physical_path: shared }];
        let snapshot = HashMap::from([(file.clone(), FileFingerprint { size: 5, modified_unix: 1 })]);
        let (_, drafts, _) = infer_scope_drafts(&roots, &snapshot, None, &HashSet::from([file]), &[]);
        assert_eq!(drafts[0].scope.unknown_file_policy, UnknownFilePolicy::Ignore);
    }

    #[test]
    fn oversized_etw_and_snapshot_files_cannot_supply_a_protection_scope() {
        let root = temp_root("large-save");
        let save_data = root.join("SaveData");
        fs::create_dir_all(&save_data).expect("create save container");
        let medium = save_data.join("medium.sav");
        let large = save_data.join("large.sav");
        for (path, size) in [(&medium, 20 * 1024 * 1024), (&large, 101 * 1024 * 1024)] {
            fs::File::create(path).unwrap().set_len(size).unwrap();
        }
        let roots = [ScanRoot { root_type: SaveRootType::LocalAppData, physical_path: root.to_path_buf() }];
        let files = HashSet::from([normalize_path(&medium), normalize_path(&large)]);
        let snapshot = collect_snapshot(&roots, |_, _| {}, &|| false).expect("snapshot");
        assert_eq!(snapshot.len(), 2);
        let operations = [operation(&normalize_path(&medium), FileOperationKind::Write),
            operation(&normalize_path(&medium), FileOperationKind::Close)];
        for targets in [files, HashSet::new()] {
            let (_, drafts, notes) = infer_scope_drafts(&roots, &snapshot, Some(&HashMap::new()), &targets, &operations);
            assert!(drafts.is_empty());
            assert!(notes.iter().any(|note| note.contains("已跳过 2 个超过")));
            assert_eq!(calculate_learning_confidence(&drafts, "etw", None), 0);
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn etw_effective_roots_ignores_unmatched_discovered_roots() {
        let workspace = temp_root("etw-roots");
        let managed = workspace.join("managed-game");
        let discovered_other_game = workspace.join("other-game-save");
        let external_save = workspace.join("external-save");
        fs::create_dir_all(&managed).expect("create managed root");
        fs::create_dir_all(&discovered_other_game).expect("create discovered root");
        fs::create_dir_all(&external_save).expect("create external root");

        let roots = vec![
            ScanRoot {
                root_type: SaveRootType::ManagedGame,
                physical_path: managed.clone(),
            },
            ScanRoot {
                root_type: SaveRootType::LocalAppData,
                physical_path: discovered_other_game.clone(),
            },
            ScanRoot {
                root_type: SaveRootType::Documents,
                physical_path: external_save.clone(),
            },
        ];
        let etw_files = HashSet::from([
            normalize_path(&managed.join("save.sav")),
            normalize_path(&external_save.join("slot.sav")),
        ]);

        let effective = etw_effective_roots(&roots, &etw_files);
        let effective_paths: HashSet<String> = effective
            .iter()
            .map(|root| normalize_path(&root.physical_path))
            .collect();

        assert!(effective_paths.contains(&normalize_path(&managed)));
        assert!(effective_paths.contains(&normalize_path(&external_save)));
        assert!(!effective_paths.contains(&normalize_path(&discovered_other_game)));
    }

    /// R2：只认「学习那一刻的文件清单」的范围，必须把目录里**已经存在**的疑似存档列成
    /// 提议交给用户确认 —— 否则用户新建的档位、游戏写出的带时间戳自动存档永远不会被
    /// 发现，而且没有任何提示。
    ///
    /// 提议只收「像存档」的文件：`settings.ini` 与 `screenshot.png` 不在默认排除模式里
    /// （默认只挡 `*.log`/`*.tmp` 一类），但它们不是存档，不该被摆到用户面前。
    #[test]
    fn proposal_lists_only_unconfirmed_save_like_files() {
        let root = temp_root("proposal");
        fs::write(root.join("slot1.sav"), b"confirmed").expect("write confirmed save");
        fs::write(root.join("autosave_2026.sav"), b"new slot").expect("write new slot");
        fs::write(root.join("settings.ini"), b"[cfg]").expect("write config");
        fs::write(root.join("screenshot.png"), b"png").expect("write screenshot");

        let proposals = propose_directory_saves(&root, &["slot1.sav".to_string()]);

        assert_eq!(proposals, vec!["autosave_2026.sav".to_string()]);
    }

    fn preview_fixture(
        label: &str,
        files: &[(&str, &[u8])],
    ) -> (
        TempWorkspace,
        Vec<ScanRoot>,
        HashMap<String, FileFingerprint>,
    ) {
        let root = temp_root(label);
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
            physical_path: root.to_path_buf(),
        }];
        (root, roots, snapshot)
    }

    /// A1「跳过学习」的只读初稿：不启动游戏、不采集 ETW，只按目录名与文件特征推断。
    ///
    /// 两条断言分别钉住「能推出范围」和「证据口径诚实」—— 后者更重要：一份没有任何写入证据
    /// 的草稿，绝不能看起来像被证据确认过，否则用户会把猜出来的范围当成已确认的。
    #[test]
    fn preview_drafts_are_review_only_and_skip_non_save_files() {
        // 绑定为 `_root` 而不是丢弃：它是目录的守卫，必须活到测试结束（见 `TempWorkspace`）。
        let (_root, roots, snapshot) = preview_fixture(
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
        assert_eq!(draft.scope.unknown_file_policy, UnknownFilePolicy::Ignore);
        // 没有任何写入证据 ⇒ 证据等级必须是「待确认」，且说明里要写清这是只读推断。
        assert_eq!(draft.evidence_level, SaveCandidateEvidenceLevel::Review);
        assert!(
            draft.evidence_reason.contains("只读初稿"),
            "说明必须写清这是只读推断，实际：{}",
            draft.evidence_reason
        );
    }

    /// 目录里没有像存档的文件时初稿必须是空的 —— 不能凭目录名硬凑一个范围出来。
    #[test]
    fn preview_drafts_stay_empty_without_save_like_files() {
        let (_root, roots, snapshot) = preview_fixture(
            "preview-empty",
            &[("settings.ini", b"[cfg]"), ("readme.txt", b"hi")],
        );

        let (drafts, _notes) = preview_drafts_from_roots(&roots, &snapshot);

        assert!(
            drafts.is_empty(),
            "没有候选文件就不该有范围，实际推出 {} 个",
            drafts.len()
        );
    }

    /// 只读初稿的返回结构必须诚实：没有观察过变化，就不能报「有文件变化」、不能标成有证据的
    /// 采集模式、也不能凭空造一份事务摘要。界面完全靠这三个字段决定渲染「初稿」还是「识别结果」，
    /// 错一个用户就会把猜出来的范围当成已确认的。
    #[test]
    fn preview_result_never_claims_observed_changes() {
        let draft = SaveScopeDraft {
            scope: SaveScope {
                root_type: SaveRootType::AppData,
                root_path: r"C:\Users\Player\AppData\Roaming\DemoGame\SaveData".to_string(),
                confirmed_files: vec!["slot1.sav".to_string()],
                include_directories: vec![".".to_string()],
                exclude_exact: vec![],
                exclude_patterns: vec![],
                exclude_directories: vec![],
                unknown_file_policy: UnknownFilePolicy::Protect,
                max_file_bytes: Some(DEFAULT_MAX_FILE_BYTES),
            },
            changed_files: vec!["slot1.sav".to_string()],
            proposed_files: vec![],
            confidence: 70,
            evidence_level: SaveCandidateEvidenceLevel::Review,
            evidence_reason: "只读初稿（测试夹具）".to_string(),
        };

        let result = preview_result(vec![draft], vec![], 3, &super::ScanCoverage::default());

        assert!(
            result.changed_files.is_empty(),
            "初稿没有观察过任何变化，不能报 {} 个文件变化",
            result.changed_files.len()
        );
        assert_eq!(result.event_capture_mode, "preview");
        assert!(
            result.transaction_summary.is_none(),
            "没有采集就谈不上事务摘要"
        );
        assert!(result.session_id.is_empty(), "初稿不属于任何学习会话");
        assert!(
            result
                .notes
                .first()
                .is_some_and(|note| note.contains("只读初稿")),
            "首条说明必须写清这是只读初稿，实际：{:?}",
            result.notes
        );
        assert!(
            result.scope_drafts.len() == 1,
            "草稿应原样透传，实际 {} 个",
            result.scope_drafts.len()
        );
    }

    /// 初稿推不出任何范围时，说明里要给出下一步 —— 不能静默返回空结果让用户干瞪眼。
    #[test]
    fn preview_result_explains_an_empty_draft() {
        let result = preview_result(
            vec![],
            vec!["原有的采集说明".to_string()],
            0,
            &super::ScanCoverage::default(),
        );

        assert!(
            result
                .notes
                .iter()
                .any(|note| note.contains("没有推断出候选范围")),
            "空初稿必须给出下一步提示，实际：{:?}",
            result.notes
        );
        assert_eq!(result.confidence, 0, "没有草稿时置信度必须是 0");
        // A3：发现失败时「改用完整识别」是无效建议（同一份发现逻辑），不能出现在初稿说明里。
        assert!(
            !result
                .notes
                .iter()
                .any(|note| note.contains("改用完整识别")),
            "初稿说明不得建议「改用完整识别」，实际：{:?}",
            result.notes
        );
    }

    /// A3：判定「发现是否只找到了安装目录」只看根类型，不看路径 —— 安装目录本身也可能叫
    /// `saves` 之类，按路径名判会漏报。
    #[test]
    fn install_only_discovery_is_decided_by_root_type() {
        let root = |root_type: SaveRootType, path: &str| ScanRoot {
            root_type,
            physical_path: PathBuf::from(path),
        };

        assert!(discovered_only_the_install_dir(&[root(
            SaveRootType::ManagedGame,
            r"C:\Games\Demo\saves"
        )]));
        assert!(!discovered_only_the_install_dir(&[
            root(SaveRootType::ManagedGame, r"C:\Games\Demo"),
            root(SaveRootType::AppData, r"C:\Users\P\AppData\Roaming\Demo"),
        ]));
    }

    /// A3 的正题：发现只找到安装目录、又没有候选时，说明必须点出真正的病因（目录名不含游戏名），
    /// 并说清「改用完整识别也没用」—— 否则用户会去重试一次注定失败的完整识别，或者反过来怀疑
    /// 自己没保存成功。
    #[test]
    fn empty_drafts_explain_a_name_mismatch_instead_of_suggesting_a_retry() {
        let (root, _roots, snapshot) = preview_fixture(
            "a3-name-mismatch",
            &[("settings.ini", b"[cfg]"), ("readme.txt", b"hi")],
        );
        // 只有安装目录这一个根 —— 正是「提示词一处都没命中」的形态。
        let install_only = vec![ScanRoot {
            root_type: SaveRootType::ManagedGame,
            physical_path: root.to_path_buf(),
        }];

        let (_, drafts, notes) = infer_scope_drafts(
            &install_only,
            &snapshot,
            Some(&HashMap::new()),
            &HashSet::new(),
            &[],
        );

        assert!(
            drafts.is_empty(),
            "settings.ini / readme.txt 都不是存档，不该推出范围，实际 {} 个",
            drafts.len()
        );
        let joined = notes.join("\n");
        assert!(
            joined.contains("目录发现只找到了游戏安装目录"),
            "必须点出发现环节的问题，实际说明：{joined}"
        );
        assert!(
            joined.contains("同一份发现逻辑"),
            "必须说清「改用完整识别也没用」，实际说明：{joined}"
        );
        assert!(
            !joined.contains("请确认游戏内完成了一次保存"),
            "这是发现失败的场景，不该把用户引去怀疑自己没保存，实际说明：{joined}"
        );
    }

    /// 反过来：发现确实找到了安装目录之外的根时，就不能再提「目录名不含游戏名」——
    /// 否则每个「忘了保存」的用户都会被引到错误的方向上去。
    #[test]
    fn empty_drafts_stay_plain_when_discovery_found_more_than_the_install_dir() {
        let (_root, roots, snapshot) =
            preview_fixture("a3-found-a-root", &[("settings.ini", b"[cfg]")]);

        let (_, drafts, notes) = infer_scope_drafts(
            &roots, // preview_fixture 给的是 AppData 根，不是 ManagedGame
            &snapshot,
            Some(&HashMap::new()),
            &HashSet::new(),
            &[],
        );

        assert!(drafts.is_empty());
        let joined = notes.join("\n");
        assert!(
            !joined.contains("目录发现只找到了游戏安装目录"),
            "发现是成功的，不该说发现失败，实际说明：{joined}"
        );
        assert!(
            joined.contains("请确认游戏内完成了一次保存"),
            "应给出普通的「确认是否保存过」说明，实际说明：{joined}"
        );
    }

    /// R5 的闭合依据（前半）：ETW 路径走 `collect_targeted_snapshot`，**不套用** `collect_snapshot`
    /// 的资产目录剪枝。所以「存档住在 `content/` 下」在 ETW 可用时看得见 —— 一旦有人把两个快照
    /// 函数「统一」掉、顺手把这个剪枝也搬过去，R5 就会真的成立。钉住它。
    #[test]
    fn targeted_snapshot_keeps_files_under_asset_directories() {
        let (root, _roots, _) = preview_fixture("r5-asset-dir", &[("content/save.dat", b"save")]);
        let roots = vec![ScanRoot {
            root_type: SaveRootType::ManagedGame,
            physical_path: root.to_path_buf(),
        }];
        let etw_files: HashSet<String> = [normalize_path(&root.join("content").join("save.dat"))]
            .into_iter()
            .collect();

        let snapshot =
            collect_targeted_snapshot(&roots, &etw_files, &|| false).expect("收集 ETW 目标快照");

        assert_eq!(
            snapshot.len(),
            1,
            "ETW 定位到的文件必须原样进快照，不该被资产目录剪枝挡掉"
        );
    }

    /// R5 的成因（对照）：无 ETW 时走 `collect_snapshot`，它**确实**把资产目录整棵剪掉。
    /// 两条测试一起钉住，「什么条件下会漏」才是明确的 —— 只测一边容易让人误以为 R5 不存在。
    #[test]
    fn full_snapshot_prunes_files_under_asset_directories() {
        let (root, _roots, _) = preview_fixture("r5-asset-full", &[("content/save.dat", b"save")]);
        let roots = vec![ScanRoot {
            root_type: SaveRootType::ManagedGame,
            physical_path: root.to_path_buf(),
        }];

        let snapshot = collect_snapshot(&roots, |_, _| {}, &|| false).expect("收集完整快照");

        assert!(
            snapshot.is_empty(),
            "完整快照应剪掉 content/ 整个子树（R5 在无 ETW 时的成因），实际 {} 项",
            snapshot.len()
        );
    }

    /// R5 的另一半（资源扩展名一刀切）**不是** ETW 能绕过的：`is_etw_candidate` 与
    /// `is_save_candidate` 都在看名字线索**之前**就把 `RESOURCE_EXTENSIONS` 一票否决。
    /// 也就是说「存档就是一张 .png / 一段 .wav」这种形态，在两条路径下都看不见。
    ///
    /// 这是**刻意保留**的取舍：放开就会把贴图、音频、字体、dll 当成存档（误收比漏收更难收拾）。
    /// 代价是「以资源扩展名命名的存档」收不到 —— 真实数据里没观察到这种布局，故接受。
    #[test]
    fn resource_extensions_are_rejected_by_both_candidate_paths() {
        // 路径里带 `slot1`（命中名字线索）也照样被拒 —— 证明这是一票否决，不是「优先级低」。
        for path in [
            r"C:\Games\Demo\content\slot1.png",
            r"C:\Games\Demo\content\slot1.wav",
            r"C:\Games\Demo\content\slot1.ttf",
        ] {
            assert!(
                !is_etw_candidate(path),
                "资源扩展名必须被 ETW 候选拒掉：{path}"
            );
            assert!(
                !is_save_candidate(path),
                "资源扩展名必须被快照候选拒掉：{path}"
            );
        }
    }

    /// 临时基准：把候选优化各做成一份独立副本，同一进程交替测量（消除缓存漂移）。

    /// 临时基准：新实现 vs 旧算法，在**同一进程内交替测量**（消除缓存漂移），

    /// P3 的对照测量（默认 `#[ignore]`）：学习侧 `collect_snapshot` 对 ManagedGame 有
    /// `max_depth(4)` + 资产目录剪枝，应当明显便宜于仓储侧那条**无深度上限**的遍历。
    /// 两个数字放在一起，才能判断「学习一次要扫 3 遍」到底值不值得优化。
    ///
    /// 手动跑：`cargo test --lib measure_learning_snapshot -- --ignored --nocapture`
    #[test]
    #[ignore = "测量用，需手动触发"]
    fn measure_learning_snapshot_cost() {
        use std::time::Instant;

        let root = temp_root("measure-snap");
        let mut created = 0usize;
        for bucket in ["bin", "data", "assets", "save"] {
            for group in 0..40 {
                let dir = root.join(bucket).join(format!("g{group:02}"));
                fs::create_dir_all(&dir).expect("create dir");
                for file in 0..60 {
                    fs::write(dir.join(format!("f{file:03}.dat")), b"x").expect("write file");
                    created += 1;
                }
            }
        }
        let deep = root
            .join("data")
            .join("g00")
            .join("a")
            .join("b")
            .join("c")
            .join("d");
        fs::create_dir_all(&deep).expect("create deep dir");
        for file in 0..60 {
            fs::write(deep.join(format!("deep{file:03}.dat")), b"x").expect("write file");
            created += 1;
        }

        let roots = vec![ScanRoot {
            root_type: SaveRootType::ManagedGame,
            physical_path: root.to_path_buf(),
        }];
        let started = Instant::now();
        let snapshot = collect_snapshot(&roots, |_, _| {}, &|| false).expect("collect_snapshot");
        let elapsed = started.elapsed();
        println!(
            "\n[P3] collect_snapshot（ManagedGame，max_depth 4 + 剪枝）：夹具 {created} 个 → 收 {} 个 / {elapsed:?}",
            snapshot.len()
        );
    }

    /// 提议只该出现在「只认历史清单」的范围上：容器命名的范围本来就整目录收集
    /// （`include_directories = ["."]`），再列一遍提议是噪音。
    #[test]
    fn only_non_container_drafts_carry_proposals() {
        let root = temp_root("proposal-scope");
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
                physical_path: root.to_path_buf(),
            }],
            baseline: None,
            tracked_processes: Arc::new(Mutex::new(Default::default())),
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
            tracked_processes: Arc::new(Mutex::new(Default::default())),
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
            tracked_processes: Arc::new(Mutex::new(Default::default())),
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
            tracked_processes: Arc::new(Mutex::new(Default::default())),
            process_tracker_stop: Arc::new(AtomicBool::new(false)),
            process_tracker_done: Arc::new(AtomicBool::new(false)),
            etw_capture: None,
            etw_start_error: None,
            validation_only: false,
        };

        let real_save = r"c:\users\player\saved games\mygame\savedata\slot1\save.dat".to_string();
        let noisy_image =
            r"c:\users\player\saved games\mygame\savedata\slot1\avatar.png".to_string();
        let future_slot_preview =
            r"c:\users\player\saved games\mygame\savedata\slot2\preview.png".to_string();

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
        final_snapshot.insert(future_slot_preview, FileFingerprint { size: 512, modified_unix: 200 });

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
        assert!(scope.exclude_exact.iter().any(|rule| rule == "slot2/preview.png"));
        assert!(super::matching_exclusion("slot2/save.sav", scope).is_none());
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
            tracked_processes: Arc::new(Mutex::new(Default::default())),
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
            save_protection_skipped: false,
            last_played_at: None,
            latest_save_version_id: None,
            added_at: None,
            pending_body_upload: None,
        };

        let hints = super::game_name_hints(&game);
        assert!(hints.contains(&"黑神话".to_string()));
        assert!(hints.contains(&"悟空".to_string()));
        assert!(hints.contains(&"黑神话悟空".to_string()));
    }

    #[test]
    fn infer_scan_root_recognizes_redirected_documents() {
        let documents = PathBuf::from(r"F:\Sync\文档");
        let save = documents.join("my games").join("example").join("slot.sav");
        let root = super::infer_scan_root_for_etw_file(&save, None, Some(&documents), None).unwrap();
        assert_eq!(root.root_type, SaveRootType::Documents);
        assert_eq!(root.physical_path, documents.join("my games").join("example"));
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
        let root = super::infer_scan_root_for_etw_file(&local_low_file, Some(managed), None, None);
        assert!(root.is_some());
        let root = root.unwrap();
        assert_eq!(root.root_type, SaveRootType::LocalLow);
        assert!(normalize_path(&root.physical_path).contains("genshin"));

        // 2. Saved Games
        let saved_games_file = PathBuf::from(&profile)
            .join("Saved Games")
            .join("Hades")
            .join("Profile1.sav");
        let root = super::infer_scan_root_for_etw_file(&saved_games_file, Some(managed), None, None);
        assert!(root.is_some());
        let root = root.unwrap();
        assert_eq!(root.root_type, SaveRootType::SavedGames);
        assert!(normalize_path(&root.physical_path).contains("hades"));

        // 3. Managed Game
        let managed_file = managed.join("Game_Data").join("slot.bin");
        let root = super::infer_scan_root_for_etw_file(&managed_file, Some(managed), None, None);
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
        let root = super::infer_scan_root_for_etw_file(&steam_file, Some(managed), None, Some(&public_doc));
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
        let root = super::infer_scan_root_for_etw_file(&nested, None, None, None).expect("应回推出范围");
        assert_eq!(root.root_type, SaveRootType::ProgramData);
        assert_eq!(
            normalize_path(&root.physical_path),
            normalize_path(&program_data.join("Vendor").join("ClassicGame")),
            "应取厂商 + 游戏两级，而不是文件所在的 save 子目录"
        );

        // 少一段时只能取一级：把第二段（其实是文件名）也拼进去，范围根就成了文件路径。
        let shallow = program_data.join("ClassicGame").join("save.dat");
        let root = super::infer_scan_root_for_etw_file(&shallow, None, None, None).expect("应回推出范围");
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
            save_protection_skipped: false,
            last_played_at: None,
            latest_save_version_id: None,
            added_at: None,
            pending_body_upload: None,
        };

        let hints = super::game_name_hints(&game);
        assert!(!hints.contains(&"game".to_string()));
        assert!(hints.contains(&"rpg".to_string()));
    }

    /// 目录发现的排除规则（存档容器 / 噪声目录 / 通用词）必须仍然生效。
    ///
    /// 2026-09-13 把这三条从「逐 hint 重复判断」提到 `DirectoryHintMatcher::new` 之后重写：
    /// 原来这条测试直接调 `directory_matches_hint`，而那个函数已被删除（它的职责拆成了
    /// 「与 hint 无关的前置检查」+ 匹配器）。改成**走真实入口** `find_candidate_directories`
    /// 来钉住同样的行为 —— 钉在真实入口上比钉在内部函数上更难被重构绕过。
    #[test]
    fn find_candidate_directories_rejects_save_containers_noise_and_generic_names() {
        let root = temp_root("scan-exclude");
        for name in ["Saved Games", "temp", "SaveData", "Game", "DemonicMahjong"] {
            fs::create_dir_all(root.join(name)).expect("create dir");
        }
        // 提示词刻意选两个**非通用**词，且都不与 Game / temp / Saved Games / SaveData 沾边：
        // 这样「没被选中」只可能是排除规则生效，而不是碰巧没匹配上。
        // （不能用 "game"：它本身就在 GENERIC_NAME_BLACKLIST 里，会被当作通用词剔除。）
        let hints = vec!["demonic".to_string(), "mahjong".to_string()];
        let found = super::find_candidate_directories(&root, &hints);
        let names: Vec<String> = found
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        // 存档容器目录不是「某个游戏」的候选；噪声目录、通用词目录名同理。
        assert!(
            !names.iter().any(|n| n == "Saved Games"),
            "存档容器目录应被排除：{names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "temp"),
            "噪声目录应被排除：{names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "SaveData"),
            "存档容器目录应被排除：{names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "Game"),
            "通用词目录名应被排除：{names:?}"
        );
        assert!(
            names.iter().any(|n| n == "DemonicMahjong"),
            "命中提示词的真实游戏目录必须留下：{names:?}"
        );
    }

    /// 提前返回不能顺手跳过下探：被排除的目录**自身**不是候选，但它下面仍可能有候选。
    ///
    /// 这是本次重构最容易被写错的一处（把「不是候选」和「不下探」混为一谈），所以单独钉住。
    #[test]
    fn rejection_does_not_stop_descending_into_deeper_candidates() {
        let root = temp_root("scan-descend");
        // 中间层是通用词（因此不可能是候选），深层才是命中提示词的目录。
        fs::create_dir_all(root.join("Game").join("DemonicMahjong")).expect("create dirs");

        let hints = vec!["game".to_string(), "demonic".to_string()];
        let found = super::find_candidate_directories(&root, &hints);
        let paths: Vec<String> = found.iter().map(|p| p.display().to_string()).collect();

        assert_eq!(found.len(), 1, "应当只在深层命中一个：{paths:?}");
        assert!(
            found[0].ends_with(std::path::Path::new("Game").join("DemonicMahjong")),
            "命中的应当是深层目录本身：{paths:?}"
        );
    }

    #[test]
    fn preview_matches_exact_game_under_containers_but_not_similarly_named_games() {
        let workspace = temp_root("preview-exact-discovery");
        let root = workspace.join("locations");
        let under_saves = root.join("Saves/Studio/Engine/Profiles/Alpha");
        let exact_package = root.join("Packages/Vendor.Alpha_abc");
        for path in [
            under_saves.clone(),
            exact_package.clone(),
            root.join("AlphaBeta"),
            root.join("Vendor/OtherAlpha"),
            root.join("Packages/Vendor.AlphaBeta_abc/LocalState/Alpha"),
        ] {
            fs::create_dir_all(path).unwrap();
        }
        let game = crate::domain::Game::new_pending("Alpha", "managed", "Game.exe");
        let hints = super::preview_name_hints(&game);
        assert_eq!(hints, ["alpha"]);
        let mut coverage = super::ScanCoverage::default();
        let found = super::discover_named_directories(&root, &hints, true, &mut coverage)
            .into_iter()
            .collect::<HashSet<_>>();
        assert_eq!(found, HashSet::from([under_saves, exact_package]));
        assert_eq!(coverage.unreadable, 0);
    }

    #[test]
    fn preview_distinguishes_read_failure_and_depth_limit_from_no_matches() {
        let workspace = temp_root("preview-partial-coverage");
        let blocked = workspace.join("not-a-directory");
        fs::write(&blocked, b"file").unwrap();
        let mut coverage = super::ScanCoverage::default();
        let found = super::discover_named_directories(&blocked, &["alpha".into()], true, &mut coverage);
        assert!(found.is_empty());
        assert_eq!(coverage.unreadable, 1);
        let root = workspace.join("root");
        let deep = root.join("vendor/one/two/three/four/five/six/seven/eight");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("slot.sav"), b"save").unwrap();
        let roots = [ScanRoot { root_type: SaveRootType::Custom, physical_path: root }];
        let snapshot = super::collect_snapshot_with_coverage(
            &roots, |_, _| {}, &|| false, Some(&mut coverage),
        ).unwrap();
        assert!(snapshot.is_empty());
        assert!(coverage.limited);
        let result = preview_result(vec![], vec![], 1, &coverage);
        assert!(result.notes[0].contains("无法读取"));
        assert!(result.notes[0].contains("扫描深度或数量上限"));
        assert!(result.notes[0].contains("已检查的位置"));
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
        let root = super::infer_scan_root_for_etw_file(steam_file, None, None, None);
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
        let analysis = crate::services::learning::analyze_save_transactions_for(operations, is_etw_candidate);
        let (level, reason, confidence) = super::classify_scope_evidence(
            scope_root, &["slot.sav".to_string()], false, &analysis.completed_paths,
        );
        assert_eq!(level, SaveCandidateEvidenceLevel::Strong);
        assert_eq!(confidence, 92);
        assert!(reason.contains("重命名"));
        assert_eq!(super::classify_scope_evidence(
            scope_root, &["other.sav".to_string()], false, &analysis.completed_paths,
        ).0, SaveCandidateEvidenceLevel::Review);
    }
}
