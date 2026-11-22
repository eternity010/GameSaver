use super::etw_capture::normalize_windows_path;
use crate::domain::path_utils::normalize_path;
use crate::domain::SaveTransactionSummary;
use std::collections::{HashMap, HashSet};
use std::path::Path;

const TRANSACTION_GAP_MS: i64 = 2_000;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum FileOperationKind {
    Create,
    Write,
    Rename,
    Delete,
    Close,
    Unknown,
}

#[derive(Clone)]
pub(crate) struct FileOperation {
    pub(crate) path: String,
    pub(crate) operation: FileOperationKind,
    pub(crate) timestamp_ms: Option<i64>,
    pub(crate) pid: u32,
    pub(crate) file_object_id: Option<String>,
}

struct TransactionGroup {
    operations: Vec<FileOperation>,
    completed: bool,
}

pub(crate) struct TransactionAnalysis {
    pub summary: SaveTransactionSummary,
    pub completed_paths: HashMap<String, FileOperationKind>,
}

#[cfg(test)]
pub(crate) fn analyze_save_transactions(operations: Vec<FileOperation>) -> SaveTransactionSummary {
    analyze_save_transactions_for(operations, |path| {
        !matches!(
            Path::new(path).extension().and_then(|value| value.to_str()),
            Some("tmp" | "temp" | "bak")
        )
    })
    .summary
}

#[cfg(test)]
pub(crate) fn analyze_save_transactions_for(
    operations: Vec<FileOperation>,
    is_target: impl Fn(&str) -> bool,
) -> TransactionAnalysis {
    FileOperationTrace::new(operations).analyze(|_| true, |_| true, is_target)
}

pub(crate) struct FileOperationTrace {
    operations: Vec<FileOperation>,
    write_sources: Vec<Option<usize>>,
}

impl FileOperationTrace {
    pub(crate) fn new(mut operations: Vec<FileOperation>) -> Self {
        operations.sort_by_key(|operation| operation.timestamp_ms);
        let mut writes = PendingWrites::default();
        let write_sources = operations
            .iter()
            .enumerate()
            .map(|(index, operation)| {
                let path = normalize_path(Path::new(&operation.path));
                writes.record(operation, &path, index)
            })
            .collect();
        Self {
            operations,
            write_sources,
        }
    }

    pub(crate) fn completed_writes(&self) -> impl Iterator<Item = (&FileOperation, &FileOperation)> {
        self.operations
            .iter()
            .zip(&self.write_sources)
            .filter_map(|(target, source)| source.map(|index| (&self.operations[index], target)))
    }

    pub(crate) fn analyze(
        self,
        is_evidence: impl Fn(&str) -> bool,
        is_write_source: impl Fn(&str) -> bool,
        is_target: impl Fn(&str) -> bool,
    ) -> TransactionAnalysis {
        let mut linked_writes = HashSet::new();
        let completed = self.operations
            .iter()
            .zip(self.write_sources)
            .map(|(operation, source)| {
                source.is_some_and(|index| {
                    let path = normalize_path(Path::new(&operation.path));
                    if !is_target(&path) || !is_write_source(&self.operations[index].path) {
                        return false;
                    }
                    linked_writes.insert(index);
                    true
                })
            })
            .collect::<Vec<_>>();
        let operations = self.operations
            .into_iter()
            .zip(completed)
            .enumerate()
            .filter_map(|(index, (operation, completed))| {
                (completed || linked_writes.contains(&index) || is_evidence(&operation.path))
                    .then_some((operation, completed))
            });
        summarize_transactions(operations, is_target)
    }
}

