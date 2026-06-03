use crate::shared::{CandidatePath, RepresentativeChangedFile};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::PathBuf;

pub(crate) const SCORE_TIME_MATCH: i64 = 30;
pub(crate) const SCORE_EXTENSION_MATCH: i64 = 20;
pub(crate) const SCORE_WEAK_EXTENSION_MATCH: i64 = 8;
pub(crate) const SCORE_KEYWORD_MATCH: i64 = 25;
pub(crate) const SCORE_GAME_NAME_MATCH: i64 = 20;
pub(crate) const SCORE_FILENAME_MATCH: i64 = 12;
pub(crate) const SCORE_CHANGE_COUNT_MATCH: i64 = 10;
pub(crate) const SCORE_ADDED_FILE_MATCH: i64 = 8;
pub(crate) const SCORE_USER_SAVE_ROOT_MATCH: i64 = 10;
pub(crate) const SCORE_GAME_DIR_MATCH: i64 = 6;
pub(crate) const SCORE_SIZE_REASONABLE: i64 = 6;
pub(crate) const SCORE_NOISE_PATH_PENALTY: i64 = 30;
pub(crate) const SCORE_STRONG_NOISE_PATH_PENALTY: i64 = 20;
pub(crate) const SCORE_NOISE_FILENAME_PENALTY: i64 = 20;
pub(crate) const SCORE_NOISE_EXTENSION_PENALTY: i64 = 12;
pub(crate) const SCORE_TOO_MANY_CHANGES_PENALTY: i64 = 20;
pub(crate) const SCORE_WEAK_ONLY_PENALTY: i64 = 15;
pub(crate) const LOW_CONFIDENCE_THRESHOLD: i64 = 45;
pub(crate) const RECOMMENDED_SCORE_THRESHOLD: i64 = 80;
pub(crate) const STRONG_SCORE_THRESHOLD: i64 = 100;
pub(crate) const REPRESENTATIVE_FILE_LIMIT: usize = 5;
pub(crate) const STRONG_SAVE_EXTENSIONS: [&str; 4] = ["sav", "save", "profile", "slot"];
pub(crate) const WEAK_SAVE_EXTENSIONS: [&str; 3] = ["dat", "json", "bin"];
pub(crate) const NOISE_EXTENSIONS: [&str; 5] = ["log", "tmp", "bak", "dmp", "mdmp"];
pub(crate) const PATH_KEYWORDS: [&str; 4] = ["save", "savedata", "profile", "userdata"];
pub(crate) const FILENAME_SAVE_KEYWORDS: [&str; 5] = ["save", "slot", "profile", "global", "system"];
pub(crate) const NOISE_FILENAME_KEYWORDS: [&str; 15] = [
    "config",
    "settings",
    "log",
    "cache",
    "crash",
    "tmp",
    "temp",
    "shader",
    "bak",
    "backup",
    "dump",
    "telemetry",
    "analytics",
    "prefs",
    "option",
];
pub(crate) const STRONG_NOISE_PATH_FRAGMENTS: [&str; 6] = [
    "\\telemetry\\",
    "\\analytics\\",
    "\\crashdumps\\",
    "\\dumps\\",
    "\\gpucache\\",
    "\\webcache\\",
];
pub(crate) const WEAK_PATH_FRAGMENTS: [&str; 14] = [
    "\\cache\\",
    "\\logs\\",
    "\\log\\",
    "\\crash\\",
    "\\config\\",
    "\\settings\\",
    "\\shader",
    "\\backup\\",
    "\\temp\\",
    "\\tmp\\",
    "\\telemetry\\",
    "\\analytics\\",
    "\\dumps\\",
    "\\crashdumps\\",
];
pub(crate) const NOISE_PATH_FRAGMENTS: [&str; 15] = [
    "\\appdata\\local\\temp\\",
    "\\appdata\\local\\tencent\\wetype\\",
    "\\appdata\\locallow\\tencent\\wetype\\",
    "\\appdata\\roaming\\tencent\\wechat\\",
    "\\appdata\\roaming\\tencent\\xwechat\\",
    "\\appdata\\local\\microsoft\\edge\\",
    "\\appdata\\local\\microsoft\\windows\\powershell\\",
    "\\appdata\\local\\google\\chrome\\",
    "\\appdata\\roaming\\mozilla\\firefox\\profiles\\",
    "\\appdata\\local\\mozilla\\firefox\\profiles\\",
    "\\appdata\\roaming\\microsoft\\windows\\",
    "\\appdata\\local\\discord\\",
    "\\appdata\\roaming\\discord\\",
    "\\$recycle.bin\\",
    "\\ebwebview\\",
];
pub(crate) const APP_IDENTIFIER: &str = "com.gamesaver.desktop";

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct CimProcessRow {
    pub(crate) process_id: u32,
    pub(crate) parent_process_id: u32,
}

#[derive(Clone)]
pub(crate) struct EventCaptureHandle {
    pub(crate) trace_name: String,
    pub(crate) etl_path: PathBuf,
}

#[derive(Clone)]
pub(crate) struct RepresentativeFileAccumulator {
    pub(crate) path: String,
    pub(crate) change_kind: String,
    pub(crate) size: u64,
    pub(crate) modified_unix: u64,
    pub(crate) extension: String,
    pub(crate) score: i64,
}

