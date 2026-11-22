use crate::domain::{
    learning::ProcessLifetime,
    timestamp::{current_filetime, FILETIME_TICKS_PER_MS, FILETIME_UNIX_EPOCH},
    EtwCaptureHandle,
};
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::Command;
use std::time::Instant;
use tauri::{AppHandle, Manager};

use super::{
    trace_evidence::{uses_file_object, FileEvidence, ProcessWindows},
    transactions::FileOperation,
};

const KERNEL_FILE_CAPTURE_KEYWORDS: u64 = 0x1EB0;

/// ETW 会话缓冲区大小（KB）。`logman` 官方示例用的就是 64。
const ETW_TRACE_BUFFER_SIZE_KB: u32 = 64;
/// ETW 会话缓冲区数量的下界 / 上界 —— 内存侧的上界（256 × 64KB ≈ 16MB）。
const ETW_TRACE_MIN_BUFFERS: u32 = 16;
const ETW_TRACE_MAX_BUFFERS: u32 = 256;
/// 单个 `.etl` 的磁盘上界（MB）。采集期原本没有任何上界，本机实测单个文件到过 422MB。
const ETW_TRACE_MAX_LOG_MB: u32 = 256;

/// 组装 `logman create trace` 的参数。
///
/// 补上三层资源上界：`-nb`/`-bs` 限住内存里的缓冲池，`-max` 限住磁盘上的日志文件。
///
/// ⚠️ **不要再加 `-b` 或 `-rf`**。评审文档曾建议用 `-b` 当缓冲参数、用 `-rf` 当会话时长
/// 上限，但在这台机器的 `logman`（10.0.26100.1150）上实测**两者都与 `-ets` 互斥**，报
/// 「参数"b"/"rf"不允许具有其他指定的参数」—— 加进去会让**整个会话创建失败**并静默回退到
/// 快照模式（存档识别质量随之下降，且不会有明显报错）：
///   - `-b` 是「在指定时间开始收集」的起始时刻，不是缓冲大小（缓冲大小是 `-bs`）；
///   - `-rf` 是「运行指定时长」，与 `-ets` 互斥 —— 所以时长上限只能放在 Rust 侧，
///     见 `save_learning_service` 里的采集看门狗。
///
/// 同理**不加 `-ct`**：它只影响时间戳时钟（省一点 CPU），却会改变 CSV 里 ClockTime 的
/// 量纲。生命周期过滤只接受 CSV 中可确认的 Windows FILETIME，不切换时钟类型。
fn etw_trace_args(trace_name: &str, etl_path: &str, providers_path: &str) -> Vec<String> {
    vec![
        "create".to_string(),
        "trace".to_string(),
        trace_name.to_string(),
        "-o".to_string(),
        etl_path.to_string(),
        "-pf".to_string(),
        providers_path.to_string(),
        "-bs".to_string(),
        ETW_TRACE_BUFFER_SIZE_KB.to_string(),
        "-nb".to_string(),
        ETW_TRACE_MIN_BUFFERS.to_string(),
        ETW_TRACE_MAX_BUFFERS.to_string(),
        "-max".to_string(),
        ETW_TRACE_MAX_LOG_MB.to_string(),
        "-ets".to_string(),
    ]
}

pub(crate) struct TraceCollectionResult {
    pub(crate) files: HashSet<String>,
    pub(crate) logs: Vec<String>,
    pub(crate) operations: Vec<FileOperation>,
    pub(crate) incomplete: bool,
}