fn summarize_transactions(
    operations: impl Iterator<Item = (FileOperation, bool)>,
    is_target: impl Fn(&str) -> bool,
) -> TransactionAnalysis {
    let mut groups = Vec::<TransactionGroup>::new();
    let mut completed_paths = HashMap::new();
    for (operation, completed) in operations {
        if completed {
            completed_paths
                .entry(normalize_path(Path::new(&operation.path)))
                .and_modify(|kind| {
                    if operation.operation == FileOperationKind::Rename {
                        *kind = FileOperationKind::Rename;
                    }
                })
                .or_insert(operation.operation);
        }
        let belongs = groups.last().is_some_and(|group| {
            same_transaction_window(group.operations.last(), Some(&operation))
        });
        if belongs {
            let group = groups.last_mut().expect("transaction group exists");
            group.operations.push(operation);
            group.completed |= completed;
        } else {
            groups.push(TransactionGroup {
                operations: vec![operation],
                completed,
            });
        }
    }

    let mut affected_files = HashSet::new();
    let mut affected_directories = HashSet::new();
    let mut raw_operation_count = 0usize;
    let mut operation_count = 0usize;
    let mut confidence = 0u8;
    let mut completed_count = 0usize;
    let mut candidate_count = 0usize;
    let mut has_missing_timestamp = false;
    let mut notes = Vec::new();
    let mut started_at = None;
    let mut ended_at = None;

    for group in &groups {
        raw_operation_count += group.operations.len();
        let compacted_group = TransactionGroup {
            operations: compact_group_operations(&group.operations),
            completed: group.completed,
        };
        operation_count += compacted_group.operations.len();
        let group_confidence = score_group(
            &compacted_group,
            &mut affected_files,
            &mut affected_directories,
            &is_target,
        );
        confidence = confidence.max(group_confidence);
        if group
            .operations
            .iter()
            .any(|operation| operation.timestamp_ms.is_none())
        {
            has_missing_timestamp = true;
        }
        let group_start = group
            .operations
            .iter()
            .filter_map(|item| item.timestamp_ms)
            .min();
        let group_end = group
            .operations
            .iter()
            .filter_map(|item| item.timestamp_ms)
            .max();
        started_at = min_optional(started_at, group_start);
        ended_at = max_optional(ended_at, group_end);
        if group.completed {
            completed_count += 1;
        } else if group_confidence >= 45 {
            candidate_count += 1;
        }
    }

    if groups.is_empty() {
        notes.push("未发现可用于事务识别的 ETW 文件操作".to_string());
    }
    if has_missing_timestamp {
        confidence = confidence.saturating_sub(20);
        notes.push("部分事件缺少可靠时间戳，无法确认完整时间窗口".to_string());
    }
    if groups.len() > 1 {
        notes.push(format!("按 2 秒静默窗口拆分为 {} 个候选事务", groups.len()));
    }
    if raw_operation_count > operation_count {
        notes.push(format!(
            "已将 {raw_operation_count} 条底层文件事件合并为 {operation_count} 条事务证据"
        ));
    }
    if completed_count == 0 && candidate_count == 0 {
        notes.push("证据不足，现有快照分析仍作为独立结果保留".to_string());
    }

    let status = if completed_count > 0 && !has_missing_timestamp {
        "completed"
    } else if candidate_count > 0 || completed_count > 0 {
        "candidate"
    } else {
        "insufficient_evidence"
    };

    let mut affected_files = affected_files.into_iter().collect::<Vec<_>>();
    affected_files.sort();
    let mut affected_directories = affected_directories.into_iter().collect::<Vec<_>>();
    affected_directories.sort();
    let summary = SaveTransactionSummary {
        status: status.to_string(),
        confidence,
        transaction_count: groups.len(),
        affected_files,
        affected_directories,
        started_at: started_at.map(|value| value.to_string()),
        ended_at: ended_at.map(|value| value.to_string()),
        operation_count,
        notes,
    };
    TransactionAnalysis {
        summary,
        completed_paths,
    }
}

#[derive(Default)]
struct PendingWrites {
    by_object: HashMap<(u32, String), (String, i64, usize)>,
    by_path: HashMap<(u32, String), (i64, usize)>,
}

impl PendingWrites {
    fn record(&mut self, operation: &FileOperation, path: &str, index: usize) -> Option<usize> {
        let now = operation.timestamp_ms?;
        if path.is_empty() {
            return None;
        }
        let path_key = (operation.pid, path.to_owned());
        let object_key = operation
            .file_object_id
            .as_ref()
            .map(|object| (operation.pid, object.clone()));
        let within_window =
            |written: i64| now >= written && now.saturating_sub(written) <= TRANSACTION_GAP_MS;
        match operation.operation {
            FileOperationKind::Write => {
                if let Some(key) = object_key {
                    self.by_object.insert(key, (path.to_owned(), now, index));
                } else {
                    self.by_path.insert(path_key, (now, index));
                }
            }
            FileOperationKind::Close => {
                return if let Some(key) = object_key {
                    self.by_object
                        .remove(&key)
                        .and_then(|(written_path, time, source)| {
                            (written_path == path && within_window(time)).then_some(source)
                        })
                } else {
                    self.by_path.remove(&path_key).and_then(|(time, source)| {
                        within_window(time).then_some(source)
                    })
                };
            }
            FileOperationKind::Rename => {
                self.by_path.remove(&path_key);
                return object_key
                    .and_then(|key| self.by_object.remove(&key))
                    .and_then(|(written_path, time, source)| {
                        (written_path != path && within_window(time)).then_some(source)
                    });
            }
            FileOperationKind::Create | FileOperationKind::Delete => {
                self.by_path.remove(&path_key);
                if let Some(key) = object_key {
                    self.by_object.remove(&key);
                }
            }
            FileOperationKind::Unknown => {}
        }
        None
    }
}

