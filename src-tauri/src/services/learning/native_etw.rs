use std::collections::HashMap;
use std::time::Instant;

use super::etw_capture::normalize_windows_path;
use super::etw_capture::{build_device_path_map, resolve_device_path, TraceCollectionResult};
use super::trace_evidence::{reads_direct_path, uses_file_object, FileEvidence, ProcessHistory, ProcessWindows};
use crate::domain::{
    learning::ProcessLifetime,
    timestamp::{FILETIME_TICKS_PER_MS, FILETIME_UNIX_EPOCH},
};

#[cfg(target_os = "windows")]
use windows_sys::Win32::{
    Foundation::ERROR_SUCCESS,
    System::Diagnostics::Etw::{
        CloseTrace, OpenTraceW, ProcessTrace, TdhGetProperty, TdhGetPropertySize, EVENT_RECORD,
        EVENT_TRACE_LOGFILEW, EVENT_TRACE_LOGFILEW_0, EVENT_TRACE_LOGFILEW_1,
        PEVENT_RECORD_CALLBACK, PROCESS_TRACE_MODE_EVENT_RECORD, PROPERTY_DATA_DESCRIPTOR,
    },
};

const KERNEL_FILE_PROVIDER: windows_sys::core::GUID =
    windows_sys::core::GUID::from_u128(0xedd08927_9cc4_4e65_b970_c2560fb5c289);
const KERNEL_PROCESS_PROVIDER: windows_sys::core::GUID =
    windows_sys::core::GUID::from_u128(0x22fb2cd6_0e7b_422b_a0c7_2fad1fd0e716);
const FILE_OBJECT_PROPERTY_NAMES: &[&str] = &["FileObject"];
const PATH_PROPERTY_NAMES: &[&str] = &["FileName", "FilePath", "OpenPath", "Path"];

#[cfg(target_os = "windows")]
struct NativeCollector<'a> {
    tracked: &'a ProcessWindows,
    device_paths: Vec<(String, String)>,
    evidence: FileEvidence,
    total_events: usize,
    matched_pid_events: usize,
    outside_lifetime_events: usize,
    task_prefiltered_events: usize,
    extracted_paths: usize,
    property_errors: usize,
    descriptor_samples: Vec<String>,
    path_property_by_schema: HashMap<u64, Option<usize>>,
    file_object_property_by_schema: HashMap<u64, Option<usize>>,
}

