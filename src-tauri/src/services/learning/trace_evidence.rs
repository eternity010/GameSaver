use super::transactions::{FileOperation, FileOperationKind};
use crate::domain::{learning::ProcessLifetime, save_candidate::should_ignore_event_path};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

#[derive(Default)]
pub(super) struct FileEvidence {
    paths: HashMap<String, String>,
    pub files: HashSet<String>,
    pub operations: Vec<FileOperation>,
    pub ignored_paths: usize,
    pub resolved_paths: usize,
    pub mutations: usize,
}

impl FileEvidence {
    pub fn path_count(&self) -> usize {
        self.paths.len()
    }

    pub fn record(
        &mut self,
        code: u32,
        path: Option<String>,
        object: Option<String>,
        pid: u32,
        timestamp_ms: Option<i64>,
    ) {
        if matches!(code, 12 | 30) {
            if let Some(object) = object.as_ref() {
                self.paths.remove(object);
            }
        }
        let direct = path.is_some();
        let path = match path {
            Some(path) if path.is_empty() || should_ignore_event_path(Path::new(&path)) => {
                self.ignored_paths += 1;
                if let Some(object) = object.as_ref() {
                    self.paths.remove(object);
                }
                None
            }
            Some(path) => {
                if let Some(object) = object.as_ref() {
                    self.paths.insert(object.clone(), path.clone());
                }
                Some(path)
            }
            None if !matches!(code, 27 | 30) => object.as_ref().and_then(|object| {
                self.paths.get(object).cloned().inspect(|_| self.resolved_paths += 1)
            }),
            None => None,
        };
        if code == 14 {
            if let Some(object) = object.as_ref() {
                self.paths.remove(object);
            }
        }
        if let (Some(path), Some(operation)) = (path, classify_kernel_operation(code)) {
            if code == 16 || (direct && matches!(code, 27 | 30)) {
                self.mutations += 1;
                self.files.insert(path.clone());
            }
            self.operations.push(FileOperation {
                path,
                operation,
                timestamp_ms,
                pid,
                file_object_id: object,
            });
        }
    }
}

pub(super) fn reads_direct_path(code: u32) -> bool {
    matches!(code, 12 | 26 | 27 | 30)
}

pub(super) fn uses_file_object(code: u32) -> bool {
    matches!(code, 12 | 13 | 14 | 16 | 18 | 26 | 27 | 30)
}

pub(super) fn classify_kernel_operation(code: u32) -> Option<FileOperationKind> {
    match code {
        13 | 14 => Some(FileOperationKind::Close),
        16 => Some(FileOperationKind::Write),
        18 | 26 => Some(FileOperationKind::Delete),
        27 => Some(FileOperationKind::Rename),
        30 => Some(FileOperationKind::Create),
        _ => None,
    }
}

#[derive(Default)]
pub(super) struct ProcessWindows(HashMap<u32, Vec<ProcessLifetime>>);

impl ProcessWindows {
    pub fn from_tracked(tracked: &[ProcessLifetime]) -> Self {
        let mut windows = Self::default();
        for lifetime in tracked {
            windows.insert(lifetime.clone());
        }
        windows
    }

    fn insert(&mut self, lifetime: ProcessLifetime) {
        self.0.entry(lifetime.pid).or_default().push(lifetime);
    }

    pub fn contains_pid(&self, pid: u32) -> bool {
        self.0.contains_key(&pid)
    }

    pub fn contains(&self, pid: u32, time: i64) -> bool {
        self.0.get(&pid).is_some_and(|windows| windows.iter().any(|window| window.contains(time)))
    }

    pub fn pids(&self) -> impl Iterator<Item = u32> + '_ {
        self.0.keys().copied()
    }
}

#[derive(Default)]
pub(super) struct ProcessHistory {
    pub starts: HashMap<(u32, i64), u32>,
    pub stops: HashMap<(u32, i64), i64>,
}

impl ProcessHistory {
    pub fn resolve(self, tracked: &[ProcessLifetime], capture_end: i64) -> (ProcessWindows, usize) {
        let mut windows = ProcessWindows::from_tracked(tracked);
        let mut starts = self.starts.into_iter().collect::<Vec<_>>();
        starts.sort_unstable_by_key(|((_, created), _)| *created);
        let mut added = 0;
        for ((pid, created), parent) in starts {
            if pid == parent || !windows.contains(parent, created) || windows.contains(pid, created) {
                continue;
            }
            let Some(&exited) = self.stops.get(&(pid, created)) else {
                continue;
            };
            if created < exited && exited <= capture_end {
                windows.insert(ProcessLifetime { pid, started_at: created, ended_at: exited });
                added += 1;
            }
        }
        (windows, added)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_open_and_flush_do_not_count_as_writes() {
        let mut evidence = FileEvidence::default();
        evidence.record(12, Some("C:/Saves/old.sav".into()), Some("key".into()), 10, Some(1));
        evidence.record(21, None, Some("key".into()), 10, Some(2));
        evidence.record(14, None, Some("key".into()), 10, Some(3));
        assert!(evidence.files.is_empty());
        assert_eq!(evidence.operations.len(), 1);
        assert_eq!(evidence.path_count(), 0);
    }

    #[test]
    fn reused_or_ignored_object_never_inherits_a_previous_files_write() {
        let mut evidence = FileEvidence::default();
        evidence.record(12, Some("C:/Saves/old.sav".into()), Some("key".into()), 10, Some(1));
        evidence.record(16, None, Some("key".into()), 10, Some(2));
        evidence.record(14, None, Some("key".into()), 10, Some(3));
        evidence.record(16, None, Some("key".into()), 10, Some(4));
        evidence.record(12, Some("C:/Saves/readonly.sav".into()), Some("key".into()), 10, Some(5));
        evidence.record(12, Some("C:/Saves/noise.log".into()), Some("key".into()), 10, Some(6));
        evidence.record(16, None, Some("key".into()), 10, Some(7));
        assert_eq!(evidence.files, HashSet::from(["C:/Saves/old.sav".into()]));
        assert_eq!(evidence.mutations, 1);
    }

    #[test]
    fn short_lived_descendants_use_exact_pid_windows_and_require_stop_evidence() {
        let root = ProcessLifetime { pid: 10, started_at: 100, ended_at: 200 };
        let history = ProcessHistory {
            starts: HashMap::from([((20, 120), 10), ((21, 121), 20), ((30, 140), 10), ((20, 300), 99)]),
            stops: HashMap::from([((20, 120), 135), ((21, 121), 130), ((20, 300), 350)]),
        };
        let (windows, added) = history.resolve(&[root], 400);
        assert_eq!(added, 2);
        assert!(windows.contains(20, 123));
        assert!(windows.contains(21, 125));
        assert!(!windows.contains(20, 135));
        assert!(!windows.contains(20, 320));
        assert!(!windows.contains_pid(30));
    }
}
