use crate::path_utils::{
    normalize_system_root, normalize_windows_path, resolve_appdata_path, resolve_documents_path,
    resolve_local_appdata_path, resolve_local_low_path, resolve_saved_games_path,
};
use crate::runtime::{now_unix, system_time_to_unix};
use crate::shared::{FileMeta, Snapshot};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};
use walkdir::WalkDir;

use super::analysis::should_ignore_snapshot_path;

pub(crate) fn collect_snapshot(
    game_id: &str,
    exe_path: &str,
    extra_scan_roots: &[String],
) -> Result<Snapshot, String> {
    let roots = collect_scan_roots(exe_path, extra_scan_roots)?;
    let mut files = HashMap::new();
    for root in roots {
        if !root.exists() {
            continue;
        }
        for entry in WalkDir::new(&root).into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            if should_ignore_snapshot_path(entry.path()) {
                continue;
            }
            let metadata = match entry.metadata() {
                Ok(value) => value,
                Err(_) => continue,
            };
            let modified_unix = metadata
                .modified()
                .ok()
                .and_then(system_time_to_unix)
                .unwrap_or_default();
            let size = metadata.len();
            let extension = entry
                .path()
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            files.insert(
                entry.path().to_string_lossy().to_string(),
                FileMeta {
                    size,
                    modified_unix,
                    extension,
                },
            );
        }
    }

    Ok(Snapshot {
        snapshot_ref: format!("snapshot_{}_{}.json", game_id, now_unix()),
        created_at_unix: now_unix(),
        files,
    })
}

pub(crate) fn normalize_learning_scan_root(root: &str) -> Option<String> {
    let trimmed = root.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path = Path::new(trimmed);
    let candidate = if path.is_file() {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    let normalized = normalize_windows_path(&candidate.to_string_lossy());
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

pub(crate) fn snapshots_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let base = app
        .path()
        .app_data_dir()
        .map_err(|err| format!("resolve snapshots dir failed: {err}"))?
        .join("snapshots");
    fs::create_dir_all(&base).map_err(|err| format!("create snapshots dir failed: {err}"))?;
    Ok(base)
}

pub(crate) fn event_logs_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let base = app
        .path()
        .app_data_dir()
        .map_err(|err| format!("resolve event dir failed: {err}"))?
        .join("events");
    fs::create_dir_all(&base).map_err(|err| format!("create event dir failed: {err}"))?;
    Ok(base)
}

pub(crate) fn write_snapshot(app: &AppHandle, snapshot_ref: &str, snapshot: &Snapshot) -> Result<(), String> {
    let path = snapshots_dir(app)?.join(snapshot_ref);
    let content = serde_json::to_string(snapshot).map_err(|err| format!("serialize snapshot failed: {err}"))?;
    fs::write(path, content).map_err(|err| format!("write snapshot failed: {err}"))
}

pub(crate) fn read_snapshot(app: &AppHandle, snapshot_ref: &str) -> Result<Snapshot, String> {
    let path = snapshots_dir(app)?.join(snapshot_ref);
    let content = fs::read_to_string(path).map_err(|err| format!("read snapshot failed: {err}"))?;
    serde_json::from_str(&content).map_err(|err| format!("parse snapshot failed: {err}"))
}

fn collect_scan_roots(exe_path: &str, extra_scan_roots: &[String]) -> Result<Vec<PathBuf>, String> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();
    for root in [
        resolve_saved_games_path(),
        resolve_documents_path(),
        resolve_local_appdata_path(),
        resolve_local_low_path(),
        resolve_appdata_path(),
        resolve_programdata_path(),
        resolve_public_documents_path(),
    ]
    .into_iter()
    .flatten()
    {
        push_scan_root(&mut roots, &mut seen, PathBuf::from(root));
    }
    for root in collect_steam_userdata_roots() {
        push_scan_root(&mut roots, &mut seen, root);
    }
    for root in extra_scan_roots {
        push_scan_root(&mut roots, &mut seen, PathBuf::from(root));
    }
    if let Some(exe_dir) = Path::new(exe_path).parent() {
        push_scan_root(&mut roots, &mut seen, exe_dir.to_path_buf());
    }
    if roots.is_empty() {
        return Err("no scan roots available".to_string());
    }
    Ok(roots)
}

fn push_scan_root(roots: &mut Vec<PathBuf>, seen: &mut HashSet<String>, root: PathBuf) {
    let normalized = normalize_windows_path(&root.to_string_lossy());
    if normalized.is_empty() || !seen.insert(normalized) {
        return;
    }
    roots.push(root);
}

fn resolve_programdata_path() -> Option<String> {
    if let Ok(path) = std::env::var("ProgramData") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.replace('/', "\\"));
        }
    }
    normalize_system_root(PathBuf::from("C:\\ProgramData"))
}

fn resolve_public_documents_path() -> Option<String> {
    if let Ok(public_path) = std::env::var("PUBLIC") {
        let trimmed = public_path.trim();
        if !trimmed.is_empty() {
            return normalize_system_root(Path::new(trimmed).join("Documents"));
        }
    }
    normalize_system_root(PathBuf::from("C:\\Users\\Public\\Documents"))
}

fn collect_steam_userdata_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();
    for steam_root in collect_steam_install_roots() {
        let userdata = steam_root.join("userdata");
        if !userdata.exists() || !userdata.is_dir() {
            continue;
        }
        push_steam_userdata_root(&mut roots, &mut seen, userdata.clone());
        if let Ok(entries) = fs::read_dir(&userdata) {
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    push_steam_userdata_root(&mut roots, &mut seen, path);
                }
            }
        }
    }
    roots
}

fn push_steam_userdata_root(roots: &mut Vec<PathBuf>, seen: &mut HashSet<String>, path: PathBuf) {
    let normalized = normalize_windows_path(&path.to_string_lossy());
    if normalized.is_empty() || !seen.insert(normalized) {
        return;
    }
    roots.push(path);
}

fn collect_steam_install_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();
    for candidate in [
        PathBuf::from("C:\\Program Files (x86)\\Steam"),
        PathBuf::from("C:\\Program Files\\Steam"),
    ] {
        let normalized = normalize_windows_path(&candidate.to_string_lossy());
        if candidate.exists() && seen.insert(normalized) {
            roots.push(candidate);
        }
    }
    if let Ok(program_files_x86) = std::env::var("ProgramFiles(x86)") {
        let candidate = Path::new(program_files_x86.trim()).join("Steam");
        let normalized = normalize_windows_path(&candidate.to_string_lossy());
        if candidate.exists() && seen.insert(normalized) {
            roots.push(candidate);
        }
    }
    if let Ok(program_files) = std::env::var("ProgramFiles") {
        let candidate = Path::new(program_files.trim()).join("Steam");
        let normalized = normalize_windows_path(&candidate.to_string_lossy());
        if candidate.exists() && seen.insert(normalized) {
            roots.push(candidate);
        }
    }
    roots
}