#[cfg(target_os = "windows")]
impl<'a> NativeCollector<'a> {
    fn new(tracked: &'a ProcessWindows) -> Self {
        Self {
            tracked,
            device_paths: build_device_path_map(),
            evidence: FileEvidence::default(),
            total_events: 0,
            matched_pid_events: 0,
            outside_lifetime_events: 0,
            task_prefiltered_events: 0,
            extracted_paths: 0,
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
        if !self.tracked.contains_pid(pid) {
            return;
        }
        if !self.tracked.contains(pid, event.EventHeader.TimeStamp) {
            self.outside_lifetime_events += 1;
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
        if !uses_file_object(operation_code) {
            self.task_prefiltered_events += 1;
            return;
        }

        let path_result = if reads_direct_path(operation_code) {
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
            Ok(value) => value.and_then(|bytes| format_file_object_id(&bytes)),
            Err(()) => {
                self.property_errors += 1;
                None
            }
        };
        self.evidence.record(
            operation_code, path, file_object_id, pid, filetime_to_unix_ms(event.EventHeader.TimeStamp),
        );
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
pub(super) fn collect_related_files_from_etl(
    etl_path: &str,
    tracked: &ProcessWindows,
) -> Result<TraceCollectionResult, String> {
    let started = Instant::now();
    let mut collector = NativeCollector::new(tracked);
    let health = read_trace(
        etl_path, (&mut collector as *mut NativeCollector).cast(), Some(event_record_callback),
    )?;
    if collector.total_events == 0 {
        return Err("native ETL contained no events".to_string());
    }
    if collector.matched_pid_events == 0 {
        return Err("native ETL contained no target process events".to_string());
    }
    if collector.extracted_paths == 0 {
        return Err("native ETL contained no decodable target paths".to_string());
    }
    let incomplete = health.incomplete() || collector.property_errors > 0;
    let mut tracked_pids: Vec<_> = tracked.pids().collect();
    tracked_pids.sort_unstable();
    use super::transactions::FileOperationKind;
    let logs = vec![
        "原生 ETL 解析完成（OpenTraceW + ProcessTrace + TDH）".to_string(),
        format!("设备路径映射：{} 个卷", collector.device_paths.len()),
        format!("目标 PID：{tracked_pids:?}"),
        format!("进程生命周期过滤：{} 条事件时间不匹配", collector.outside_lifetime_events),
        format!("事件描述符样本：{}", collector.descriptor_samples.join("；")),
        format!(
            "原生事件统计：总事件 {}，PID 命中 {}，Task 前置过滤 {}，路径提取 {}，路径忽略 {}",
            collector.total_events, collector.matched_pid_events, collector.task_prefiltered_events,
            collector.extracted_paths, collector.evidence.ignored_paths,
        ),
        format!(
            "写入相关事件：{}，最终去重文件：{}，活动文件对象：{}，对象路径回填：{}",
            collector.evidence.mutations, collector.evidence.files.len(),
            collector.evidence.path_count(), collector.evidence.resolved_paths,
        ),
        format!(
            "操作分类：创建 {}，写入 {}，重命名 {}，删除 {}，关闭 {}",
            collector.evidence.operations.iter().filter(|item| item.operation == FileOperationKind::Create).count(),
            collector.evidence.operations.iter().filter(|item| item.operation == FileOperationKind::Write).count(),
            collector.evidence.operations.iter().filter(|item| item.operation == FileOperationKind::Rename).count(),
            collector.evidence.operations.iter().filter(|item| item.operation == FileOperationKind::Delete).count(),
            collector.evidence.operations.iter().filter(|item| item.operation == FileOperationKind::Close).count(),
        ),
        format!("原生属性解码失败：{} 次", collector.property_errors),
        health.describe(),
        format!("性能：原生 ETL 文件解析 {} ms", started.elapsed().as_millis()),
    ];
    Ok(TraceCollectionResult {
        files: collector.evidence.files, logs, operations: collector.evidence.operations, incomplete,
    })
}

#[cfg(target_os = "windows")]
fn read_trace(
    etl_path: &str,
    context: *mut std::ffi::c_void,
    callback: PEVENT_RECORD_CALLBACK,
) -> Result<TraceHealth, String> {
    let mut etl_path_wide = etl_path.encode_utf16().collect::<Vec<_>>();
    etl_path_wide.push(0);
    let mut logfile = EVENT_TRACE_LOGFILEW::default();
    logfile.LogFileName = etl_path_wide.as_mut_ptr();
    logfile.Anonymous1 = EVENT_TRACE_LOGFILEW_0 {
        ProcessTraceMode: PROCESS_TRACE_MODE_EVENT_RECORD,
    };
    logfile.Anonymous2 = EVENT_TRACE_LOGFILEW_1 {
        EventRecordCallback: callback,
    };
    logfile.Context = context;
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
    let header = &logfile.LogfileHeader;
    Ok(TraceHealth {
        events_lost: unsafe { header.Anonymous2.Anonymous.EventsLost },
        buffers_lost: header.BuffersLost,
        finalized: header.EndTime > 0,
    })
}

struct TraceHealth {
    events_lost: u32,
    buffers_lost: u32,
    finalized: bool,
}

impl TraceHealth {
    fn incomplete(&self) -> bool {
        !self.finalized || self.events_lost > 0 || self.buffers_lost > 0
    }

    fn describe(&self) -> String {
        format!(
            "ETL 完整性：丢失事件 {}，丢失缓冲区 {}，已正常收尾 {}",
            self.events_lost, self.buffers_lost, self.finalized,
        )
    }
}

#[cfg(target_os = "windows")]
#[derive(Default)]
struct ProcessCollector {
    history: ProcessHistory,
    decode_failures: usize,
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn process_callback(event: *mut EVENT_RECORD) {
    let Some(event) = event.as_ref() else { return; };
    if !same_guid(&event.EventHeader.ProviderId, &KERNEL_PROCESS_PROVIDER) {
        return;
    }
    let code = event.EventHeader.EventDescriptor.Id;
    if !matches!(code, 1 | 2) {
        return;
    }
    let Some(collector) = event.UserContext.cast::<ProcessCollector>().as_mut() else { return; };
    let fields = (|| {
        let pid = u32::from_le_bytes(event_property_bytes_named(event, "ProcessID").ok()??.try_into().ok()?);
        let created = i64::from_le_bytes(event_property_bytes_named(event, "CreateTime").ok()??.try_into().ok()?);
        Some((pid, created))
    })();
    let Some(key) = fields else {
        collector.decode_failures += 1;
        return;
    };
    match code {
        1 => {
            if let Some(parent) = event_property_bytes_named(event, "ParentProcessID")
                .ok().flatten().and_then(|bytes| bytes.try_into().ok()).map(u32::from_le_bytes)
            {
                collector.history.starts.insert(key, parent);
            } else {
                collector.decode_failures += 1;
            }
        }
        2 => {
            if let Some(exited) = event_property_bytes_named(event, "ExitTime")
                .ok().flatten().and_then(|bytes| bytes.try_into().ok()).map(i64::from_le_bytes)
            {
                collector.history.stops.insert(key, exited);
            } else {
                collector.decode_failures += 1;
            }
        }
        _ => {}
    }
}

#[cfg(target_os = "windows")]
pub(super) fn collect_process_windows(
    etl_path: &str, tracked: &[ProcessLifetime], capture_end: i64,
) -> Result<(ProcessWindows, bool, String), String> {
    let started = Instant::now();
    let mut collector = ProcessCollector::default();
    let health = read_trace(
        etl_path, (&mut collector as *mut ProcessCollector).cast(), Some(process_callback),
    )?;
    let events = collector.history.starts.len() + collector.history.stops.len();
    let (windows, added) = collector.history.resolve(tracked, capture_end);
    let incomplete = events == 0 || collector.decode_failures > 0 || health.incomplete();
    let note = format!(
        "ETW 进程生命周期：记录 {}，补充短命子进程 {}，解码失败 {}，耗时 {} ms；{}",
        events, added, collector.decode_failures, started.elapsed().as_millis(), health.describe(),
    );
    Ok((windows, incomplete, note))
}

#[cfg(not(target_os = "windows"))]
pub(super) fn collect_process_windows(
    _etl_path: &str, _tracked: &[ProcessLifetime], _capture_end: i64,
) -> Result<(ProcessWindows, bool, String), String> {
    Err("native ETL parsing requires Windows".into())
}

#[cfg(not(target_os = "windows"))]
pub(super) fn collect_related_files_from_etl(
    _etl_path: &str,
    _tracked: &ProcessWindows,
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

fn format_file_object_id(bytes: &[u8]) -> Option<String> {
    if !matches!(bytes.len(), 4 | 8) {
        return None;
    }
    let mut raw = [0u8; 8];
    for (index, value) in bytes.iter().take(8).enumerate() {
        raw[index] = *value;
    }
    let address = u64::from_le_bytes(raw);
    (address != 0).then(|| format!("0x{address:016X}"))
}

fn filetime_to_unix_ms(value: i64) -> Option<i64> {
    (value > 100_000_000_000_000).then(|| (value - FILETIME_UNIX_EPOCH) / FILETIME_TICKS_PER_MS)
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
        decode_property_text, event_schema_key, filetime_to_unix_ms, format_file_object_id,
        kernel_file_operation_code, reads_direct_path, TraceHealth,
    };
    use super::super::trace_evidence::classify_kernel_operation;
    use crate::services::learning::transactions::FileOperationKind;

    #[cfg(target_os = "windows")]
    #[test]
    fn native_etl_filters_reused_pids_before_decoding_properties() {
        use super::{NativeCollector, ProcessLifetime, ProcessWindows, KERNEL_FILE_PROVIDER};
        use windows_sys::Win32::System::Diagnostics::Etw::EVENT_RECORD;

        let started_at = 134_313_202_837_027_063;
        let ended_at = started_at + 2;
        let lifetime = ProcessLifetime { pid: 42, started_at, ended_at };
        let tracked = ProcessWindows::from_tracked(std::slice::from_ref(&lifetime));
        let mut collector = NativeCollector::new(&tracked);
        let mut event = EVENT_RECORD::default();
        event.EventHeader.ProviderId = KERNEL_FILE_PROVIDER;
        event.EventHeader.ProcessId = lifetime.pid;
        event.EventHeader.EventDescriptor.Task = 15;
        for time in [started_at - 1, started_at, ended_at - 1, ended_at, ended_at + 1] {
            event.EventHeader.TimeStamp = time;
            unsafe { collector.consume(&event) };
        }
        assert_eq!(collector.matched_pid_events, 2);
        assert_eq!(collector.outside_lifetime_events, 3);
        assert_eq!(collector.task_prefiltered_events, 2);
        assert!(collector.evidence.operations.is_empty());
    }

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
            format_file_object_id(&0xFFFF_8000_0000_1234u64.to_le_bytes()).as_deref(),
            Some("0xFFFF800000001234")
        );
        assert_eq!(format_file_object_id(&[0; 8]), None);
        assert_eq!(format_file_object_id(&[0; 16]), None);
    }

    #[test]
    fn converts_filetime_to_unix_milliseconds() {
        assert_eq!(filetime_to_unix_ms(116_444_736_000_000_000), Some(0));
    }

    #[test]
    fn incomplete_trace_is_detected_from_header() {
        let mut health = TraceHealth { events_lost: 0, buffers_lost: 0, finalized: true };
        assert!(!health.incomplete());
        health.buffers_lost = 1;
        assert!(health.incomplete());
        health.buffers_lost = 0;
        health.events_lost = 1;
        assert!(health.incomplete());
        health.events_lost = 0;
        health.finalized = false;
        assert!(health.incomplete());
    }

    #[test]
    fn prefers_kernel_file_task_for_operation_classification() {
        assert_eq!(kernel_file_operation_code(27, 1, 0), 27);
        assert_eq!(kernel_file_operation_code(0, 16, 0), 16);
        assert_eq!(kernel_file_operation_code(0, 1, 21), 21);
    }

    #[test]
    fn decodes_paths_only_for_operations_that_carry_names() {
        assert!(reads_direct_path(12));
        assert!(reads_direct_path(27));
        assert!(reads_direct_path(30));
        assert!(!reads_direct_path(16));
        assert!(!reads_direct_path(14));
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
