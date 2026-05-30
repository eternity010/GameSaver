use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::Path,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) fn now_unix() -> u64 {
    system_time_to_unix(SystemTime::now()).unwrap_or_default()
}

pub(crate) fn system_time_to_unix(value: SystemTime) -> Option<u64> {
    value
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

pub(crate) fn now_iso_string() -> String {
    now_unix().to_string()
}

pub(crate) fn apply_background_process_flags(command: &mut Command) -> &mut Command {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

pub(crate) fn iso_to_unix(value: &str) -> Option<u64> {
    value.parse::<u64>().ok()
}

pub(crate) fn file_sha256_hex(path: &Path) -> Result<String, String> {
    file_sha256_hex_with_context(path, "exe")
}

pub(crate) fn file_sha256_hex_with_context(path: &Path, context: &str) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|err| format!("read {context} failed: {err}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|err| format!("read {context} content failed: {err}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub(crate) fn restart_as_admin() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|err| format!("read current exe failed: {err}"))?;
    let exe_str = exe.to_string_lossy().to_string();
    let mut command = Command::new("powershell");
    command.args([
        "-NoProfile",
        "-Command",
        &format!("Start-Process -FilePath '{}' -Verb RunAs", exe_str.replace('\'', "''")),
    ]);
    let started = apply_background_process_flags(&mut command)
        .status()
        .map_err(|err| format!("request admin restart failed: {err}"))?;

    if !started.success() {
        return Err("admin restart request did not succeed".to_string());
    }

    std::process::exit(0);
}
