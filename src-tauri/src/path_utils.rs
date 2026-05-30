use std::collections::HashSet;
use std::path::{Path, PathBuf};

const USERPROFILE_TOKEN: &str = "%USERPROFILE%";
const DOCUMENTS_TOKEN: &str = "%DOCUMENTS%";
const APPDATA_TOKEN: &str = "%APPDATA%";
const LOCALAPPDATA_TOKEN: &str = "%LOCALAPPDATA%";
const LOCALLOW_TOKEN: &str = "%LOCALLOW%";
const SAVED_GAMES_TOKEN: &str = "%SAVED_GAMES%";
const GAME_DIR_TOKEN: &str = "%GAME_DIR%";

pub(crate) fn normalize_windows_path(path: &str) -> String {
    path.replace('/', "\\").to_ascii_lowercase()
}

pub(crate) fn normalize_system_root(path: PathBuf) -> Option<String> {
    let normalized = path.to_string_lossy().trim().replace('/', "\\");
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

pub(crate) fn resolve_user_profile_path() -> Option<String> {
    let by_user_profile = std::env::var("USERPROFILE")
        .ok()
        .map(|value| value.trim().to_string());
    if let Some(path) = by_user_profile.filter(|value| !value.is_empty()) {
        return Some(path.replace('/', "\\"));
    }
    let home_drive = std::env::var("HOMEDRIVE")
        .ok()
        .map(|value| value.trim().to_string());
    let home_path = std::env::var("HOMEPATH")
        .ok()
        .map(|value| value.trim().to_string());
    match (home_drive, home_path) {
        (Some(drive), Some(path)) if !drive.is_empty() && !path.is_empty() => {
            Some(format!("{}{}", drive, path).replace('/', "\\"))
        }
        _ => None,
    }
}

pub(crate) fn resolve_documents_path() -> Option<String> {
    let profile_root = resolve_user_profile_path()?;
    normalize_system_root(Path::new(&profile_root).join("Documents"))
}

pub(crate) fn resolve_saved_games_path() -> Option<String> {
    let profile_root = resolve_user_profile_path()?;
    normalize_system_root(Path::new(&profile_root).join("Saved Games"))
}

pub(crate) fn resolve_appdata_path() -> Option<String> {
    if let Ok(appdata) = std::env::var("APPDATA") {
        let trimmed = appdata.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.replace('/', "\\"));
        }
    }
    let profile_root = resolve_user_profile_path()?;
    normalize_system_root(Path::new(&profile_root).join("AppData").join("Roaming"))
}

pub(crate) fn resolve_local_appdata_path() -> Option<String> {
    if let Ok(appdata) = std::env::var("LOCALAPPDATA") {
        let trimmed = appdata.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.replace('/', "\\"));
        }
    }
    let profile_root = resolve_user_profile_path()?;
    normalize_system_root(Path::new(&profile_root).join("AppData").join("Local"))
}

pub(crate) fn resolve_local_low_path() -> Option<String> {
    let profile_root = resolve_user_profile_path()?;
    normalize_system_root(Path::new(&profile_root).join("AppData").join("LocalLow"))
}

fn resolve_game_dir_root(exe_path: Option<&str>) -> Option<String> {
    let exe_path = exe_path?.trim();
    if exe_path.is_empty() {
        return None;
    }
    let exe_dir = Path::new(exe_path).parent()?;
    let normalized = exe_dir.to_string_lossy().trim().replace('/', "\\");
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn resolve_anchor_root(token: &str, exe_path: Option<&str>) -> Option<String> {
    if token.eq_ignore_ascii_case(GAME_DIR_TOKEN) {
        return resolve_game_dir_root(exe_path);
    }
    if token.eq_ignore_ascii_case(SAVED_GAMES_TOKEN) {
        return resolve_saved_games_path();
    }
    if token.eq_ignore_ascii_case(DOCUMENTS_TOKEN) {
        return resolve_documents_path();
    }
    if token.eq_ignore_ascii_case(LOCALLOW_TOKEN) {
        return resolve_local_low_path();
    }
    if token.eq_ignore_ascii_case(LOCALAPPDATA_TOKEN) {
        return resolve_local_appdata_path();
    }
    if token.eq_ignore_ascii_case(APPDATA_TOKEN) {
        return resolve_appdata_path();
    }
    if token.eq_ignore_ascii_case(USERPROFILE_TOKEN) {
        return resolve_user_profile_path();
    }
    None
}

fn anchor_priority_tokens() -> [&'static str; 7] {
    [
        GAME_DIR_TOKEN,
        SAVED_GAMES_TOKEN,
        DOCUMENTS_TOKEN,
        LOCALLOW_TOKEN,
        LOCALAPPDATA_TOKEN,
        APPDATA_TOKEN,
        USERPROFILE_TOKEN,
    ]
}

fn compose_token_path(token: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        token.to_string()
    } else {
        format!(r"{}\{}", token, suffix.trim_start_matches('\\'))
    }
}

fn strip_prefix_case_insensitive(path: &str, prefix: &str) -> Option<String> {
    let normalized_path = path.replace('/', "\\");
    let normalized_prefix = prefix.replace('/', "\\").trim_end_matches('\\').to_string();
    if normalized_prefix.is_empty() {
        return None;
    }
    let path_lower = normalized_path.to_ascii_lowercase();
    let prefix_lower = normalized_prefix.to_ascii_lowercase();
    if path_lower == prefix_lower {
        return Some(String::new());
    }
    let prefix_with_sep = format!("{prefix_lower}\\");
    if path_lower.starts_with(&prefix_with_sep) {
        return Some(normalized_path[normalized_prefix.len() + 1..].to_string());
    }
    None
}

