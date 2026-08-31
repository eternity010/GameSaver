use crate::domain::EtwCaptureHandle;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::Command;
use std::time::Instant;
use tauri::{AppHandle, Manager};

use super::transactions::{FileOperation, FileOperationKind};

const KERNEL_FILE_CAPTURE_KEYWORDS: u64 = 0x1EB0;

pub(crate) struct TraceCollectionResult {
    pub(crate) files: HashSet<String>,
    pub(crate) logs: Vec<String>,
    pub(crate) operations: Vec<FileOperation>,
}

#[cfg(target_os = "windows")]
pub(crate) fn extend_tracked_process_tree(tracked: &mut HashSet<u32>) -> Result<(), String> {
    use std::collections::HashMap;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        },
    };

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err("create process snapshot failed".to_string());
    }
    let mut parents = HashMap::new();
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    let mut has_entry = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while has_entry {
        parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
        has_entry = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe {
        CloseHandle(snapshot);
    }

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

    let _ = {
        let mut command = Command::new("logman");
        command.args(["stop", &trace_name, "-ets"]);
        apply_background_process_flags(&mut command).output()
    };

    let created = {
        let keyword_mask = format!("0x{KERNEL_FILE_CAPTURE_KEYWORDS:X}");
        let mut command = Command::new("logman");
        command.args([
            "create",
            "trace",
            &trace_name,
            "-o",
            &etl_path_str,
            "-p",
            "Microsoft-Windows-Kernel-File",
            &keyword_mask,
            "4",
            "-ets",
        ]);
        apply_background_process_flags(&mut command)
            .output()
            .map_err(|err| format!("ETW 启动失败：无法执行 logman（{err}）"))?
    };
    if !created.status.success() {
        let stderr = String::from_utf8_lossy(&created.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&created.stdout).trim().to_string();
        let msg = if !stderr.is_empty() { stderr } else { stdout };
        return Err(format!("ETW 启动失败：logman 返回 {}", command_failure_detail(&created, &msg)));
    }

    Ok(EtwCaptureHandle {
        trace_name,
        etl_path,
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
    Err(format!("ETW 停止失败：logman 返回 {}", command_failure_detail(&stopped, &message)))
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
            for trace_name in text.lines().map(str::trim).filter(|line| line.starts_with("GameSaverTrace_")) {
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
                    .is_some_and(|age| age > std::time::Duration::from_secs(24 * 60 * 60));
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

pub(crate) fn is_running_as_admin() -> bool {
    let mut command = Command::new("powershell");
    command.args([
        "-NoProfile",
        "-Command",
        "([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)",
    ]);
    let output = apply_background_process_flags(&mut command).output();
    let Ok(out) = output else {
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout)
        .trim()
        .to_ascii_lowercase();
    text.contains("true")
}

pub(crate) fn collect_related_files_by_trace(
    trace_name: Option<&str>,
    trace_path: Option<&str>,
    tracked_pids: &[u32],
) -> Result<TraceCollectionResult, String> {
    let collection_started = Instant::now();
    let mut files = HashSet::new();
    let mut logs = Vec::new();
    let Some(name) = trace_name else {
        logs.push("缺少 trace 名称，无法读取 ETW 结果".to_string());
        return Ok(TraceCollectionResult {
            files,
            logs,
            operations: Vec::new(),
        });
    };
    let Some(etl_path) = trace_path else {
        logs.push("缺少 ETL 路径，无法读取 ETW 结果".to_string());
        return Ok(TraceCollectionResult {
            files,
            logs,
            operations: Vec::new(),
        });
    };

    let stop_started = Instant::now();
    match stop_etw_capture(name) {
        Ok(()) => logs.push(format!("已停止 trace：{name}")),
        Err(err) => logs.push(format!("停止 trace 返回警告：{err}")),
    }
    logs.push(format!(
        "性能：停止 ETW {} ms",
        stop_started.elapsed().as_millis()
    ));
    logs.push(format!("ETL 文件：{etl_path}"));

    match super::native_etw::collect_related_files_from_etl(etl_path, tracked_pids) {
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
            });
        }
        Err(err) => logs.push(format!("原生 ETL 解析不可用，已回退 tracerpt CSV：{err}")),
    }

    let csv_path = format!("{etl_path}.csv");
    let convert_started = Instant::now();
    let converted = {
        let mut command = Command::new("tracerpt");
        command.args([etl_path, "-of", "CSV", "-o", &csv_path, "-y"]);
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

    let open_started = Instant::now();
    let csv_file = File::open(&csv_path).map_err(|err| format!("open etw csv failed: {err}"))?;
    let csv_size = csv_file
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or_default();
    logs.push(format!("CSV 文件：{csv_path}（{csv_size} 字节）"));
    let mut reader = BufReader::new(csv_file);
    let mut line_buffer = Vec::new();
    let header_bytes = reader
        .read_until(b'\n', &mut line_buffer)
        .map_err(|err| format!("read etw csv header failed: {err}"))?;
    logs.push(format!(
        "性能：打开 CSV 并读取表头 {} ms",
        open_started.elapsed().as_millis()
    ));
    if header_bytes == 0 {
        logs.push("CSV 为空，没有事件表头".to_string());
        return Ok(TraceCollectionResult {
            files,
            logs,
            operations: Vec::new(),
        });
    }
    let header_line = String::from_utf8_lossy(&line_buffer);
    let headers = parse_csv_line(header_line.trim_start_matches('\u{feff}').trim());
    let pid_idx = find_header_index(&headers, &["processid", "process id", "pid"]);
    let event_name_idx = find_header_index(&headers, &["event name", "eventname", "opcode name"]);
    let opcode_idx = find_header_index(&headers, &["opcode"]);
    let task_idx = find_header_index(&headers, &["task"]);
    let keyword_idx = find_header_index(&headers, &["keyword"]);
    let clock_time_idx =
        find_header_index(&headers, &["clock-time", "clock time", "timestamp", "time"]);
    let user_data_idx = find_header_index(&headers, &["user data", "userdata"]);
    let path_idx = find_header_index(
        &headers,
        &[
            "filename",
            "file name",
            "filepath",
            "file path",
            "pathname",
            "path",
        ],
    );
    logs.push(format!("CSV 表头：{}", headers.join(" | ")));
    logs.push(format!(
        "字段定位：PID={}，Task={}，Keyword={}，UserData={}，路径={}",
        pid_idx.map_or_else(|| "未找到".to_string(), |idx| idx.to_string()),
        task_idx.map_or_else(|| "未找到".to_string(), |idx| idx.to_string()),
        keyword_idx.map_or_else(|| "未找到".to_string(), |idx| idx.to_string()),
        user_data_idx.map_or_else(|| "未找到".to_string(), |idx| idx.to_string()),
        path_idx.map_or_else(|| "回退扫描整行".to_string(), |idx| idx.to_string())
    ));
    let pid_set = tracked_pids.iter().copied().collect::<HashSet<u32>>();
    let device_paths = build_device_path_map();
    logs.push(format!("设备路径映射：{} 个卷", device_paths.len()));
    logs.push(format!("目标 PID：{:?}", tracked_pids));
    let mut total_rows = 0usize;
    let mut parsed_pid_rows = 0usize;
    let mut matched_pid_rows = 0usize;
    let mut task_prefiltered_rows = 0usize;
    let mut write_like_rows = 0usize;
    let mut extracted_path_rows = 0usize;
    let mut ignored_path_rows = 0usize;
    let mut object_path_resolved_rows = 0usize;
    let mut observed_pids = HashSet::new();
    let mut matched_row_samples = Vec::new();
    let mut unparsed_pid_samples = Vec::new();
    let mut file_object_paths = HashMap::new();
    let mut written_file_objects = HashSet::new();
    let mut operations = Vec::new();
    let parse_started = Instant::now();
    loop {
        line_buffer.clear();
        let bytes_read = reader
            .read_until(b'\n', &mut line_buffer)
            .map_err(|err| format!("read etw csv row failed: {err}"))?;
        if bytes_read == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&line_buffer);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        total_rows += 1;
        let Some(pid_index) = pid_idx else {
            continue;
        };
        let is_kernel_file = trimmed
            .trim_start()
            .starts_with("Microsoft-Windows-Kernel-File");
        let actual_pid_index = if is_kernel_file {
            pid_index.saturating_sub(1)
        } else {
            pid_index
        };
        let Some(pid_value) = fast_csv_field(trimmed, actual_pid_index) else {
            continue;
        };
        let Some(pid) = parse_u32(pid_value) else {
            if unparsed_pid_samples.len() < 3 {
                unparsed_pid_samples.push(pid_value.to_string());
            }
            continue;
        };
        parsed_pid_rows += 1;
        if observed_pids.len() < 12 {
            observed_pids.insert(pid);
        }
        if !pid_set.is_empty() && !pid_set.contains(&pid) {
            continue;
        }
        matched_pid_rows += 1;
        if is_kernel_file {
            let actual_task_index = task_idx.map(|index| index.saturating_sub(1));
            let task = actual_task_index
                .and_then(|index| fast_csv_field(trimmed, index))
                .and_then(parse_u32);
            if task.is_some_and(should_skip_kernel_task) {
                task_prefiltered_rows += 1;
                continue;
            }
        }
        let row = parse_csv_line(trimmed);

        let extracted = path_idx
            .and_then(|idx| row.get(idx).cloned())
            .or_else(|| row.iter().find_map(|cell| extract_trace_path(cell)));
        let normalized_path = extracted.map(|path| {
            extracted_path_rows += 1;
            normalize_windows_path(&resolve_device_path(&path, &device_paths))
        });
        let file_object_id = event_file_object_id(&row, user_data_idx);
        if let Some(path) = normalized_path.as_ref() {
            if !path.is_empty() && !should_ignore_snapshot_path(Path::new(path)) {
                if let Some(object_id) = file_object_id.as_ref() {
                    file_object_paths.insert(object_id.clone(), path.clone());
                }
            } else {
                ignored_path_rows += 1;
            }
        }

        let operation_path = normalized_path.clone().or_else(|| {
            file_object_id
                .as_ref()
                .and_then(|object_id| file_object_paths.get(object_id).cloned())
        });
        if normalized_path.is_none() && operation_path.is_some() {
            object_path_resolved_rows += 1;
        }
        let operation_kind = classify_file_operation(
            event_name_idx.and_then(|idx| event_field_value(&row, idx)),
            task_idx.and_then(|idx| event_field_value(&row, idx)),
            opcode_idx.and_then(|idx| event_field_value(&row, idx)),
            keyword_idx.and_then(|idx| event_field_value(&row, idx)),
        );
        if let (Some(path), Some(operation)) = (
            operation_path.as_ref().filter(|path| !path.is_empty()),
            operation_kind,
        ) {
            operations.push(FileOperation {
                path: path.clone(),
                operation,
                timestamp_ms: clock_time_idx
                    .and_then(|idx| event_field_value(&row, idx))
                    .and_then(|value| parse_trace_timestamp_ms(value)),
                pid,
                file_object_id: file_object_id.clone(),
            });
        }

        let is_write_like = keyword_idx
            .and_then(|idx| event_field_value(&row, idx))
            .and_then(|value| parse_u64(value))
            .is_some_and(|keyword| keyword & 0x1E00 != 0);
        if !is_write_like {
            continue;
        }
        write_like_rows += 1;
        if let Some(object_id) = file_object_id {
            written_file_objects.insert(object_id);
        }
        if let Some(path) = operation_path {
            if !path.is_empty() && !should_ignore_snapshot_path(Path::new(&path)) {
                files.insert(path);
            }
        }
        if matched_row_samples.len() < 3 {
            matched_row_samples.push(trimmed.chars().take(320).collect::<String>());
        }
    }

    for object_id in &written_file_objects {
        if let Some(path) = file_object_paths.get(object_id) {
            files.insert(path.clone());
        }
    }

    let mut observed_pids = observed_pids.into_iter().collect::<Vec<_>>();
    observed_pids.sort_unstable();
    logs.push(format!(
        "事件统计：总行 {total_rows}，PID 可解析 {parsed_pid_rows}，PID 命中 {matched_pid_rows}，Task 前置过滤 {task_prefiltered_rows}，写相关 {write_like_rows}，路径提取 {extracted_path_rows}，路径忽略 {ignored_path_rows}"
    ));
    logs.push(format!(
        "文件对象关联：路径对象 {}，写入对象 {}",
        file_object_paths.len(),
        written_file_objects.len()
    ));
    logs.push(format!("CSV 中观察到的部分 PID：{observed_pids:?}"));
    if !unparsed_pid_samples.is_empty() {
        logs.push(format!("无法解析的 PID 样本：{unparsed_pid_samples:?}"));
    }
    for (index, sample) in matched_row_samples.iter().enumerate() {
        logs.push(format!("PID 命中事件样本 {}：{sample}", index + 1));
    }
    logs.push(format!("最终去重文件：{}", files.len()));
    logs.push(format!("结构化文件操作：{} 条", operations.len()));
    logs.push(format!(
        "性能：解析 CSV {} ms，共 {} 行",
        parse_started.elapsed().as_millis(),
        total_rows
    ));
    logs.push(format!(
        "操作分类：创建 {}，写入 {}，重命名 {}，删除 {}，关闭 {}，未知 {}；对象路径回填 {}",
        operations
            .iter()
            .filter(|item| item.operation == FileOperationKind::Create)
            .count(),
        operations
            .iter()
            .filter(|item| item.operation == FileOperationKind::Write)
            .count(),
        operations
            .iter()
            .filter(|item| item.operation == FileOperationKind::Rename)
            .count(),
        operations
            .iter()
            .filter(|item| item.operation == FileOperationKind::Delete)
            .count(),
        operations
            .iter()
            .filter(|item| item.operation == FileOperationKind::Close)
            .count(),
        operations
            .iter()
            .filter(|item| item.operation == FileOperationKind::Unknown)
            .count(),
        object_path_resolved_rows
    ));
    logs.push(format!(
        "性能：ETW 结果收集总计 {} ms",
        collection_started.elapsed().as_millis()
    ));
    let _ = std::fs::remove_file(&csv_path);
    Ok(TraceCollectionResult {
        files,
        logs,
        operations,
    })
}

pub(super) fn classify_file_operation(
    event_name: Option<&String>,
    task: Option<&String>,
    opcode: Option<&String>,
    keyword: Option<&String>,
) -> Option<FileOperationKind> {
    let is_kernel_file = event_name.is_some_and(|value| {
        value
            .trim()
            .eq_ignore_ascii_case("Microsoft-Windows-Kernel-File")
    });
    if is_kernel_file {
        if let Some(task) = task.and_then(|value| parse_u32(value)) {
            return match task {
                10 | 11 | 15 | 20 | 22 | 23 | 24 | 25 => None,
                12 | 30 => Some(FileOperationKind::Create),
                13 | 14 => Some(FileOperationKind::Close),
                16 | 21 => Some(FileOperationKind::Write),
                18 | 26 => Some(FileOperationKind::Delete),
                19 | 27 | 28 | 29 => Some(FileOperationKind::Rename),
                _ => Some(FileOperationKind::Unknown),
            };
        }
    }

    let text = [event_name, opcode, keyword]
        .into_iter()
        .flatten()
        .map(|value| value.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    if text.contains("rename") || text.contains("renamepath") {
        Some(FileOperationKind::Rename)
    } else if text.contains("delete") || text.contains("cleanup") {
        Some(FileOperationKind::Delete)
    } else if text.contains("close") {
        Some(FileOperationKind::Close)
    } else if text.contains("write") || text.contains("flush") {
        Some(FileOperationKind::Write)
    } else if text.contains("create") || text.contains("open") {
        Some(FileOperationKind::Create)
    } else {
        Some(FileOperationKind::Unknown)
    }
}

fn parse_trace_timestamp_ms(value: &str) -> Option<i64> {
    let trimmed = value.trim().trim_matches('"');
    if let Ok(number) = trimmed.parse::<i64>() {
        const WINDOWS_TO_UNIX_EPOCH_MS: i64 = 11_644_473_600_000;
        return Some(if number > 100_000_000_000_000 {
            number / 10_000 - WINDOWS_TO_UNIX_EPOCH_MS
        } else if number.abs() < 10_000_000_000 {
            number * 1_000
        } else {
            number
        });
    }
    let (date, time) = trimmed.split_once(char::is_whitespace)?;
    let mut date_parts = date
        .split(['/', '-'])
        .filter_map(|part| part.parse::<i32>().ok());
    let first = date_parts.next()?;
    let second = date_parts.next()?;
    let third = date_parts.next()?;
    let (year, month, day) = if first > 31 {
        (first, second, third)
    } else {
        (third, first, second)
    };
    let mut time_parts = time.split(':');
    let hour = time_parts.next()?.parse::<i64>().ok()?;
    let minute = time_parts.next()?.parse::<i64>().ok()?;
    let seconds = time_parts.next()?.replace(',', ".");
    let seconds_value = seconds.parse::<f64>().ok()?;
    let days = days_from_civil(year, month, day)?;
    Some(
        (days * 86_400_000 + hour * 3_600_000 + minute * 60_000 + (seconds_value * 1_000.0) as i64)
            as i64,
    )
}

fn days_from_civil(year: i32, month: i32, day: i32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || year < 1970 {
        return None;
    }
    let year = year - i32::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_adjusted = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_adjusted + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some((era * 146097 + day_of_era - 719468) as i64)
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

pub(super) fn should_skip_kernel_task(task: u32) -> bool {
    matches!(task, 15 | 20 | 22 | 23 | 24 | 25)
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

fn parse_u64(text: &str) -> Option<u64> {
    let trimmed = text.trim().trim_matches('"');
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        return u64::from_str_radix(hex, 16).ok();
    }
    trimmed.parse::<u64>().ok()
}

pub(crate) fn normalize_windows_path(path: &str) -> String {
    path.replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

pub(crate) fn should_ignore_snapshot_path(path: &Path) -> bool {
    let text = path.to_string_lossy().to_ascii_lowercase();
    [
        "com.gamesaver.desktop",
        "com.gamesaver.next",
        "\\appdata\\local\\temp\\",
        "\\appdata\\local\\microsoft\\windows\\powershell\\",
        "\\shadervariantanalytics\\",
        "\\cache\\",
        "\\logs\\",
        "\\crashdumps\\",
        "\\shadercache\\",
    ]
    .iter()
    .any(|fragment| text.contains(fragment))
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
    use std::path::Path;
    use super::{
        classify_file_operation, event_field_value, event_file_object_id, event_pid_value,
        fast_csv_field, is_trace_artifact, parse_csv_line, parse_trace_timestamp_ms, parse_u32,
        resolve_device_path, should_skip_kernel_task, KERNEL_FILE_CAPTURE_KEYWORDS,
    };
    use crate::services::learning::transactions::FileOperationKind;

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
    fn prefilters_only_known_non_mutating_kernel_tasks() {
        for task in [15, 20, 22, 23, 24, 25] {
            assert!(should_skip_kernel_task(task));
        }
        for task in [
            10, 11, 12, 13, 14, 16, 17, 18, 19, 21, 26, 27, 28, 29, 30, 32,
        ] {
            assert!(!should_skip_kernel_task(task));
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
            classify_file_operation(row.first(), event_field_value(&row, 7), None, None),
            Some(FileOperationKind::Write)
        ));
    }

    #[test]
    fn ignores_known_read_only_kernel_file_tasks() {
        let provider = "Microsoft-Windows-Kernel-File".to_string();
        for task in [10, 11, 15, 20, 22, 23, 24, 25] {
            let task = task.to_string();
            assert!(classify_file_operation(Some(&provider), Some(&task), None, None).is_none());
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

    #[test]
    fn converts_windows_filetime_to_unix_milliseconds() {
        assert_eq!(
            parse_trace_timestamp_ms("134313202837027064"),
            Some(1_786_846_683_702)
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
        assert!(is_trace_artifact(Path::new("C:/appdata/events/trace.etl.csv")));
        assert!(is_trace_artifact(Path::new("C:/appdata/events/trace.CSV")));
        assert!(!is_trace_artifact(Path::new("C:/appdata/events/store.json")));
        assert!(!is_trace_artifact(Path::new("C:/appdata/events/game.exe")));
    }
}
