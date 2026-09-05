use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use super::etw_capture::normalize_windows_path;
use super::etw_capture::should_ignore_snapshot_path;
use super::etw_capture::{
    build_device_path_map, resolve_device_path, should_skip_kernel_task, TraceCollectionResult,
};
use super::transactions::{FileOperation, FileOperationKind};

#[cfg(target_os = "windows")]
use windows_sys::Win32::{
    Foundation::ERROR_SUCCESS,
    System::Diagnostics::Etw::{
        CloseTrace, OpenTraceW, ProcessTrace, TdhGetProperty, TdhGetPropertySize, EVENT_RECORD,
        EVENT_TRACE_LOGFILEW, EVENT_TRACE_LOGFILEW_0, EVENT_TRACE_LOGFILEW_1,
        PROCESS_TRACE_MODE_EVENT_RECORD, PROPERTY_DATA_DESCRIPTOR,
    },
};

const KERNEL_FILE_PROVIDER: windows_sys::core::GUID =
    windows_sys::core::GUID::from_u128(0xedd08927_9cc4_4e65_b970_c2560fb5c289);
const FILE_OBJECT_PROPERTY_NAMES: &[&str] = &["FileObject", "FileKey"];
const PATH_PROPERTY_NAMES: &[&str] = &["FileName", "FilePath", "OpenPath", "Path"];

#[cfg(target_os = "windows")]
struct NativeCollector {
    tracked_pids: HashSet<u32>,
    device_paths: Vec<(String, String)>,
    files: HashSet<String>,
    operations: Vec<FileOperation>,
    file_object_paths: HashMap<String, String>,
    written_file_objects: HashSet<String>,
    total_events: usize,
    matched_pid_events: usize,
    task_prefiltered_events: usize,
    extracted_paths: usize,
    ignored_paths: usize,
    object_path_resolved: usize,
    property_errors: usize,
    descriptor_samples: Vec<String>,
    path_property_by_schema: HashMap<u64, Option<usize>>,
    file_object_property_by_schema: HashMap<u64, Option<usize>>,
}

#[cfg(target_os = "windows")]
impl NativeCollector {
    fn new(tracked_pids: &[u32]) -> Self {
        Self {
            tracked_pids: tracked_pids.iter().copied().collect(),
            device_paths: build_device_path_map(),
            files: HashSet::new(),
            operations: Vec::new(),
            file_object_paths: HashMap::new(),
            written_file_objects: HashSet::new(),
            total_events: 0,
            matched_pid_events: 0,
            task_prefiltered_events: 0,
            extracted_paths: 0,
            ignored_paths: 0,
            object_path_resolved: 0,
            property_errors: 0,
            descriptor_samples: Vec::new(),
            path_property_by_schema: HashMap::new(),
            file_object_property_by_schema: HashMap::new(),
        }
    }

