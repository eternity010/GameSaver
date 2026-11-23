use std::{fs::File, io::Read, path::Path};

pub(crate) fn detect_unity_player_prefs(executable: &Path) -> Option<String> {
    let mut data_dir = executable.file_stem()?.to_os_string();
    data_dir.push("_Data");
    let info = executable.with_file_name(data_dir).join("app.info");
    let mut contents = String::new();
    File::open(info)
        .ok()?
        .take(8193)
        .read_to_string(&mut contents)
        .ok()?;
    let key = player_prefs_subkey(&contents)?;
    has_game_preferences(&key).then(|| format!(r"HKCU\{key}"))
}

fn player_prefs_subkey(info: &str) -> Option<String> {
    if info.len() > 8192 {
        return None;
    }
    let mut lines = info.trim_start_matches('\u{feff}').lines();
    let company = lines.next()?.trim();
    let product = lines.next()?.trim();
    for part in [company, product] {
        if part.is_empty()
            || matches!(part, "." | "..")
            || part.contains(['\\', '/'])
            || part.chars().any(char::is_control)
        {
            return None;
        }
    }
    Some(format!(r"Software\{company}\{product}"))
}

#[cfg(any(windows, test))]
fn is_game_preference(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    !name.is_empty()
        && !name.starts_with("screenmanager")
        && !name.starts_with("unity.")
        && !name.starts_with("unityselectmonitor")
        && !name.starts_with("unitygraphicsquality")
}

#[cfg(windows)]
fn has_game_preferences(key: &str) -> bool {
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::{
        Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS},
        System::Registry::{
            RegCloseKey, RegEnumValueW, RegOpenKeyExW, HKEY_CURRENT_USER, KEY_QUERY_VALUE,
        },
    };

    let wide_key = key.encode_utf16().chain([0]).collect::<Vec<_>>();
    let mut handle = null_mut();
    if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, wide_key.as_ptr(), 0, KEY_QUERY_VALUE, &mut handle) }
        != ERROR_SUCCESS
    {
        return false;
    }
    let mut found = false;
    let mut name = [0u16; 512];
    for index in 0..256 {
        let mut length = name.len() as u32;
        let status = unsafe {
            RegEnumValueW(
                handle,
                index,
                name.as_mut_ptr(),
                &mut length,
                null(),
                null_mut(),
                null_mut(),
                null_mut(),
            )
        };
        match status {
            ERROR_SUCCESS => {
                if name.get(..length as usize)
                    .is_some_and(|value| is_game_preference(&String::from_utf16_lossy(value)))
                {
                    found = true;
                    break;
                }
            }
            ERROR_MORE_DATA => continue,
            _ => break,
        }
    }
    unsafe {
        RegCloseKey(handle);
    }
    found
}

#[cfg(not(windows))]
fn has_game_preferences(_key: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::{is_game_preference, player_prefs_subkey};

    #[test]
    fn unity_metadata_can_only_select_its_exact_software_key() {
        assert_eq!(
            player_prefs_subkey("\u{feff}幻夢劇場シャルロッテ\r\nFour Nights at the Burger Shop\r\n").as_deref(),
            Some(r"Software\幻夢劇場シャルロッテ\Four Nights at the Burger Shop"),
        );
        for invalid in ["Studio", "Studio\n..\n", "Other\\Subkey\nGame", "Studio\nGame\0", "Studio\n \n"] {
            assert!(player_prefs_subkey(invalid).is_none());
        }
        assert!(player_prefs_subkey(&format!("{}\nGame", "a".repeat(8193))).is_none());
    }

    #[test]
    fn game_values_are_distinguished_from_unity_runtime_defaults() {
        assert!(is_game_preference("ClearedDay_h790065377"));
        assert!(!is_game_preference("Screenmanager Resolution Width_h182942802"));
        assert!(!is_game_preference("UnitySelectMonitor_h17969598"));
        assert!(!is_game_preference("unity.player_session_count_h922449978"));
    }
}
