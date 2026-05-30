use crate::path_utils::normalize_windows_path;
use crate::runtime::apply_background_process_flags;
use crate::storage::decode_text_bytes;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::process::Command;
use tauri::AppHandle;

use super::analysis::should_ignore_snapshot_path;
use super::shared::{CimProcessRow, EventCaptureHandle};
use super::snapshot::event_logs_dir;

pub(crate) fn collect_process_tree_pids(root_pid: u32) -> Result<Vec<u32>, String> {
    let script = "Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId | ConvertTo-Json -Compress";
    let mut command = Command::new("powershell");
    command.args(["-NoProfile", "-Command", script]);
    let output = apply_background_process_flags(&mut command)
        .output()
        .map_err(|err| format!("read process list failed: {err}"))?;
    if !output.status.success() {
        return Err("read process list failed: powershell returned non-zero".to_string());
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        return Ok(vec![root_pid]);
    }

    let rows = if stdout.starts_with('[') {
        serde_json::from_str::<Vec<CimProcessRow>>(&stdout)
            .map_err(|err| format!("parse process list failed: {err}"))?
    } else {
        let single = serde_json::from_str::<CimProcessRow>(&stdout)
            .map_err(|err| format!("parse process list failed: {err}"))?;
        vec![single]
    };

    let mut child_map: HashMap<u32, Vec<u32>> = HashMap::new();
    for row in rows {
        child_map
            .entry(row.parent_process_id)
            .or_default()
            .push(row.process_id);
    }

    let mut tracked = Vec::new();
    let mut visited = HashSet::new();
    let mut stack = vec![root_pid];
    while let Some(current) = stack.pop() {
        if !visited.insert(current) {
            continue;
        }
        tracked.push(current);
        if let Some(children) = child_map.get(&current) {
            for child in children {
                stack.push(*child);
            }
        }
    }

    tracked.sort_unstable();
    Ok(tracked)
}

pub(crate) fn try_start_etw_capture(app: &AppHandle, session_id: &str) -> Result<EventCaptureHandle, String> {
    if !is_running_as_admin() {
        return Err("current process is not elevated, fallback to snapshot mode".to_string());
    }

    let trace_name = format!("GameSaverTrace_{}", session_id.replace('-', "").chars().take(10).collect::<String>());
    let etl_path = event_logs_dir(app)?.join(format!("{trace_name}.etl"));
    let etl_path_str = etl_path.to_string_lossy().to_string();

    let _ = {
        let mut command = Command::new("logman");
        command.args(["stop", &trace_name, "-ets"]);
        apply_background_process_flags(&mut command).output()
    };

    let created = {
        let mut command = Command::new("logman");
        command.args([
            "create",
            "trace",
            &trace_name,
            "-o",
            &etl_path_str,
            "-p",
            "Microsoft-Windows-Kernel-File",
            "0x10",
            "5",
            "-ets",
        ]);
        apply_background_process_flags(&mut command)
            .output()
            .map_err(|err| format!("start etw failed: {err}"))?
    };
    if !created.status.success() {
        let stderr = String::from_utf8_lossy(&created.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&created.stdout).trim().to_string();
        let msg = if !stderr.is_empty() { stderr } else { stdout };
        return Err(format!("start etw failed: {msg}"));
    }

    Ok(EventCaptureHandle { trace_name, etl_path })
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
    let text = String::from_utf8_lossy(&out.stdout).trim().to_ascii_lowercase();
    text.contains("true")
}

pub(crate) fn collect_related_files_by_trace(
    trace_name: Option<&str>,
    trace_path: Option<&str>,
    tracked_pids: &[u32],
) -> Result<HashSet<String>, String> {
    let mut files = HashSet::new();
    let Some(name) = trace_name else {
        return Ok(files);
    };
    let Some(etl_path) = trace_path else {
        return Ok(files);
    };

    let _ = {
        let mut command = Command::new("logman");
        command.args(["stop", name, "-ets"]);
        apply_background_process_flags(&mut command).output()
    };

    let csv_path = format!("{etl_path}.csv");
    let converted = {
        let mut command = Command::new("tracerpt");
        command.args([etl_path, "-of", "CSV", "-o", &csv_path, "-y"]);
        apply_background_process_flags(&mut command)
            .output()
            .map_err(|err| format!("parse etw failed: {err}"))?
    };
    if !converted.status.success() {
        let stderr = String::from_utf8_lossy(&converted.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&converted.stdout).trim().to_string();
        let msg = if !stderr.is_empty() { stderr } else { stdout };
        return Err(format!("parse etw failed: {msg}"));
    }

    let raw = fs::read(&csv_path).map_err(|err| format!("read etw csv failed: {err}"))?;
    let content = decode_text_bytes(&raw);
    let mut lines = content.lines();
    let Some(header_line) = lines.next() else {
        return Ok(files);
    };
    let headers = parse_csv_line(header_line);
    let pid_idx = find_header_index(&headers, &["processid", "process id", "pid"]);
    let path_idx = find_header_index(&headers, &["filename", "file name", "filepath", "file path", "pathname", "path"]);
    let op_idx = find_header_index(&headers, &["opcode", "opcode name", "task", "task name", "eventname", "event name", "operation"]);

    let pid_set = tracked_pids.iter().copied().collect::<HashSet<u32>>();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let row = parse_csv_line(trimmed);
        let Some(pid_index) = pid_idx else {
            continue;
        };
        let Some(pid_value) = row.get(pid_index) else {
            continue;
        };
        let Some(pid) = parse_u32(pid_value) else {
            continue;
        };
        if !pid_set.is_empty() && !pid_set.contains(&pid) {
            continue;
        }

        let op_text = op_idx
            .and_then(|idx| row.get(idx))
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        let is_write_like = op_text.is_empty()
            || op_text.contains("write")
            || op_text.contains("create")
            || op_text.contains("setinfo")
            || op_text.contains("rename");
        if !is_write_like {
            continue;
        }

        let extracted = path_idx
            .and_then(|idx| row.get(idx).cloned())
            .or_else(|| row.iter().find_map(|cell| extract_windows_path(cell)));
        let Some(path) = extracted else {
            continue;
        };
        let normalized = normalize_windows_path(&path);
        if !normalized.is_empty() && !should_ignore_snapshot_path(Path::new(&normalized)) {
            files.insert(normalized);
        }
    }

    Ok(files)
}

fn extract_windows_path(text: &str) -> Option<String> {
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
    None
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
    let digits = text
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if digits.is_empty() {
        None
    } else {
        digits.parse::<u32>().ok()
    }
}