fn compact_group_operations(operations: &[FileOperation]) -> Vec<FileOperation> {
    let mut seen = HashSet::new();
    operations
        .iter()
        .filter_map(|operation| {
            let normalized_path = normalize_windows_path(&operation.path);
            let key = (
                operation.pid,
                normalized_path.to_ascii_lowercase(),
                operation.operation,
            );
            seen.insert(key).then(|| {
                let mut operation = operation.clone();
                operation.path = normalized_path;
                operation
            })
        })
        .collect()
}

fn same_transaction_window(
    previous: Option<&FileOperation>,
    current: Option<&FileOperation>,
) -> bool {
    let (Some(previous), Some(current)) = (previous, current) else {
        return false;
    };
    match (previous.timestamp_ms, current.timestamp_ms) {
        (Some(prev_ts), Some(curr_ts)) => {
            if curr_ts.saturating_sub(prev_ts) > TRANSACTION_GAP_MS {
                return false;
            }
            if previous.pid != current.pid {
                return false;
            }
            if previous.file_object_id.is_some()
                && current.file_object_id.is_some()
                && previous.file_object_id == current.file_object_id
            {
                return true;
            }
            let prev_parent = Path::new(&previous.path).parent();
            let curr_parent = Path::new(&current.path).parent();
            prev_parent.is_some() && prev_parent == curr_parent
        }
        _ => false,
    }
}

fn score_group(
    group: &TransactionGroup,
    affected_files: &mut HashSet<String>,
    affected_directories: &mut HashSet<String>,
    is_target: &impl Fn(&str) -> bool,
) -> u8 {
    let mut score = 0i16;
    let mut has_write = false;
    let mut has_close = false;
    let mut has_rename = false;
    let mut has_delete = false;
    let mut has_unknown = false;
    let mut pids = HashSet::new();
    let mut has_target = false;

    for operation in &group.operations {
        let path = normalize_windows_path(&operation.path);
        if path.is_empty() {
            continue;
        }
        has_target |= is_target(&normalize_path(Path::new(&operation.path)));
        affected_files.insert(path.clone());
        if let Some(parent) = Path::new(&path).parent() {
            affected_directories.insert(normalize_windows_path(&parent.to_string_lossy()));
        }
        pids.insert(operation.pid);
        match operation.operation {
            FileOperationKind::Write => has_write = true,
            FileOperationKind::Close => has_close = true,
            FileOperationKind::Rename => has_rename = true,
            FileOperationKind::Delete => has_delete = true,
            FileOperationKind::Unknown => has_unknown = true,
            FileOperationKind::Create => {}
        }
    }

    if has_write {
        score += 30;
    }
    if has_close {
        score += 30;
    }
    if has_rename {
        score += 30;
    }
    if group.operations.len() > 1 {
        score += 10;
    }
    if pids.len() == 1 {
        score += 10;
    }
    if has_delete && !has_rename {
        score -= 30;
    }
    if has_unknown && !has_write && !has_rename {
        score -= 20;
    }
    if group
        .operations
        .iter()
        .any(|item| item.timestamp_ms.is_none())
    {
        score -= 20;
    }
    if group.completed {
        score.clamp(80, 100) as u8
    } else if has_target {
        score.clamp(0, 79) as u8
    } else {
        0
    }
}

fn min_optional(current: Option<i64>, candidate: Option<i64>) -> Option<i64> {
    match (current, candidate) {
        (Some(current), Some(candidate)) => Some(current.min(candidate)),
        (None, candidate) => candidate,
        (current, None) => current,
    }
}

fn max_optional(current: Option<i64>, candidate: Option<i64>) -> Option<i64> {
    match (current, candidate) {
        (Some(current), Some(candidate)) => Some(current.max(candidate)),
        (None, candidate) => candidate,
        (current, None) => current,
    }
}

#[cfg(test)]
mod tests {
    use super::{analyze_save_transactions, FileOperation, FileOperationKind};

    fn operation(path: &str, kind: FileOperationKind, timestamp_ms: Option<i64>) -> FileOperation {
        FileOperation {
            path: path.to_string(),
            operation: kind,
            timestamp_ms,
            pid: 10,
            file_object_id: None,
        }
    }

