use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, Once, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
static LOG_PATH: OnceLock<Mutex<PathBuf>> = OnceLock::new();
static PANIC_HOOK: Once = Once::new();

pub fn init(data_dir: &Path) -> Result<(), String> {
    let log_dir = data_dir.join("logs");
    fs::create_dir_all(&log_dir).map_err(|error| format!("创建日志目录失败：{error}"))?;
    let path = log_dir.join("gamesaver.log");
    LOG_PATH
        .set(Mutex::new(path))
        .map_err(|_| "日志系统已初始化".to_string())?;
    info("日志系统已初始化");
    Ok(())
}

pub fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic| {
            error(format!("未处理的 Rust panic：{panic}"));
            previous(panic);
        }));
    });
}

pub fn info(message: impl AsRef<str>) {
    write_line("INFO", message.as_ref());
}

pub fn error(message: impl AsRef<str>) {
    write_line("ERROR", message.as_ref());
}

fn write_line(level: &str, message: &str) {
    let Some(path) = LOG_PATH.get() else {
        return;
    };
    let Ok(path) = path.lock() else {
        return;
    };
    let _ = rotate_if_needed(&path);
    let sanitized = truncate(message.replace('\r', " ").replace('\n', " "), 16_000);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or_default();
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&*path) {
        let _ = writeln!(file, "{timestamp} [{level}] {sanitized}");
    }
}

fn rotate_if_needed(path: &Path) -> Result<(), String> {
    let size = fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or_default();
    if size < MAX_LOG_BYTES {
        return Ok(());
    }
    let previous = path.with_extension("log.1");
    let older = path.with_extension("log.2");
    let _ = fs::remove_file(&older);
    let _ = fs::rename(&previous, &older);
    let _ = fs::rename(path, &previous);
    Ok(())
}

fn truncate(value: String, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value;
    }
    let mut truncated = value.chars().take(max_chars).collect::<String>();
    truncated.push_str("...<truncated>");
    truncated
}
