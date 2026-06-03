use crate::path_utils::normalize_windows_path;
use crate::shared::{CandidatePath, FileMeta, Snapshot};
use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::shared::{
    APP_IDENTIFIER, CandidateAccumulator, FILENAME_SAVE_KEYWORDS, LOW_CONFIDENCE_THRESHOLD,
    NOISE_EXTENSIONS, NOISE_FILENAME_KEYWORDS, NOISE_PATH_FRAGMENTS, PATH_KEYWORDS,
    RepresentativeFileAccumulator, STRONG_NOISE_PATH_FRAGMENTS, STRONG_SAVE_EXTENSIONS,
    WEAK_PATH_FRAGMENTS, WEAK_SAVE_EXTENSIONS,
};

pub(crate) fn build_candidates(
    baseline: &Snapshot,
    final_snapshot: &Snapshot,
    game_id: &str,
    exe_path: &str,
    start_unix: u64,
    end_unix_with_grace: u64,
    related_files: Option<&HashSet<String>>,
) -> Vec<CandidatePath> {
    let mut grouped: HashMap<String, CandidateAccumulator> = HashMap::new();
    let game_id_lower = game_id.to_ascii_lowercase();
    let exe = Path::new(exe_path);
    let exe_dir = exe.parent().map(|path| normalize_windows_path(&path.to_string_lossy()));
    let exe_name = exe
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    for (path, final_meta) in &final_snapshot.files {
        if let Some(related) = related_files {
            if !related.is_empty() && !related.contains(&normalize_windows_path(path)) {
                continue;
            }
        }
        let changed = match baseline.files.get(path) {
            None => true,
            Some(base_meta) => {
                base_meta.modified_unix != final_meta.modified_unix || base_meta.size != final_meta.size
            }
        };
        if !changed {
            continue;
        }

        let raw_parent = Path::new(path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string());
        let parent = promote_candidate_parent(&raw_parent);
        let entry = grouped.entry(parent.clone()).or_insert_with(|| CandidateAccumulator {
            path: parent.clone(),
            ..CandidateAccumulator::default()
        });

        let is_added = !baseline.files.contains_key(path);
        if is_added {
            entry.added_files += 1;
        } else {
            entry.modified_files += 1;
        }
        entry.changed_files += 1;

        let in_time_window = final_meta.modified_unix >= start_unix && final_meta.modified_unix <= end_unix_with_grace;
        if in_time_window {
            entry.time_hits += 1;
            entry.signals.insert("time-window".to_string());
        }

        let extension = final_meta.extension.as_str();
        let has_strong_extension = STRONG_SAVE_EXTENSIONS.contains(&extension);
        let has_weak_extension = WEAK_SAVE_EXTENSIONS.contains(&extension);
        let has_noise_extension = NOISE_EXTENSIONS.contains(&extension);
        if has_strong_extension {
            entry.extension_hits += 1;
            entry.signals.insert(format!("extension:{}", final_meta.extension));
        } else if has_weak_extension {
            entry.weak_extension_hits += 1;
            entry.signals.insert(format!("weak-extension:{}", final_meta.extension));
        }
        if has_noise_extension {
            entry.noise_extension_hits += 1;
            entry
                .signals
                .insert(format!("noise-extension:{}", final_meta.extension));
        }

        let lower_parent = parent.to_ascii_lowercase();
        let has_save_path_keyword = matches_save_path_keyword(&lower_parent);
        if has_save_path_keyword {
            entry.keyword_hits += 1;
            entry.signals.insert("save-path-keyword".to_string());
        }

        let has_game_name_keyword = matches_game_name_keyword(&lower_parent, &game_id_lower, &exe_name);
        if has_game_name_keyword {
            entry.game_name_hits += 1;
            entry.signals.insert("game-name-path".to_string());
        }

        let has_strong_noise_path = is_strong_noise_candidate_path(&lower_parent);
        let has_weak_noise_path = !has_strong_noise_path && is_weak_candidate_path(&lower_parent);
        if has_strong_noise_path {
            entry.strong_noise_hits += 1;
            entry.signals.insert("path-noise-strong".to_string());
        } else if has_weak_noise_path {
            entry.noise_hits += 1;
            entry.signals.insert("path-noise".to_string());
        }

        let is_user_save_root = is_user_save_root_path(&lower_parent);
        if is_user_save_root {
            entry.user_save_root_hits += 1;
            entry.signals.insert("user-save-root".to_string());
        }

        let in_game_dir = exe_dir
            .as_ref()
            .is_some_and(|dir| normalize_windows_path(&parent).starts_with(dir));
        if in_game_dir {
            entry.game_dir_hits += 1;
            entry.signals.insert("game-dir".to_string());
        }

        let file_name = Path::new(path)
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let has_save_filename = matches_filename_keyword(&file_name, &FILENAME_SAVE_KEYWORDS);
        if has_save_filename {
            entry.filename_hits += 1;
            entry.signals.insert("save-filename".to_string());
        }
        let has_noise_filename = matches_filename_keyword(&file_name, &NOISE_FILENAME_KEYWORDS);
        if has_noise_filename {
            entry.noise_filename_hits += 1;
            entry.signals.insert("filename-noise".to_string());
        }

        let has_reasonable_size = final_meta.size > 0 && final_meta.size < 200 * 1024 * 1024;
        if has_reasonable_size {
            entry.reasonable_size_hits += 1;
            entry.signals.insert("size-reasonable".to_string());
        }

        entry.representative_files.push(build_representative_file(
            path,
            final_meta,
            is_added,
            in_time_window,
            has_strong_extension,
            has_weak_extension,
            has_noise_extension,
            has_save_path_keyword,
            has_game_name_keyword,
            has_strong_noise_path,
            has_weak_noise_path,
            is_user_save_root,
            in_game_dir,
            has_save_filename,
            has_noise_filename,
            has_reasonable_size,
        ));
    }

    let mut output = grouped
        .into_values()
        .map(CandidateAccumulator::into_candidate)
        .collect::<Vec<_>>();
    output.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| recommendation_rank(&b.recommendation).cmp(&recommendation_rank(&a.recommendation)))
            .then_with(|| b.changed_files.cmp(&a.changed_files))
            .then_with(|| a.path.cmp(&b.path))
    });
    output.truncate(10);
    output
}