#[cfg(target_os = "windows")]
pub(crate) fn extend_tracked_process_tree(tracked: &mut HashSet<u32>) -> Result<(), String> {
    let parents = crate::services::process_service::process_parent_snapshot()?;

    loop {
        let descendants = parents
            .iter()
            .filter_map(|(pid, parent_pid)| tracked.contains(parent_pid).then_some(*pid))
            .collect::<Vec<_>>();
        let previous_len = tracked.len();
        tracked.extend(descendants);
        if tracked.len() == previous_len {
            return Ok(());
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn extend_tracked_process_tree(_tracked: &mut HashSet<u32>) -> Result<(), String> {
    Ok(())
}

pub(crate) fn try_start_etw_capture(
    app: &AppHandle,
    session_id: &str,
) -> Result<EtwCaptureHandle, String> {
    if !is_running_as_admin() {
        return Err("权限不足：当前进程不是管理员，已回退快照模式".to_string());
    }

    let trace_name = format!(
        "GameSaverTrace_{}",
        session_id
            .replace('-', "")
            .chars()
            .take(10)
            .collect::<String>()
    );
    let etl_path = event_logs_dir(app)?.join(format!("{trace_name}.etl"));
    let etl_path_str = etl_path.to_string_lossy().to_string();
    let providers_path = etl_path.with_extension("providers.txt");
    std::fs::write(&providers_path, format!(
        "Microsoft-Windows-Kernel-File 0x{KERNEL_FILE_CAPTURE_KEYWORDS:X} 4\r\nMicrosoft-Windows-Kernel-Process 0x10 4\r\n",
    )).map_err(|error| format!("无法写入 ETW 采集配置：{error}"))?;

    let _ = {
        let mut command = Command::new("logman");
        command.args(["stop", &trace_name, "-ets"]);
        apply_background_process_flags(&mut command).output()
    };

    let created = {
        let mut command = Command::new("logman");
        command.args(etw_trace_args(&trace_name, &etl_path_str, &providers_path.to_string_lossy()));
        apply_background_process_flags(&mut command).output()
    };
    let _ = std::fs::remove_file(&providers_path);
    let created = created.map_err(|err| format!("ETW 启动失败：无法执行 logman（{err}）"))?;
    if !created.status.success() {
        let stderr = String::from_utf8_lossy(&created.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&created.stdout).trim().to_string();
        let msg = if !stderr.is_empty() { stderr } else { stdout };
        return Err(format!(
            "ETW 启动失败：logman 返回 {}",
            command_failure_detail(&created, &msg)
        ));
    }

    Ok(EtwCaptureHandle {
        trace_name,
        etl_path,
        timed_out: Default::default(),
    })
}

pub(crate) fn stop_etw_capture(trace_name: &str) -> Result<(), String> {
    if trace_name.trim().is_empty() {
        return Ok(());
    }
    let stopped = {
        let mut command = Command::new("logman");
        command.args(["stop", trace_name, "-ets"]);
        apply_background_process_flags(&mut command)
            .output()
            .map_err(|err| format!("ETW 停止失败：无法执行 logman（{err}）"))?
    };
    if stopped.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&stopped.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&stopped.stdout).trim().to_string();
    let message = if !stderr.is_empty() { stderr } else { stdout };
    Err(format!(
        "ETW 停止失败：logman 返回 {}",
        command_failure_detail(&stopped, &message)
    ))
}

pub(crate) fn cleanup_stale_captures(app: &AppHandle) {
    #[cfg(target_os = "windows")]
    {
        let mut query = Command::new("logman");
        query.args(["query", "-ets"]);
        if let Ok(output) = apply_background_process_flags(&mut query).output() {
            let text = format!(
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            for trace_name in text
                .lines()
                .map(str::trim)
                .filter(|line| line.starts_with("GameSaverTrace_"))
            {
                let mut stop = Command::new("logman");
                stop.args(["stop", trace_name, "-ets"]);
                let _ = apply_background_process_flags(&mut stop).output();
            }
        }
    }
    if let Ok(directory) = event_logs_dir(app) {
        if let Ok(entries) = std::fs::read_dir(directory) {
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                let is_trace = is_trace_artifact(&path);
                let old_enough = entry
                    .metadata()
                    .ok()
                    .and_then(|metadata| metadata.modified().ok())
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age > std::time::Duration::from_secs(60 * 60));
                if is_trace && old_enough {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }
}

pub(crate) fn is_trace_artifact(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("csv") || ext.eq_ignore_ascii_case("etl"))
}

#[cfg(target_os = "windows")]
pub(crate) fn is_running_as_admin() -> bool {
    use std::ptr::null_mut;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY},
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }

        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut returned_length = 0;
        let result = GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned_length,
        ) != 0;
        CloseHandle(token);
        result
            && returned_length >= std::mem::size_of::<TOKEN_ELEVATION>() as u32
            && elevation.TokenIsElevated != 0
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn is_running_as_admin() -> bool {
    false
}

pub(crate) fn collect_related_files_by_trace(
    capture: &EtwCaptureHandle,
    process_lifetimes: impl FnOnce(i64) -> Vec<ProcessLifetime>,
) -> Result<TraceCollectionResult, String> {
    let collection_started = Instant::now();
    let mut logs = Vec::new();
    let name = &capture.trace_name;
    let etl_path = capture.etl_path.to_string_lossy();
    let mut incomplete = false;

    let stop_started = Instant::now();
    match stop_etw_capture(name) {
        Ok(()) => logs.push(format!("已停止 trace：{name}")),
        Err(err) => {
            incomplete = true;
            logs.push(format!("停止 trace 返回警告：{err}"));
        }
    }
    logs.push(format!(
        "性能：停止 ETW {} ms",
        stop_started.elapsed().as_millis()
    ));
    logs.push(format!("ETL 文件：{etl_path}"));
    if capture.timed_out.load(std::sync::atomic::Ordering::Acquire) {
        incomplete = true;
        logs.push("ETW 采集已达到时长上限，之后的写入未记录。".to_string());
    }
    if std::fs::metadata(&capture.etl_path).is_ok_and(|metadata| {
        let max_bytes = u64::from(ETW_TRACE_MAX_LOG_MB) * 1024 * 1024;
        metadata.len() >= max_bytes - u64::from(ETW_TRACE_BUFFER_SIZE_KB) * 1024
    }) {
        incomplete = true;
        logs.push("ETL 达到日志大小上限，采集可能截断。".to_string());
    }
    let capture_end = current_filetime().ok_or("无法确定 ETW 采集结束时间")?;
    let lifetimes = process_lifetimes(capture_end);
    if lifetimes.is_empty() {
        return Err("无法确认游戏进程生命周期，未使用未验证的 ETW 事件".to_string());
    }
    let tracked = match super::native_etw::collect_process_windows(&etl_path, &lifetimes, capture_end) {
        Ok((tracked, partial, note)) => {
            incomplete |= partial;
            logs.push(note);
            tracked
        }
        Err(error) => {
            incomplete = true;
            logs.push(format!("无法读取 ETW 进程启停事件，仅使用已追踪的进程：{error}"));
            ProcessWindows::from_tracked(&lifetimes)
        }
    };
    let mut tracked_pids: Vec<_> = tracked.pids().collect();
    tracked_pids.sort_unstable();

    match super::native_etw::collect_related_files_from_etl(&etl_path, &tracked) {
        Ok(mut native) => {
            logs.append(&mut native.logs);
            logs.push(format!(
                "性能：ETW 结果收集总计 {} ms",
                collection_started.elapsed().as_millis()
            ));
            return Ok(TraceCollectionResult {
                files: native.files,
                logs,
                operations: native.operations,
                incomplete: incomplete || native.incomplete,
            });
        }
        Err(err) => logs.push(format!("原生 ETL 解析不可用，已回退 tracerpt CSV：{err}")),
    }

    let csv_path = format!("{etl_path}.csv");
    let csv_cleanup = CsvCleanup(&csv_path);
    let convert_started = Instant::now();
    let converted = {
        let mut command = Command::new("tracerpt");
        command.args([etl_path.as_ref(), "-of", "CSV", "-o", &csv_path, "-y"]);
        apply_background_process_flags(&mut command)
            .output()
            .map_err(|err| format!("parse etw failed: {err}"))?
    };
    if !converted.status.success() {
        let stderr = String::from_utf8_lossy(&converted.stderr)
            .trim()
            .to_string();
        let stdout = String::from_utf8_lossy(&converted.stdout)
            .trim()
            .to_string();
        let msg = if !stderr.is_empty() { stderr } else { stdout };
        return Err(format!("parse etw failed: {msg}"));
    }
    logs.push(format!(
        "性能：tracerpt 转换 {} ms",
        convert_started.elapsed().as_millis()
    ));
    logs.push("tracerpt 已将 ETL 转换为 CSV".to_string());

    let started = Instant::now();
    let mut reader = BufReader::new(
        File::open(&csv_path).map_err(|error| format!("读取 ETW CSV 失败：{error}"))?,
    );
    let mut line_buffer = Vec::new();
    reader.read_until(b'\n', &mut line_buffer)
        .map_err(|error| format!("读取 ETW CSV 表头失败：{error}"))?;
    let header_line = String::from_utf8_lossy(&line_buffer);
    let headers = parse_csv_line(header_line.trim_start_matches('\u{feff}').trim());
    let pid_idx = find_header_index(&headers, &["processid", "process id", "pid"])
        .ok_or("ETW CSV 缺少 PID 字段")?;
    let task_idx = find_header_index(&headers, &["task"]).ok_or("ETW CSV 缺少 Task 字段")?;
    let clock_time_idx = find_header_index(&headers, &["clock-time", "clock time", "timestamp"]);
    let user_data_idx = find_header_index(&headers, &["user data", "userdata"]);
    let path_idx = find_header_index(&headers, &["filename", "file name", "filepath", "file path", "pathname", "path"]);
    let device_paths = build_device_path_map();
    let mut evidence = FileEvidence::default();
    let mut total_rows = 0usize;
    let mut matched_rows = 0usize;
    let mut outside_lifetime = 0usize;
    loop {
        line_buffer.clear();
        if reader.read_until(b'\n', &mut line_buffer)
            .map_err(|error| format!("读取 ETW CSV 事件失败：{error}"))? == 0 {
            break;
        }
        total_rows += 1;
        let line = String::from_utf8_lossy(&line_buffer);
        let line = line.trim();
        if !fast_csv_field(line, 0).is_some_and(|name| name.eq_ignore_ascii_case("Microsoft-Windows-Kernel-File")) {
            continue;
        }
        let Some(pid) = fast_csv_field(line, pid_idx.saturating_sub(1)).and_then(parse_u32) else {
            continue;
        };
        if !tracked.contains_pid(pid) {
            continue;
        }
        let Some(code) = fast_csv_field(line, task_idx.saturating_sub(1)).and_then(parse_u32) else {
            continue;
        };
        if !uses_file_object(code) {
            continue;
        }
        let Some(event_time) = csv_event_time(line, clock_time_idx, true)
            .filter(|time| tracked.contains(pid, *time))
        else {
            outside_lifetime += 1;
            continue;
        };
        matched_rows += 1;
        let row = parse_csv_line(line);
        let path = if super::trace_evidence::reads_direct_path(code) {
            path_idx.and_then(|idx| event_field_value(&row, idx)).and_then(|cell| extract_trace_path(cell))
                .or_else(|| row.iter().skip(user_data_idx.map_or(0, |idx| idx.saturating_sub(1)))
                    .find_map(|cell| extract_trace_path(cell)))
                .map(|path| normalize_windows_path(&resolve_device_path(&path, &device_paths)))
        } else {
            None
        };
        evidence.record(code, path, event_file_object_id(&row, user_data_idx), pid,
            Some((event_time - FILETIME_UNIX_EPOCH) / FILETIME_TICKS_PER_MS));
    }
    logs.push(format!(
        "CSV 事件统计：总行 {total_rows}，匹配 {matched_rows}，生命周期不匹配或时间不可用 {outside_lifetime}，有效写入 {}，文件 {}",
        evidence.mutations, evidence.files.len(),
    ));
    logs.push(format!("CSV 结构化操作：{}，解析耗时 {} ms", evidence.operations.len(), started.elapsed().as_millis()));
    logs.push(format!("性能：ETW 结果收集总计 {} ms", collection_started.elapsed().as_millis()));
    drop(reader);
    drop(csv_cleanup);
    Ok(TraceCollectionResult {
        files: evidence.files,
        logs,
        operations: evidence.operations,
        incomplete: true,
    })
}

struct CsvCleanup<'a>(&'a str);