fn contains_parent_dir_segment(path: &str) -> bool {
    path.split('\\')
        .filter(|segment| !segment.is_empty())
        .any(|segment| segment == "..")
}

fn normalize_token_reference(path: &str, token: &str) -> Option<String> {
    if path.eq_ignore_ascii_case(token) {
        return Some(token.to_string());
    }
    let suffix = strip_prefix_case_insensitive(path, token)?;
    if suffix.is_empty() {
        return Some(token.to_string());
    }
    if contains_parent_dir_segment(&suffix) {
        return None;
    }
    Some(compose_token_path(token, &suffix))
}

fn strip_windows_users_prefix(path: &str) -> Option<String> {
    let normalized = path.trim().replace('/', "\\");
    let bytes = normalized.as_bytes();
    if bytes.len() < 4 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' || bytes[2] != b'\\' {
        return None;
    }
    let rest = &normalized[3..];
    let rest_lower = rest.to_ascii_lowercase();
    if !rest_lower.starts_with("users\\") {
        return None;
    }
    let after_users = &rest[6..];
    let mut splitter = after_users.splitn(2, '\\');
    let user_segment = splitter.next().unwrap_or("");
    if user_segment.is_empty() {
        return None;
    }
    let suffix = splitter.next().unwrap_or("");
    if suffix.is_empty() {
        return None;
    }
    Some(suffix.to_string())
}

fn infer_anchor_from_users_suffix(suffix: &str) -> Option<String> {
    let normalized = suffix.trim().replace('/', "\\").trim_start_matches('\\').to_string();
    if normalized.is_empty() {
        return None;
    }
    let candidates = [
        (SAVED_GAMES_TOKEN, "Saved Games"),
        (DOCUMENTS_TOKEN, "Documents"),
        (LOCALLOW_TOKEN, r"AppData\LocalLow"),
        (LOCALAPPDATA_TOKEN, r"AppData\Local"),
        (APPDATA_TOKEN, r"AppData\Roaming"),
    ];
    for (token, prefix) in candidates {
        if let Some(remainder) = strip_prefix_case_insensitive(&normalized, prefix) {
            if contains_parent_dir_segment(&remainder) {
                return None;
            }
            return Some(compose_token_path(token, &remainder));
        }
    }
    None
}

pub(crate) fn expand_confirmed_path_for_runtime(path: &str, exe_path: Option<&str>) -> Result<String, String> {
    let trimmed = path.trim().replace('/', "\\");
    if trimmed.is_empty() {
        return Err("rule path is empty".to_string());
    }
    for token in anchor_priority_tokens() {
        if let Some(suffix) = strip_prefix_case_insensitive(&trimmed, token) {
            if contains_parent_dir_segment(&suffix) {
                return Err(format!("rule path is invalid: {}", path.trim()));
            }
            let root = resolve_anchor_root(token, exe_path).ok_or_else(|| {
                if token.eq_ignore_ascii_case(GAME_DIR_TOKEN) {
                    format!("rule path depends on {}, but no exe is bound yet", GAME_DIR_TOKEN)
                } else {
                    format!("rule path depends on {}, but the system directory could not be resolved", token)
                }
            })?;
            if suffix.is_empty() {
                return Ok(root);
            }
            return Ok(format!(r"{}\{}", root.trim_end_matches('\\'), suffix));
        }
    }
    if contains_parent_dir_segment(&trimmed) {
        return Err(format!("rule path is invalid: {}", path.trim()));
    }
    Ok(trimmed)
}

pub(crate) fn normalize_confirmed_path_for_storage(path: &str, exe_path: Option<&str>) -> String {
    let trimmed = path.trim().replace('/', "\\");
    if trimmed.is_empty() {
        return String::new();
    }
    for token in anchor_priority_tokens() {
        if let Some(normalized) = normalize_token_reference(&trimmed, token) {
            return normalized;
        }
    }
    for token in anchor_priority_tokens() {
        if let Some(root) = resolve_anchor_root(token, exe_path) {
            if let Some(suffix) = strip_prefix_case_insensitive(&trimmed, &root) {
                if contains_parent_dir_segment(&suffix) {
                    continue;
                }
                return compose_token_path(token, &suffix);
            }
        }
    }
    if let Some(suffix) = strip_windows_users_prefix(&trimmed) {
        if let Some(inferred) = infer_anchor_from_users_suffix(&suffix) {
            return inferred;
        }
        if contains_parent_dir_segment(&suffix) {
            return trimmed;
        }
        return compose_token_path(USERPROFILE_TOKEN, &suffix);
    }
    trimmed
}

pub(crate) fn normalize_paths(paths: Vec<String>, exe_path: Option<&str>) -> Vec<String> {
    let mut dedup = HashSet::new();
    let mut output = Vec::new();
    for path in paths {
        let normalized = normalize_confirmed_path_for_storage(&path, exe_path);
        if normalized.is_empty() {
            continue;
        }
        let dedup_key = normalize_windows_path(&normalized);
        if dedup.insert(dedup_key) {
            output.push(normalized);
        }
    }
    output
}