#[derive(Default)]
pub(crate) struct CandidateAccumulator {
    pub(crate) path: String,
    pub(crate) added_files: usize,
    pub(crate) modified_files: usize,
    pub(crate) changed_files: usize,
    pub(crate) time_hits: usize,
    pub(crate) extension_hits: usize,
    pub(crate) weak_extension_hits: usize,
    pub(crate) keyword_hits: usize,
    pub(crate) game_name_hits: usize,
    pub(crate) filename_hits: usize,
    pub(crate) reasonable_size_hits: usize,
    pub(crate) user_save_root_hits: usize,
    pub(crate) game_dir_hits: usize,
    pub(crate) noise_hits: usize,
    pub(crate) strong_noise_hits: usize,
    pub(crate) noise_filename_hits: usize,
    pub(crate) noise_extension_hits: usize,
    pub(crate) representative_files: Vec<RepresentativeFileAccumulator>,
    pub(crate) signals: HashSet<String>,
}

impl CandidateAccumulator {
    pub(crate) fn into_candidate(self) -> CandidatePath {
        let mut score = 0;
        if self.time_hits > 0 {
            score += SCORE_TIME_MATCH;
        }
        if self.extension_hits > 0 {
            score += SCORE_EXTENSION_MATCH;
        }
        if self.weak_extension_hits > 0 {
            score += SCORE_WEAK_EXTENSION_MATCH;
        }
        if self.keyword_hits > 0 {
            score += SCORE_KEYWORD_MATCH;
        }
        if self.game_name_hits > 0 {
            score += SCORE_GAME_NAME_MATCH;
        }
        if self.filename_hits > 0 {
            score += SCORE_FILENAME_MATCH;
        }
        if (1..=50).contains(&self.changed_files) {
            score += SCORE_CHANGE_COUNT_MATCH;
        }
        if self.added_files > 0 {
            score += SCORE_ADDED_FILE_MATCH;
        }
        if self.user_save_root_hits > 0 {
            score += SCORE_USER_SAVE_ROOT_MATCH;
        }
        if self.game_dir_hits > 0 {
            score += SCORE_GAME_DIR_MATCH;
        }
        if self.reasonable_size_hits > 0 {
            score += SCORE_SIZE_REASONABLE;
        }
        if self.noise_hits > 0 {
            score -= SCORE_NOISE_PATH_PENALTY;
        }
        if self.strong_noise_hits > 0 {
            score -= SCORE_STRONG_NOISE_PATH_PENALTY;
        }
        if self.noise_filename_hits > 0 {
            score -= SCORE_NOISE_FILENAME_PENALTY;
        }
        if self.noise_extension_hits > 0 {
            score -= SCORE_NOISE_EXTENSION_PENALTY;
        }
        if self.changed_files > 200 {
            score -= SCORE_TOO_MANY_CHANGES_PENALTY;
        }
        if self.weak_extension_hits > 0 && self.extension_hits == 0 && self.keyword_hits == 0 {
            score -= SCORE_WEAK_ONLY_PENALTY;
        }
        score = score.max(0);

        let mut signals = self.signals.into_iter().collect::<Vec<_>>();
        signals.sort();
        let effective_signal_count = [
            self.extension_hits > 0,
            self.keyword_hits > 0,
            self.game_name_hits > 0,
            self.filename_hits > 0,
            self.user_save_root_hits > 0,
            self.game_dir_hits > 0,
            self.added_files > 0,
        ]
        .into_iter()
        .filter(|hit| *hit)
        .count();
        let noisy = self.noise_hits > 0
            || self.strong_noise_hits > 0
            || self.noise_filename_hits > 0
            || self.noise_extension_hits > 0
            || self.changed_files > 200;
        let strong_signals = self.time_hits > 0
            && self.keyword_hits > 0
            && (self.extension_hits > 0 || self.game_name_hits > 0);
        let base_recommendation = if strong_signals || score >= STRONG_SCORE_THRESHOLD {
            "strong"
        } else if (self.time_hits > 0 && effective_signal_count >= 2) || score >= RECOMMENDED_SCORE_THRESHOLD {
            "recommended"
        } else if (self.time_hits > 0 && effective_signal_count >= 1) || score >= LOW_CONFIDENCE_THRESHOLD {
            "possible"
        } else {
            "weak"
        };
        let recommendation = if noisy {
            match base_recommendation {
                "strong" => "recommended",
                "recommended" => "possible",
                other => other,
            }
        } else {
            base_recommendation
        };

        let mut representative_files = self.representative_files;
        representative_files.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| b.modified_unix.cmp(&a.modified_unix))
                .then_with(|| a.path.cmp(&b.path))
        });
        representative_files.truncate(REPRESENTATIVE_FILE_LIMIT);
        let representative_changed_files = representative_files
            .into_iter()
            .map(|file| RepresentativeChangedFile {
                path: file.path,
                change_kind: file.change_kind,
                size: file.size,
                modified_unix: file.modified_unix,
                extension: file.extension,
            })
            .collect::<Vec<_>>();

        CandidatePath {
            path: self.path,
            score,
            changed_files: self.changed_files,
            added_files: self.added_files,
            modified_files: self.modified_files,
            matched_signals: signals,
            representative_changed_files,
            recommendation: recommendation.to_string(),
            collapsed: score < LOW_CONFIDENCE_THRESHOLD,
        }
    }
}