impl Drop for CsvCleanup<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0);
    }
}

fn csv_event_time(
    row: &str,
    clock_index: Option<usize>,
    is_kernel_file: bool,
) -> Option<i64> {
    let index = clock_index?.checked_sub(usize::from(is_kernel_file))?;
    let ticks = fast_csv_field(row, index)?.parse::<i64>().ok()?;
    (ticks >= FILETIME_UNIX_EPOCH).then_some(ticks)
}

#[cfg(test)]
fn event_pid_value(row: &[String], header_pid_index: usize) -> Option<&String> {
    let is_kernel_file = row.first().is_some_and(|value| {
        value
            .trim()
            .eq_ignore_ascii_case("Microsoft-Windows-Kernel-File")
    });
    let actual_index = if is_kernel_file {
        header_pid_index.saturating_sub(1)
    } else {
        header_pid_index
    };
    row.get(actual_index)
}

fn event_field_value(row: &[String], header_index: usize) -> Option<&String> {
    let is_kernel_file = row.first().is_some_and(|value| {
        value
            .trim()
            .eq_ignore_ascii_case("Microsoft-Windows-Kernel-File")
    });
    let actual_index = if is_kernel_file {
        header_index.saturating_sub(1)
    } else {
        header_index
    };
    row.get(actual_index)
}