    unsafe fn consume(&mut self, event: &EVENT_RECORD) {
        self.total_events += 1;
        if !same_guid(&event.EventHeader.ProviderId, &KERNEL_FILE_PROVIDER) {
            return;
        }
        let pid = event.EventHeader.ProcessId;
        if !self.tracked_pids.is_empty() && !self.tracked_pids.contains(&pid) {
            return;
        }
        self.matched_pid_events += 1;
        let descriptor = &event.EventHeader.EventDescriptor;
        let schema_key = event_schema_key(
            descriptor.Id,
            descriptor.Version,
            descriptor.Task,
            descriptor.Opcode,
        );
        let operation_code =
            kernel_file_operation_code(descriptor.Task, descriptor.Id, descriptor.Opcode);
        if self.descriptor_samples.len() < 3 {
            self.descriptor_samples.push(format!(
                "Id={}，Task={}，Opcode={}，采用={operation_code}",
                descriptor.Id, descriptor.Task, descriptor.Opcode
            ));
        }
        if should_skip_kernel_task(operation_code) {
            self.task_prefiltered_events += 1;
            return;
        }

        let operation = classify_kernel_operation(operation_code);
        let path_result = if operation.is_some_and(needs_direct_path) {
            event_property_text_cached(
                event,
                PATH_PROPERTY_NAMES,
                &mut self.path_property_by_schema,
                schema_key,
            )
        } else {
            Ok(None)
        };
        let path = match path_result {
            Ok(value) => value.filter(|value| !value.is_empty()).map(|value| {
                self.extracted_paths += 1;
                normalize_windows_path(&resolve_device_path(&value, &self.device_paths))
            }),
            Err(()) => {
                self.property_errors += 1;
                None
            }
        };
        let file_object_id = match event_property_bytes_cached(
            event,
            FILE_OBJECT_PROPERTY_NAMES,
            &mut self.file_object_property_by_schema,
            schema_key,
        ) {
            Ok(value) => value.map(|bytes| format_file_object_id(&bytes)),
            Err(()) => {
                self.property_errors += 1;
                None
            }
        };
        if let Some(path) = path.as_ref() {
            if should_ignore_snapshot_path(Path::new(path)) {
                self.ignored_paths += 1;
            } else if let Some(object_id) = file_object_id.as_ref() {
                self.file_object_paths
                    .insert(object_id.clone(), path.clone());
            }
        }
        let operation_path = path
            .clone()
            .filter(|path| !should_ignore_snapshot_path(Path::new(path)))
            .or_else(|| {
                file_object_id
                    .as_ref()
                    .and_then(|object_id| self.file_object_paths.get(object_id).cloned())
            });
        if path.is_none() && operation_path.is_some() {
            self.object_path_resolved += 1;
        }
        if let (Some(path), Some(operation)) = (operation_path.clone(), operation) {
            self.operations.push(FileOperation {
                path,
                operation,
                timestamp_ms: filetime_to_unix_ms(event.EventHeader.TimeStamp),
                pid,
                file_object_id: file_object_id.clone(),
            });
        }
        if is_write_related_task(operation_code) {
            if let Some(object_id) = file_object_id {
                self.written_file_objects.insert(object_id);
            }
            if let Some(path) = operation_path {
                self.files.insert(path);
            }
        }
    }
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn event_record_callback(event: *mut EVENT_RECORD) {
    let Some(event) = event.as_ref() else {
        return;
    };
    let collector = event.UserContext.cast::<NativeCollector>();
    if let Some(collector) = collector.as_mut() {
        collector.consume(event);
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn collect_related_files_from_etl(
    etl_path: &str,
    tracked_pids: &[u32],
) -> Result<TraceCollectionResult, String> {
    let started = Instant::now();
    let mut collector = NativeCollector::new(tracked_pids);
    let mut etl_path_wide = etl_path.encode_utf16().collect::<Vec<_>>();
    etl_path_wide.push(0);
    let mut logfile = EVENT_TRACE_LOGFILEW::default();
    logfile.LogFileName = etl_path_wide.as_mut_ptr();
    logfile.Anonymous1 = EVENT_TRACE_LOGFILEW_0 {
        ProcessTraceMode: PROCESS_TRACE_MODE_EVENT_RECORD,
    };
    logfile.Anonymous2 = EVENT_TRACE_LOGFILEW_1 {
        EventRecordCallback: Some(event_record_callback),
    };
    logfile.Context = (&mut collector as *mut NativeCollector).cast();
    let trace_handle = unsafe { OpenTraceW(&mut logfile) };
    if trace_handle.Value == u64::MAX {
        return Err("OpenTraceW failed".to_string());
    }
    let process_status =
        unsafe { ProcessTrace(&trace_handle, 1, std::ptr::null(), std::ptr::null()) };
    let close_status = unsafe { CloseTrace(trace_handle) };
    if process_status != ERROR_SUCCESS {
        return Err(format!(
            "ProcessTrace failed with Win32 error {process_status}"
        ));
    }
    if close_status != ERROR_SUCCESS {
        return Err(format!("CloseTrace failed with Win32 error {close_status}"));
    }
    if collector.total_events == 0 {
        return Err("native ETL contained no events".to_string());
    }
    if !collector.tracked_pids.is_empty() && collector.matched_pid_events == 0 {
        return Err("native ETL contained no target process events".to_string());
    }
    if collector.matched_pid_events > 0 && collector.extracted_paths == 0 {
        return Err("native ETL contained no decodable target paths".to_string());
    }
    if !collector.operations.is_empty()
        && collector
            .operations
            .iter()
            .all(|operation| operation.operation == FileOperationKind::Unknown)
    {
        return Err(format!(
            "native ETL operation classification failed ({})",
            collector.descriptor_samples.join("；")
        ));
    }
    for object_id in &collector.written_file_objects {
        if let Some(path) = collector.file_object_paths.get(object_id) {
            collector.files.insert(path.clone());
        }
    }
    let mut logs = vec![
        "原生 ETL 解析完成（OpenTraceW + ProcessTrace + TDH）".to_string(),
        format!("设备路径映射：{} 个卷", collector.device_paths.len()),
        format!("目标 PID：{tracked_pids:?}"),
        format!(
            "事件描述符样本：{}",
            collector.descriptor_samples.join("；")
        ),
        format!(
            "原生事件统计：总事件 {}，PID 命中 {}，Task 前置过滤 {}，路径提取 {}，路径忽略 {}",
            collector.total_events,
            collector.matched_pid_events,
            collector.task_prefiltered_events,
            collector.extracted_paths,
            collector.ignored_paths,
        ),
        format!(
            "文件对象关联：路径对象 {}，写入对象 {}",
            collector.file_object_paths.len(),
            collector.written_file_objects.len(),
        ),
        format!("最终去重文件：{}", collector.files.len()),
        format!("结构化文件操作：{} 条", collector.operations.len()),
        format!(
            "操作分类：创建 {}，写入 {}，重命名 {}，删除 {}，关闭 {}，未知 {}；对象路径回填 {}",
            collector
                .operations
                .iter()
                .filter(|item| item.operation == FileOperationKind::Create)
                .count(),
            collector
                .operations
                .iter()
                .filter(|item| item.operation == FileOperationKind::Write)
                .count(),
            collector
                .operations
                .iter()
                .filter(|item| item.operation == FileOperationKind::Rename)
                .count(),
            collector
                .operations
                .iter()
                .filter(|item| item.operation == FileOperationKind::Delete)
                .count(),
            collector
                .operations
                .iter()
                .filter(|item| item.operation == FileOperationKind::Close)
                .count(),
            collector
                .operations
                .iter()
                .filter(|item| item.operation == FileOperationKind::Unknown)
                .count(),
            collector.object_path_resolved,
        ),
        format!("原生属性解码失败：{} 次", collector.property_errors),
        format!(
            "TDH 属性缓存：路径结构 {}，文件对象结构 {}",
            collector.path_property_by_schema.len(),
            collector.file_object_property_by_schema.len()
        ),
        format!("性能：原生 ETL 解析 {} ms", started.elapsed().as_millis()),
    ];
    Ok(TraceCollectionResult {
        files: collector.files,
        logs: std::mem::take(&mut logs),
        operations: collector.operations,
    })
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn collect_related_files_from_etl(
    _etl_path: &str,
    _tracked_pids: &[u32],
) -> Result<TraceCollectionResult, String> {
    Err("native ETL parsing is only available on Windows".to_string())
}

#[cfg(target_os = "windows")]
unsafe fn event_property_text_cached(
    event: &EVENT_RECORD,
    names: &[&str],
    property_cache: &mut HashMap<u64, Option<usize>>,
    schema_key: u64,
) -> Result<Option<String>, ()> {
    event_property_bytes_cached(event, names, property_cache, schema_key).map(|value| {
        value
            .map(|bytes| decode_property_text(&bytes))
            .filter(|text| !text.is_empty())
    })
}

#[cfg(target_os = "windows")]
unsafe fn event_property_bytes_cached(
    event: &EVENT_RECORD,
    names: &[&str],
    property_cache: &mut HashMap<u64, Option<usize>>,
    schema_key: u64,
) -> Result<Option<Vec<u8>>, ()> {
    if let Some(property_index) = property_cache.get(&schema_key) {
        return match property_index {
            Some(index) => event_property_bytes_named(event, names[*index]),
            None => Ok(None),
        };
    }
    for (index, name) in names.iter().enumerate() {
        if let Some(value) = event_property_bytes_named(event, name)? {
            property_cache.insert(schema_key, Some(index));
            return Ok(Some(value));
        }
    }
    property_cache.insert(schema_key, None);
    Ok(None)
}

#[cfg(target_os = "windows")]
unsafe fn event_property_bytes_named(
    event: &EVENT_RECORD,
    name: &str,
) -> Result<Option<Vec<u8>>, ()> {
    let mut property_name = name.encode_utf16().collect::<Vec<_>>();
    property_name.push(0);
    let descriptor = PROPERTY_DATA_DESCRIPTOR {
        PropertyName: property_name.as_ptr() as u64,
        ArrayIndex: u32::MAX,
        Reserved: 0,
    };
    let mut size = 0u32;
    let size_status = TdhGetPropertySize(event, 0, std::ptr::null(), 1, &descriptor, &mut size);
    if size_status != ERROR_SUCCESS {
        return Ok(None);
    }
    if size == 0 {
        return Ok(Some(Vec::new()));
    }
    let mut bytes = vec![0u8; size as usize];
    let status = TdhGetProperty(
        event,
        0,
        std::ptr::null(),
        1,
        &descriptor,
        size,
        bytes.as_mut_ptr(),
    );
    if status != ERROR_SUCCESS {
        return Err(());
    }
    Ok(Some(bytes))
}

fn decode_property_text(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes.len() % 2 == 0 {
        let wide = bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .take_while(|value| *value != 0)
            .collect::<Vec<_>>();
        let text = String::from_utf16_lossy(&wide);
        if text.contains(':') || text.contains("\\\\Device\\") || text.contains('\\') {
            return text;
        }
    }
    String::from_utf8_lossy(bytes)
        .trim_end_matches('\0')
        .to_string()
}

fn format_file_object_id(bytes: &[u8]) -> String {
    let mut raw = [0u8; 8];
    for (index, value) in bytes.iter().take(8).enumerate() {
        raw[index] = *value;
    }
    format!("0x{:016X}", u64::from_le_bytes(raw))
}

fn filetime_to_unix_ms(value: i64) -> Option<i64> {
    const WINDOWS_TO_UNIX_EPOCH_MS: i64 = 11_644_473_600_000;
    (value > 100_000_000_000_000).then(|| value / 10_000 - WINDOWS_TO_UNIX_EPOCH_MS)
}

fn is_write_related_task(task: u32) -> bool {
    matches!(task, 16 | 21)
}

fn classify_kernel_operation(operation_code: u32) -> Option<FileOperationKind> {
    match operation_code {
        10 | 11 | 15 | 20 | 22 | 23 | 24 | 25 => None,
        12 | 30 => Some(FileOperationKind::Create),
        13 | 14 => Some(FileOperationKind::Close),
        16 | 21 => Some(FileOperationKind::Write),
        18 | 26 => Some(FileOperationKind::Delete),
        19 | 27 | 28 | 29 => Some(FileOperationKind::Rename),
        _ => Some(FileOperationKind::Unknown),
    }
}

fn needs_direct_path(operation: FileOperationKind) -> bool {
    matches!(
        operation,
        FileOperationKind::Create
            | FileOperationKind::Rename
            | FileOperationKind::Delete
            | FileOperationKind::Unknown
    )
}

fn kernel_file_operation_code(task: u16, event_id: u16, opcode: u8) -> u32 {
    [u32::from(task), u32::from(event_id), u32::from(opcode)]
        .into_iter()
        .find(|value| is_known_kernel_file_operation(*value))
        .unwrap_or(u32::from(task))
}

fn event_schema_key(event_id: u16, version: u8, task: u16, opcode: u8) -> u64 {
    (u64::from(event_id) << 32)
        | (u64::from(version) << 24)
        | (u64::from(task) << 8)
        | u64::from(opcode)
}

fn is_known_kernel_file_operation(value: u32) -> bool {
    matches!(value, 10..=30 | 32)
}

#[cfg(target_os = "windows")]
fn same_guid(left: &windows_sys::core::GUID, right: &windows_sys::core::GUID) -> bool {
    left.data1 == right.data1
        && left.data2 == right.data2
        && left.data3 == right.data3
        && left.data4 == right.data4
}

#[cfg(test)]
mod tests {
    use super::{
        classify_kernel_operation, decode_property_text, event_schema_key, filetime_to_unix_ms,
        format_file_object_id, is_write_related_task, kernel_file_operation_code,
        needs_direct_path,
    };
    use crate::services::learning::transactions::FileOperationKind;

    #[test]
    fn decodes_unicode_path_properties() {
        let path = r"C:\\Users\\eternity\\保存\\SaveData.xml";
        let bytes = path
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .chain([0, 0])
            .collect::<Vec<_>>();
        assert_eq!(decode_property_text(&bytes), path);
    }

    #[test]
    fn formats_file_object_identifiers_stably() {
        assert_eq!(
            format_file_object_id(&0xFFFF_8000_0000_1234u64.to_le_bytes()),
            "0xFFFF800000001234"
        );
    }

    #[test]
    fn converts_filetime_to_unix_milliseconds() {
        assert_eq!(filetime_to_unix_ms(116_444_736_000_000_000), Some(0));
    }

    #[test]
    fn keeps_only_mutating_file_tasks() {
        assert!(is_write_related_task(16));
        assert!(is_write_related_task(21));
        assert!(!is_write_related_task(15));
    }

    #[test]
    fn prefers_kernel_file_task_for_operation_classification() {
        assert_eq!(kernel_file_operation_code(27, 1, 0), 27);
        assert_eq!(kernel_file_operation_code(0, 16, 0), 16);
        assert_eq!(kernel_file_operation_code(0, 1, 21), 21);
    }

    #[test]
    fn decodes_paths_only_for_operations_that_carry_names() {
        assert!(needs_direct_path(FileOperationKind::Create));
        assert!(needs_direct_path(FileOperationKind::Rename));
        assert!(!needs_direct_path(FileOperationKind::Write));
        assert!(!needs_direct_path(FileOperationKind::Close));
        assert!(matches!(
            classify_kernel_operation(16),
            Some(FileOperationKind::Write)
        ));
    }

    #[test]
    fn separates_property_cache_entries_by_event_schema() {
        assert_ne!(
            event_schema_key(12, 1, 12, 0),
            event_schema_key(12, 2, 12, 0)
        );
        assert_ne!(
            event_schema_key(12, 1, 12, 0),
            event_schema_key(13, 1, 13, 0)
        );
    }
}
