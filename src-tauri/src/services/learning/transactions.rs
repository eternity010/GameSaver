use super::etw_capture::normalize_windows_path;
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
}

pub(crate) fn analyze_save_transactions(
    mut operations: Vec<FileOperation>,
) -> SaveTransactionSummary {
    operations.sort_by_key(|operation| operation.timestamp_ms);
    let mut groups = Vec::<TransactionGroup>::new();
    for operation in operations {
        let belongs = groups.last().is_some_and(|group| {
            same_transaction_window(group.operations.last(), Some(&operation))
        });
        if belongs {
            groups
                .last_mut()
                .expect("transaction group exists")
                .operations
                .push(operation);
        } else {
            groups.push(TransactionGroup {
                operations: vec![operation],
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
        };
        operation_count += compacted_group.operations.len();
        let group_confidence = score_group(
            &compacted_group,
            &mut affected_files,
            &mut affected_directories,
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
        if group_confidence >= 80 {
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
    SaveTransactionSummary {
        status: status.to_string(),
        confidence,
        transaction_count: groups.len(),
        affected_files,
        affected_directories,
        started_at: started_at.map(|value| value.to_string()),
        ended_at: ended_at.map(|value| value.to_string()),
        operation_count,
        notes,
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
        (Some(previous), Some(current)) => current.saturating_sub(previous) <= TRANSACTION_GAP_MS,
        _ => false,
    }
}

fn score_group(
    group: &TransactionGroup,
    affected_files: &mut HashSet<String>,
    affected_directories: &mut HashSet<String>,
) -> u8 {
    let mut score = 0i16;
    let mut has_write = false;
    let mut has_close = false;
    let mut has_rename = false;
    let mut has_delete = false;
    let mut has_unknown = false;
    let mut pids = HashSet::new();
    let mut object_paths = HashMap::new();

    for operation in &group.operations {
        let path = normalize_windows_path(&operation.path);
        if path.is_empty() {
            continue;
        }
        affected_files.insert(path.clone());
        if let Some(parent) = Path::new(&path).parent() {
            affected_directories.insert(normalize_windows_path(&parent.to_string_lossy()));
        }
        if let Some(object_id) = operation.file_object_id.as_ref() {
            object_paths.insert(object_id.clone(), path);
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
    let _ = object_paths;
    score.clamp(0, 100) as u8
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
    fn temp_write_and_rename_is_completed() {
        let result = analyze_save_transactions(vec![
            operation("C:/save/slot.tmp", FileOperationKind::Write, Some(1_000)),
            operation("C:/save/slot.sav", FileOperationKind::Rename, Some(1_200)),
        ]);
        assert_eq!(result.status, "completed");
        assert!(result
            .affected_files
            .iter()
            .any(|path| path.ends_with("slot.sav")));
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