fn event_file_object_id(row: &[String], user_data_index: Option<usize>) -> Option<String> {
    let start = user_data_index
        .map(|idx| idx.saturating_sub(1))
        .unwrap_or(row.len());
    row.iter()
        .skip(start)
        .filter_map(|value| {
            let trimmed = value.trim();
            (trimmed.starts_with("0xFFFF") && trimmed.len() >= 14)
                .then(|| trimmed.to_ascii_uppercase())
        })
        .nth(1)
}

fn extract_trace_path(text: &str) -> Option<String> {
    let chars = text.chars().collect::<Vec<_>>();
    for i in 0..chars.len().saturating_sub(2) {
        if chars[i].is_ascii_alphabetic() && chars[i + 1] == ':' && chars[i + 2] == '\\' {
            let mut end = i + 3;
            while end < chars.len() {
                let c = chars[end];
                if c == '"' || c == ',' {
                    break;
                }
                end += 1;
            }
            let path = chars[i..end].iter().collect::<String>();
            if path.len() > 4 {
                return Some(path);
            }
        }
    }
    if let Some(start) = text.find("\\Device\\") {
        let path = text[start..]
            .trim_end_matches('"')
            .trim_end_matches(',')
            .to_string();
        if path.len() > "\\Device\\".len() {
            return Some(path);
        }
    }
    None
}