    #[test]
    fn direct_write_and_close_is_completed() {
        let result = analyze_save_transactions(vec![
            operation("C:/save/slot.sav", FileOperationKind::Write, Some(1_000)),
            operation("C:/save/slot.sav", FileOperationKind::Close, Some(1_500)),
        ]);
        assert_eq!(result.status, "completed");
        assert_eq!(result.transaction_count, 1);
    }

    #[test]
    fn transactions_split_after_two_seconds() {
        let result = analyze_save_transactions(vec![
            operation("C:/save/a.sav", FileOperationKind::Write, Some(1_000)),
            operation("C:/save/b.sav", FileOperationKind::Write, Some(4_000)),
        ]);
        assert_eq!(result.transaction_count, 2);
    }

    #[test]
    fn transactions_split_across_unrelated_directories() {
        let result = analyze_save_transactions(vec![
            operation("C:/game/config.ini", FileOperationKind::Write, Some(1_000)),
            operation("C:/save/slot.sav", FileOperationKind::Write, Some(1_100)),
        ]);
        assert_eq!(result.transaction_count, 2);
    }

    #[test]
    fn temp_write_and_rename_is_completed() {
        let mut operations = vec![
            operation("C:/save/slot.tmp", FileOperationKind::Write, Some(1_000)),
            operation("C:/save/slot.sav", FileOperationKind::Rename, Some(1_200)),
        ];
        assert_ne!(
            analyze_save_transactions(operations.clone()).status,
            "completed"
        );
        for event in &mut operations {
            event.file_object_id = Some("0x1234".to_owned());
        }
        let result = analyze_save_transactions(operations);
        assert_eq!(result.status, "completed");
        assert!(result
            .affected_files
            .iter()
            .any(|path| path.ends_with("slot.sav")));
    }

    #[test]
    fn unrelated_closes_and_temporary_files_do_not_complete_a_save() {
        for (written, closed) in [
            ("C:/save/a.sav", "C:/save/b.sav"),
            ("C:/save/slot.tmp", "C:/save/slot.tmp"),
        ] {
            let result = analyze_save_transactions(vec![
                operation(written, FileOperationKind::Write, Some(1_000)),
                operation(closed, FileOperationKind::Close, Some(1_100)),
            ]);
            assert_ne!(result.status, "completed");
            assert!(result.confidence < 80);
        }
    }

    #[test]
    fn writes_survive_unrelated_events_but_not_a_different_file_object() {
        let mut write = operation("C:/save/slot.sav", FileOperationKind::Write, Some(1_000));
        write.file_object_id = Some("object-1".into());
        let mut close = operation("C:/save/slot.sav", FileOperationKind::Close, Some(1_200));
        close.file_object_id = Some("object-2".into());
        let other = operation("C:/other/slot.sav", FileOperationKind::Write, Some(1_100));
        assert_ne!(
            analyze_save_transactions(vec![write.clone(), other.clone(), close.clone()]).status,
            "completed"
        );
        close.file_object_id = write.file_object_id.clone();
        assert_eq!(
            analyze_save_transactions(vec![write, other, close]).status,
            "completed"
        );
    }

    #[test]
    fn delete_without_replacement_is_not_completed() {
        let result = analyze_save_transactions(vec![operation(
            "C:/save/slot.sav",
            FileOperationKind::Delete,
            Some(1_000),
        )]);
        assert_eq!(result.status, "insufficient_evidence");
    }

    #[test]
    fn unicode_paths_are_preserved() {
        let result = analyze_save_transactions(vec![operation(
            "C:/存档/日文セーブ/slot.sav",
            FileOperationKind::Write,
            None,
        )]);
        assert!(result
            .affected_files
            .iter()
            .any(|path| path.contains("日文セーブ")));
        assert_eq!(result.status, "insufficient_evidence");
    }

    #[test]
    fn repeated_low_level_writes_are_compacted_per_transaction() {
        let mut operations = (0..100)
            .map(|offset| {
                operation(
                    "C:/save/slot.sav",
                    FileOperationKind::Write,
                    Some(1_000 + offset),
                )
            })
            .collect::<Vec<_>>();
        operations.push(operation(
            "C:/save/slot.sav",
            FileOperationKind::Close,
            Some(1_200),
        ));
        let result = analyze_save_transactions(operations);
        assert_eq!(result.operation_count, 2);
        assert_eq!(result.status, "completed");
    }
}
