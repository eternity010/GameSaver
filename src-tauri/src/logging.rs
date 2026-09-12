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

pub fn warn(message: impl AsRef<str>) {
    write_line("WARN", message.as_ref());
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
    let collapsed = message.replace('\r', " ").replace('\n', " ");
    let sanitized = truncate(redact_secrets(&collapsed), 16_000);
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

/// 已知会承载凭据的键名（同时登记下划线与驼峰两种拼写）。
///
/// 日志是排障用的，留下「哪个请求的哪个参数出错」就够了；参数取值一旦是
/// access token / client_secret 这类长期凭据，写进磁盘就等于泄露。
const SENSITIVE_KEYS: [&str; 16] = [
    "access_token",
    "accesstoken",
    "refresh_token",
    "refreshtoken",
    "client_secret",
    "clientsecret",
    "client_id",
    "clientid",
    "secret_key",
    "secretkey",
    "app_key",
    "appkey",
    "password",
    "passwd",
    "token",
    "code",
];

const REDACTED_VALUE: &str = "<redacted>";

/// 把日志文本里敏感参数的取值替换掉，只保留键名与 URL 骨架。
///
/// 覆盖三类写法：查询串/表单的 `key=value`、JSON 的 `"key":"value"`、以及
/// `Authorization: Bearer <token>`。扫描按 ASCII 逐字节进行，多字节 UTF-8
/// 的字节都 >= 0x80，不会被当成 `=`、`&`、引号这些分隔符，因此切点始终落在
/// 字符边界上，输出仍是合法 UTF-8。
///
/// 放在 `write_line` 这一唯一出口上，是为了让「命令返回值 → 前端 → 日志」这条
/// 链路无论中间谁忘了处理，凭据都不会落盘。
fn redact_secrets(message: &str) -> String {
    let bytes = message.as_bytes();
    let mut output = String::with_capacity(message.len());
    let mut index = 0;
    while index < bytes.len() {
        if let Some(value_start) = sensitive_value_start(bytes, index) {
            let value_end = sensitive_value_end(bytes, value_start);
            if value_end > value_start {
                output.push_str(&message[index..value_start]);
                output.push_str(REDACTED_VALUE);
                index = value_end;
                continue;
            }
        }
        let character = message[index..]
            .chars()
            .next()
            .expect("index 始终落在字符边界上");
        output.push(character);
        index += character.len_utf8();
    }
    output
}

/// 若 `bytes[at..]` 是敏感键名或 `Bearer ` 前缀，返回其取值的起始下标。
///
/// 键名必须落在标识符边界上，否则 `error_code=110`、`code_verifier=...`
/// 这类「只是包含敏感词」的参数会被误伤。
fn sensitive_value_start(bytes: &[u8], at: usize) -> Option<usize> {
    if at > 0 && is_identifier_byte(bytes[at - 1]) {
        return None;
    }
    if starts_with_ignore_ascii_case(&bytes[at..], b"bearer ") {
        return Some(at + "bearer ".len());
    }
    for key in SENSITIVE_KEYS {
        let key_bytes = key.as_bytes();
        if !starts_with_ignore_ascii_case(&bytes[at..], key_bytes) {
            continue;
        }
        let mut cursor = at + key_bytes.len();
        // 键名可能被引号包裹（JSON）。
        if matches!(bytes.get(cursor), Some(&b'"') | Some(&b'\'')) {
            cursor += 1;
        }
        while bytes.get(cursor) == Some(&b' ') {
            cursor += 1;
        }
        if !matches!(bytes.get(cursor), Some(&b'=') | Some(&b':')) {
            continue;
        }
        cursor += 1;
        while bytes.get(cursor) == Some(&b' ') {
            cursor += 1;
        }
        // 值的起始引号留在输出里，只替换引号之间的内容。
        if matches!(bytes.get(cursor), Some(&b'"') | Some(&b'\'')) {
            cursor += 1;
        }
        return Some(cursor);
    }
    None
}

/// 取值的结束下标：遇到查询串、JSON 或错误文本里的分隔符就停。
fn sensitive_value_end(bytes: &[u8], start: usize) -> usize {
    let mut cursor = start;
    while let Some(&byte) = bytes.get(cursor) {
        if matches!(
            byte,
            b'&' | b' ' | b'\t' | b'"' | b'\'' | b')' | b',' | b'}' | b']' | b'>' | b'<' | b';'
        ) {
            break;
        }
        cursor += 1;
    }
    cursor
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn starts_with_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && haystack[..needle.len()].eq_ignore_ascii_case(needle)
}

#[cfg(test)]
mod tests {
    use super::{redact_secrets, SENSITIVE_KEYS};

    #[test]
    fn redacts_credential_values_but_keeps_the_url_skeleton() {
        let message = "读取百度网盘文件列表失败：error sending request for url (https://pan.baidu.com/rest/2.0/xpan/file?method=list&dir=/&access_token=123.abcdef.ghi)";
        let redacted = redact_secrets(message);
        assert!(
            !redacted.contains("123.abcdef.ghi"),
            "access token 取值不应留在日志里：{redacted}"
        );
        assert!(
            redacted.contains("access_token=<redacted>"),
            "键名应保留：{redacted}"
        );
        assert!(
            redacted.contains("https://pan.baidu.com/rest/2.0/xpan/file?method=list&dir=/"),
            "URL 骨架应保留：{redacted}"
        );
    }

    #[test]
    fn redacts_every_known_credential_key() {
        for key in SENSITIVE_KEYS {
            let message =
                format!("请求失败：https://example.com/api?{key}=SECRET-VALUE&other=keep");
            let redacted = redact_secrets(&message);
            assert!(
                !redacted.contains("SECRET-VALUE"),
                "{key} 的取值不应保留：{redacted}"
            );
            assert!(
                redacted.contains("other=keep"),
                "无关参数不应被牵连：{redacted}"
            );
        }
    }

    #[test]
    fn redacts_json_and_bearer_forms() {
        let json = redact_secrets(r#"{"accessToken":"SECRET-JSON","expiresAt":123}"#);
        assert!(!json.contains("SECRET-JSON"), "{json}");
        assert!(json.contains(r#""accessToken":"<redacted>""#), "{json}");
        assert!(json.contains("123"), "非敏感字段应保留：{json}");

        let bearer = redact_secrets("Authorization: Bearer SECRET-BEARER");
        assert!(!bearer.contains("SECRET-BEARER"), "{bearer}");
        assert!(bearer.contains("Bearer <redacted>"), "{bearer}");
    }

    #[test]
    fn keeps_parameters_that_only_look_like_credentials() {
        // `error_code` / `code_verifier` 只是包含敏感词，本身不是凭据。
        let message = "errno=-6 error_code=110 code_verifier=plain capture_id=abc session_id=xyz";
        assert_eq!(redact_secrets(message), message);
    }

    #[test]
    fn redaction_stays_on_character_boundaries() {
        let redacted = redact_secrets("失败：access_token=秘密令牌&备注=中文说明");
        assert_eq!(
            redacted, "失败：access_token=<redacted>&备注=中文说明",
            "中文必须原样保留"
        );
    }
}