#[cfg(target_os = "windows")]
pub(super) fn build_device_path_map() -> Vec<(String, String)> {
    use windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW;

    let mut output = Vec::new();
    for letter in b'A'..=b'Z' {
        let drive = format!("{}:", letter as char);
        let mut drive_wide = drive.encode_utf16().collect::<Vec<_>>();
        drive_wide.push(0);
        let mut buffer = vec![0u16; 1024];
        let length = unsafe {
            QueryDosDeviceW(
                drive_wide.as_ptr(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
            )
        };
        if length == 0 {
            continue;
        }
        let end = buffer
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(length as usize);
        let device = String::from_utf16_lossy(&buffer[..end]);
        if !device.is_empty() {
            output.push((device, drive));
        }
    }
    output.sort_by(|left, right| right.0.len().cmp(&left.0.len()));
    output
}

#[cfg(not(target_os = "windows"))]
fn build_device_path_map() -> Vec<(String, String)> {
    Vec::new()
}

pub(super) fn resolve_device_path(path: &str, device_paths: &[(String, String)]) -> String {
    let path_lower = path.to_ascii_lowercase();
    for (device, drive) in device_paths {
        if path_lower.starts_with(&device.to_ascii_lowercase()) {
            return format!("{drive}{}", &path[device.len()..]);
        }
    }
    path.to_string()
}

fn parse_csv_line(line: &str) -> Vec<String> {
    let mut output = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                if in_quotes && chars.peek().is_some_and(|next| *next == '"') {
                    current.push('"');
                    let _ = chars.next();
                } else {
                    in_quotes = !in_quotes;
                }
            }
            ',' if !in_quotes => {
                output.push(current.trim().trim_matches('"').to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    output.push(current.trim().trim_matches('"').to_string());
    output
}

fn fast_csv_field(line: &str, index: usize) -> Option<&str> {
    line.split(',')
        .nth(index)
        .map(|value| value.trim().trim_matches('"'))
}

fn find_header_index(headers: &[String], keywords: &[&str]) -> Option<usize> {
    headers.iter().position(|header| {
        let normalized = header
            .to_ascii_lowercase()
            .replace(' ', "")
            .replace('_', "");
        keywords.iter().any(|keyword| {
            let key = keyword
                .to_ascii_lowercase()
                .replace(' ', "")
                .replace('_', "");
            normalized == key || normalized.contains(&key)
        })
    })
}

fn parse_u32(text: &str) -> Option<u32> {
    let trimmed = text.trim().trim_matches('"');
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        return u32::from_str_radix(hex, 16).ok();
    }
    trimmed.parse::<u32>().ok()
}

pub(crate) fn normalize_windows_path(path: &str) -> String {
    path.replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

fn event_logs_dir(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let directory = app
        .path()
        .app_data_dir()
        .map_err(|err| format!("resolve event directory failed: {err}"))?
        .join("events");
    std::fs::create_dir_all(&directory)
        .map_err(|err| format!("create event directory failed: {err}"))?;
    Ok(directory)
}

fn apply_background_process_flags(command: &mut Command) -> &mut Command {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    command
}

fn command_failure_detail(output: &std::process::Output, message: &str) -> String {
    if output.status.code() == Some(5) {
        return "权限不足（错误 5）".to_string();
    }
    if message.trim().is_empty() {
        return format!("进程退出码 {:?}", output.status.code());
    }
    message.to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        etw_trace_args, event_field_value, event_file_object_id,
        event_pid_value, fast_csv_field, is_trace_artifact, parse_csv_line,
        csv_event_time, parse_u32, resolve_device_path, uses_file_object, ProcessLifetime,
        KERNEL_FILE_CAPTURE_KEYWORDS,
    };
    use super::super::trace_evidence::classify_kernel_operation;
    use crate::services::learning::transactions::FileOperationKind;
    use std::path::Path;

    #[test]
    fn parses_decimal_and_hexadecimal_process_ids() {
        assert_eq!(parse_u32("1234"), Some(1234));
        assert_eq!(parse_u32("0x000004D2"), Some(1234));
        assert_eq!(parse_u32("invalid"), None);
    }

    #[test]
    fn parses_quoted_csv_fields() {
        assert_eq!(
            parse_csv_line(r#""1234","C:\\Games\\Save, Slot.sav","Write""#),
            vec!["1234", r#"C:\\Games\\Save, Slot.sav"#, "Write"]
        );
    }

    #[test]
    fn reads_pid_field_without_parsing_entire_row() {
        let row = "Microsoft-Windows-Kernel-File,12,1,16,4,0,16,0x220,0x0000755C,0x00007630,2";
        assert_eq!(fast_csv_field(row, 8), Some("0x0000755C"));
    }

    #[test]
    fn prefilters_unneeded_kernel_tasks() {
        for task in [10, 11, 15, 17, 19, 20, 21, 22, 23, 24, 25, 28, 29, 32] {
            assert!(!uses_file_object(task));
        }
        for task in [12, 13, 14, 16, 18, 26, 27, 30] {
            assert!(uses_file_object(task));
        }
    }

    #[test]
    fn reads_kernel_file_pid_from_shifted_column() {
        let row = parse_csv_line(
            "Microsoft-Windows-Kernel-File,12,1,16,4,0,12,0xA0,0x0000755C,0x00007630,2",
        );
        assert_eq!(
            event_pid_value(&row, 9).and_then(|value| parse_u32(value)),
            Some(30044)
        );
    }

    #[test]
    fn reads_kernel_file_task_from_shifted_column() {
        let row = parse_csv_line(
            "Microsoft-Windows-Kernel-File,12,1,16,4,0,16,0x220,0x0000755C,0x00007630,2",
        );
        assert_eq!(event_field_value(&row, 7).map(String::as_str), Some("16"));
        assert!(matches!(
            event_field_value(&row, 7).and_then(|task| parse_u32(task)).and_then(classify_kernel_operation),
            Some(FileOperationKind::Write)
        ));
    }

    #[test]
    fn ignores_known_read_only_kernel_file_tasks() {
        for task in [10, 11, 12, 15, 20, 21, 22, 23, 24, 25] {
            assert!(classify_kernel_operation(task).is_none());
        }
    }

    #[test]
    fn capture_keywords_exclude_read_and_operation_end() {
        assert_eq!(KERNEL_FILE_CAPTURE_KEYWORDS & 0x100, 0);
        assert_eq!(KERNEL_FILE_CAPTURE_KEYWORDS & 0x40, 0);
        assert_ne!(KERNEL_FILE_CAPTURE_KEYWORDS & 0x20, 0);
        assert_ne!(KERNEL_FILE_CAPTURE_KEYWORDS & 0x200, 0);
        assert_ne!(KERNEL_FILE_CAPTURE_KEYWORDS & 0x400, 0);
        assert_ne!(KERNEL_FILE_CAPTURE_KEYWORDS & 0x800, 0);
        assert_ne!(KERNEL_FILE_CAPTURE_KEYWORDS & 0x1000, 0);
    }

    /// 取 `flag` 紧跟其后的那个值。
    fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        let index = args.iter().position(|arg| arg == flag)?;
        args.get(index + 1).map(String::as_str)
    }

    #[test]
    fn etw_trace_args_bound_the_capture_resources() {
        let args = etw_trace_args("GameSaverTrace_abc", r"C:\logs\abc.etl", "providers.txt");
        assert_eq!(flag_value(&args, "-pf"), Some("providers.txt"));
        assert_eq!(flag_value(&args, "-bs"), Some("64"), "缓冲区大小");
        assert_eq!(flag_value(&args, "-nb"), Some("16"), "缓冲区数量下界");
        // `-nb` 吃「最小值 最大值」两个值，上界是紧跟其后的第二个。
        let nb_index = args
            .iter()
            .position(|arg| arg == "-nb")
            .expect("-nb 必须存在");
        assert_eq!(args.get(nb_index + 2).map(String::as_str), Some("256"));
        assert_eq!(flag_value(&args, "-max"), Some("256"), "日志文件大小上界");
        // 会话形态不能变：`-ets` 表示立即开始且不落成数据收集器集，停止路径依赖它。
        assert_eq!(args.first().map(String::as_str), Some("create"));
        assert_eq!(args.get(1).map(String::as_str), Some("trace"));
        assert_eq!(args.last().map(String::as_str), Some("-ets"));
    }

    #[test]
    fn etw_trace_args_avoid_flags_that_conflict_with_ets() {
        let args = etw_trace_args("GameSaverTrace_abc", r"C:\logs\abc.etl", "providers.txt");
        // 本机 logman（10.0.26100.1150）实测：`-b`（起始时刻）与 `-rf`（运行时长）都与
        // `-ets` 互斥，报「参数不允许具有其他指定的参数」。一旦出现，整个会话创建会失败，
        // 且只会静默回退到快照模式 —— 用户看不到明显报错，识别质量却掉了。`-ct` 虽能用，
        // 但会改变 ClockTime 的量纲，而时间戳是按数值大小猜格式解析的，故一并禁止。
        for forbidden in ["-b", "-rf", "-ct"] {
            assert!(
                !args.iter().any(|arg| arg == forbidden),
                "{forbidden} 与 -ets 冲突或会改变时间戳量纲，不得出现在采集参数里"
            );
        }
    }

    #[test]
    fn csv_filters_reused_pids_and_unverifiable_timestamps() {
        let started_at = 134_313_202_837_027_063;
        let ended_at = started_at + 2;
        let lifetime = ProcessLifetime { pid: 42, started_at, ended_at };
        for (time, accepted) in [
            (started_at - 1, false),
            (started_at, true),
            (ended_at - 1, true),
            (ended_at, false),
            (ended_at + 1, false),
            (1_786_846_683_702, false),
        ] {
            let row = format!("Microsoft-Windows-Kernel-File,{time}");
            assert_eq!(
                csv_event_time(&row, Some(2), true).filter(|ticks| lifetime.contains(*ticks)),
                accepted.then_some(time)
            );
        }
        assert_eq!(
            csv_event_time("Microsoft-Windows-Kernel-File,", Some(2), true),
            None
        );
    }

    #[test]
    fn resolves_nt_device_paths_to_drive_paths() {
        let mappings = vec![("\\Device\\HarddiskVolume3".to_string(), "C:".to_string())];
        assert_eq!(
            resolve_device_path("\\Device\\HarddiskVolume3\\Games\\save.sav", &mappings),
            "C:\\Games\\save.sav"
        );
    }

    #[test]
    fn selects_second_kernel_pointer_as_file_object() {
        let row = parse_csv_line(
            "Provider,16,1,16,4,0,16,0x220,0x5258,0x990,0,,,,,,,clock,0,0,0x3BBBF,0xFFFF000000000001,0xFFFF000000000002,0xFFFF000000000003",
        );
        assert_eq!(
            event_file_object_id(&row, Some(19)).as_deref(),
            Some("0XFFFF000000000002")
        );
    }

    #[test]
    fn recognizes_trace_artifacts_for_cleanup() {
        assert!(is_trace_artifact(Path::new("C:/appdata/events/trace.etl")));
        assert!(is_trace_artifact(Path::new("C:/appdata/events/trace.ETL")));
        assert!(is_trace_artifact(Path::new(
            "C:/appdata/events/trace.etl.csv"
        )));
        assert!(is_trace_artifact(Path::new("C:/appdata/events/trace.CSV")));
        assert!(!is_trace_artifact(Path::new(
            "C:/appdata/events/store.json"
        )));
        assert!(!is_trace_artifact(Path::new("C:/appdata/events/game.exe")));
    }
}