pub(crate) fn should_ignore_snapshot_path(path: &Path) -> bool {
    let lower = path.to_string_lossy().to_ascii_lowercase();
    if lower.contains(APP_IDENTIFIER) {
        return true;
    }
    NOISE_PATH_FRAGMENTS
        .iter()
        .any(|fragment| lower.contains(fragment))
}

pub(crate) fn default_confidence() -> i64 {
    LOW_CONFIDENCE_THRESHOLD
}

fn build_representative_file(
    path: &str,
    final_meta: &FileMeta,
    is_added: bool,
    in_time_window: bool,
    has_strong_extension: bool,
    has_weak_extension: bool,
    has_noise_extension: bool,
    has_save_path_keyword: bool,
    has_game_name_keyword: bool,
    has_strong_noise_path: bool,
    has_weak_noise_path: bool,
    is_user_save_root: bool,
    in_game_dir: bool,
    has_save_filename: bool,
    has_noise_filename: bool,
    has_reasonable_size: bool,
) -> RepresentativeFileAccumulator {
    let mut score = 0_i64;
    if in_time_window {
        score += 30;
    }
    if has_strong_extension {
        score += 25;
    } else if has_weak_extension {
        score += 10;
    }
    if has_save_path_keyword {
        score += 18;
    }
    if has_game_name_keyword {
        score += 15;
    }
    if has_save_filename {
        score += 15;
    }
    if is_user_save_root {
        score += 10;
    }
    if in_game_dir {
        score += 6;
    }
    if has_reasonable_size {
        score += 6;
    }
    if is_added {
        score += 8;
    }
    if has_weak_noise_path {
        score -= 15;
    }
    if has_strong_noise_path {
        score -= 25;
    }
    if has_noise_filename {
        score -= 15;
    }
    if has_noise_extension {
        score -= 12;
    }

    RepresentativeFileAccumulator {
        path: path.to_string(),
        change_kind: if is_added { "added" } else { "modified" }.to_string(),
        size: final_meta.size,
        modified_unix: final_meta.modified_unix,
        extension: final_meta.extension.clone(),
        score,
    }
}

fn is_strong_noise_candidate_path(lower_path: &str) -> bool {
    STRONG_NOISE_PATH_FRAGMENTS
        .iter()
        .any(|fragment| lower_path.contains(fragment))
}

fn is_weak_candidate_path(lower_path: &str) -> bool {
    WEAK_PATH_FRAGMENTS
        .iter()
        .any(|fragment| lower_path.contains(fragment))
}

fn is_user_save_root_path(lower_path: &str) -> bool {
    lower_path.contains("\\appdata\\locallow\\")
        || lower_path.contains("\\appdata\\local\\")
        || lower_path.contains("\\appdata\\roaming\\")
        || lower_path.contains("\\documents\\")
        || lower_path.contains("\\saved games\\")
}

fn matches_save_path_keyword(lower_path: &str) -> bool {
    split_path_words(lower_path).iter().any(|segment| {
        PATH_KEYWORDS
            .iter()
            .any(|keyword| *segment == *keyword || segment.starts_with(keyword))
    })
}

fn matches_game_name_keyword(lower_path: &str, game_id_lower: &str, exe_name_lower: &str) -> bool {
    let segments = split_path_words(lower_path);
    let game_id_hit = if game_id_lower.trim().is_empty() {
        false
    } else {
        let compact_game_id = game_id_lower.replace(['-', '_', ' '], "");
        segments
            .iter()
            .any(|segment| segment.contains(game_id_lower) || segment.contains(&compact_game_id))
    };
    let exe_name_hit = if exe_name_lower.trim().is_empty() {
        false
    } else {
        let compact_exe = exe_name_lower.replace(['-', '_', ' '], "");
        segments
            .iter()
            .any(|segment| segment.contains(exe_name_lower) || segment.contains(&compact_exe))
    };
    game_id_hit || exe_name_hit
}

fn matches_filename_keyword(file_name_lower: &str, keywords: &[&str]) -> bool {
    let compact = file_name_lower.replace(['-', '_', ' ', '.'], "");
    keywords
        .iter()
        .any(|keyword| file_name_lower.contains(keyword) || compact.contains(keyword))
}

fn promote_candidate_parent(parent: &str) -> String {
    let normalized = parent.replace('/', "\\");
    let parts = normalized.split('\\').collect::<Vec<_>>();
    for (index, part) in parts.iter().enumerate().rev() {
        let lower = part.to_ascii_lowercase();
        if PATH_KEYWORDS
            .iter()
            .any(|keyword| lower == *keyword || lower.starts_with(keyword))
        {
            return parts[..=index].join("\\");
        }
    }
    parent.to_string()
}

fn split_path_words(lower_path: &str) -> Vec<String> {
    lower_path
        .replace('/', "\\")
        .split(|ch: char| ch == '\\' || ch == '_' || ch == '-' || ch == '.')
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.to_string())
        .collect::<Vec<_>>()
}

fn recommendation_rank(recommendation: &str) -> i32 {
    match recommendation {
        "strong" => 4,
        "recommended" => 3,
        "possible" => 2,
        _ => 1,
    }
}
