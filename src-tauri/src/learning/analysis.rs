use crate::path_utils::normalize_windows_path;
use crate::shared::{CandidatePath, Snapshot};
use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::shared::{
    APP_IDENTIFIER, CandidateAccumulator, FILENAME_SAVE_KEYWORDS, LOW_CONFIDENCE_THRESHOLD,
    NOISE_FILENAME_KEYWORDS, NOISE_PATH_FRAGMENTS, PATH_KEYWORDS, STRONG_SAVE_EXTENSIONS,
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

        if final_meta.modified_unix >= start_unix && final_meta.modified_unix <= end_unix_with_grace {
            entry.time_hits += 1;
            entry.signals.insert("time-window".to_string());
        }

        if STRONG_SAVE_EXTENSIONS.contains(&final_meta.extension.as_str()) {
            entry.extension_hits += 1;
            entry.signals.insert(format!("extension:{}", final_meta.extension));
        } else if WEAK_SAVE_EXTENSIONS.contains(&final_meta.extension.as_str()) {
            entry.weak_extension_hits += 1;
            entry.signals.insert(format!("weak-extension:{}", final_meta.extension));
        }

        let lower_parent = parent.to_ascii_lowercase();
        if matches_save_path_keyword(&lower_parent) {
            entry.keyword_hits += 1;
            entry.signals.insert("save-path-keyword".to_string());
        }

        if matches_game_name_keyword(&lower_parent, &game_id_lower, &exe_name) {
            entry.game_name_hits += 1;
            entry.signals.insert("game-name-path".to_string());
        }

        if is_weak_candidate_path(&lower_parent) {
            entry.noise_hits += 1;
            entry.signals.insert("path-noise".to_string());
        }

        if is_user_save_root_path(&lower_parent) {
            entry.user_save_root_hits += 1;
            entry.signals.insert("user-save-root".to_string());
        }

        if exe_dir
            .as_ref()
            .is_some_and(|dir| normalize_windows_path(&parent).starts_with(dir))
        {
            entry.game_dir_hits += 1;
            entry.signals.insert("game-dir".to_string());
        }

        let file_name = Path::new(path)
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if matches_filename_keyword(&file_name, &FILENAME_SAVE_KEYWORDS) {
            entry.filename_hits += 1;
            entry.signals.insert("save-filename".to_string());
        }
        if matches_filename_keyword(&file_name, &NOISE_FILENAME_KEYWORDS) {
            entry.noise_filename_hits += 1;
            entry.signals.insert("filename-noise".to_string());
        }

        if final_meta.size > 0 && final_meta.size < 200 * 1024 * 1024 {
            entry.reasonable_size_hits += 1;
            entry.signals.insert("size-reasonable".to_string());
        }
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
