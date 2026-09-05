use crate::{
    domain::{
        game::{Game, LaunchConfig},
        AppStore, GameLifecycle, SaveRootType, SaveScope, UnknownFilePolicy,
    },
    services::{BaiduNetdiskClient, RemoteFile},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use uuid::Uuid;

const FORMAT_VERSION: u32 = 1;
const REMOTE_DIRECTORY: &str = "/apps/GameSaver/account";
const REMOTE_PROFILE_PATH: &str = "/apps/GameSaver/account/profile.json";
const MAX_PROFILE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudAccountProfile {
    pub format_version: u32,
    pub updated_at: String,
    pub games: Vec<CloudGameRecord>,
    pub save_profiles: Vec<CloudSaveProfile>,
    pub body_versions: Vec<CloudBodyVersionRecord>,
    pub settings: CloudPlatformSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudGameRecord {
    #[serde(default)]
    pub game_key: String,
    pub game_uid: String,
    pub display_name: String,
    pub launch: LaunchConfig,
    #[serde(default)]
    pub save_profile_id: Option<String>,
    #[serde(default)]
    pub last_played_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudSaveProfile {
    pub profile_id: String,
    #[serde(default)]
    pub game_key: String,
    pub game_uid: String,
    pub executable_hash: String,
    pub scopes: Vec<CloudSaveScope>,
    pub detection_evidence: Vec<String>,
    pub confidence: u8,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudSaveScope {
    pub root_type: SaveRootType,
    #[serde(default)]
    pub custom_root_path: Option<String>,
    #[serde(default)]
    pub sub_path: Option<String>,
    pub confirmed_files: Vec<String>,
    pub include_directories: Vec<String>,
    pub exclude_exact: Vec<String>,
    pub exclude_patterns: Vec<String>,
    pub exclude_directories: Vec<String>,
    pub unknown_file_policy: UnknownFilePolicy,
    pub max_file_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudBodyVersionRecord {
    pub version_id: String,
    #[serde(default)]
    pub game_key: String,
    pub game_uid: String,
    pub created_at: String,
    pub file_count: usize,
    pub total_bytes: u64,
    pub sha256: Option<String>,
    pub upload_status: Option<String>,
    pub remote_path: Option<String>,
    pub remote_fs_id: Option<u64>,
    pub remote_size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CloudPlatformSettings {
    pub auto_upload_body: bool,
}

pub struct CloudAccountService;

impl CloudAccountService {
    pub fn remote_directory() -> &'static str {
        REMOTE_DIRECTORY
    }

    pub fn remote_profile_path() -> &'static str {
        REMOTE_PROFILE_PATH
    }

    pub fn build(store: &AppStore, auto_upload_body: bool) -> CloudAccountProfile {
        let active_games = store
            .games
            .iter()
            .filter(|game| matches!(game.lifecycle, GameLifecycle::Active))
            .collect::<Vec<_>>();
        let active_ids = active_games
            .iter()
            .map(|game| game.game_uid.as_str())
            .collect::<HashSet<_>>();
        let games = active_games
            .iter()
            .map(|game| CloudGameRecord {
                game_key: game.game_key.clone(),
                game_uid: game.game_uid.clone(),
                display_name: game.display_name.clone(),
                launch: game.launch.clone(),
                save_profile_id: game.save_profile_id.clone(),
                last_played_at: game.last_played_at.clone(),
            })
            .collect();
        let save_profiles = store
            .save_profiles
            .iter()
            .filter(|profile| active_ids.contains(profile.game_uid.as_str()) && profile.enabled)
            .map(|profile| {
                let game = active_games
                    .iter()
                    .find(|game| game.game_uid == profile.game_uid)
                    .copied();
                CloudSaveProfile {
                    profile_id: profile.profile_id.clone(),
                    game_key: game.map(|g| g.game_key.clone()).unwrap_or_default(),
                    game_uid: profile.game_uid.clone(),
                    executable_hash: profile.executable_hash.clone(),
                    scopes: profile
                        .scopes
                        .iter()
                        .map(|scope| cloud_scope(scope, game))
                        .collect(),
                    detection_evidence: profile.detection_evidence.clone(),
                    confidence: profile.confidence,
                    updated_at: profile.updated_at.clone(),
                }
            })
            .collect();
        let body_versions = store
            .body_versions
            .iter()
            .filter(|version| {
                active_ids.contains(version.game_uid.as_str())
                    && version.remote_path.as_deref().is_some_and(|path| {
                        active_games
                            .iter()
                            .find(|game| game.game_uid == version.game_uid)
                            .is_some_and(|game| is_valid_remote_body_path(&game.game_key, path))
                    })
            })
            .map(|version| CloudBodyVersionRecord {
                version_id: version.version_id.clone(),
                game_key: active_games
                    .iter()
                    .find(|game| game.game_uid == version.game_uid)
                    .map(|game| game.game_key.clone())
                    .unwrap_or_default(),
                game_uid: version.game_uid.clone(),
                created_at: version.created_at.clone(),
                file_count: version.file_count,
                total_bytes: version.total_bytes,
                sha256: version.sha256.clone(),
                upload_status: version.upload_status.clone(),
                remote_path: version.remote_path.clone(),
                remote_fs_id: version.remote_fs_id,
                remote_size: version.remote_size,
            })
            .collect();
        CloudAccountProfile {
            format_version: FORMAT_VERSION,
            updated_at: now_iso(),
            games,
            save_profiles,
            body_versions,
            settings: CloudPlatformSettings { auto_upload_body },
        }
    }

    pub fn write(
        client: &BaiduNetdiskClient,
        profile: &CloudAccountProfile,
        temporary_root: &Path,
    ) -> Result<(), String> {
        validate(profile)?;
        let bytes = serde_json::to_vec_pretty(profile)
            .map_err(|error| format!("序列化云端账号清单失败：{error}"))?;
        if bytes.len() > MAX_PROFILE_BYTES {
            return Err("云端账号清单过大，拒绝上传".to_string());
        }
        fs::create_dir_all(temporary_root)
            .map_err(|error| format!("创建云端账号清单临时目录失败：{error}"))?;
        client.ensure_directory(REMOTE_DIRECTORY)?;
        let temporary =
            temporary_root.join(format!(".cloud-account-{}.json", Uuid::new_v4().simple()));
        let result = (|| -> Result<(), String> {
            let mut file = fs::File::create(&temporary)
                .map_err(|error| format!("创建云端账号清单临时文件失败：{error}"))?;
            file.write_all(&bytes)
                .map_err(|error| format!("写入云端账号清单临时文件失败：{error}"))?;
            file.sync_all()
                .map_err(|error| format!("刷新云端账号清单临时文件失败：{error}"))?;
            client.upload_file(&temporary, REMOTE_PROFILE_PATH, |_, _| true)?;
            Ok(())
        })();
        let _ = fs::remove_file(&temporary);
        result
    }

    pub fn read(
        client: &BaiduNetdiskClient,
        remote_files: &[RemoteFile],
        temporary_root: &Path,
    ) -> Result<Option<CloudAccountProfile>, String> {
        let Some(remote) = remote_files
            .iter()
            .find(|file| file.path == REMOTE_PROFILE_PATH && !file.is_dir)
        else {
            return Ok(None);
        };
        if remote.size > MAX_PROFILE_BYTES as u64 {
            return Err("云端账号清单超过大小限制，拒绝读取".to_string());
        }
        fs::create_dir_all(temporary_root)
            .map_err(|error| format!("创建云端账号清单下载目录失败：{error}"))?;
        let temporary = temporary_root.join(format!(
            ".cloud-account-download-{}.json",
            Uuid::new_v4().simple()
        ));
        let result = (|| -> Result<CloudAccountProfile, String> {
            client.download_file(remote, &temporary, |_, _| true)?;
            let raw =
                fs::read(&temporary).map_err(|error| format!("读取云端账号清单失败：{error}"))?;
            if raw.len() > MAX_PROFILE_BYTES {
                return Err("云端账号清单超过大小限制，拒绝解析".to_string());
            }
            let profile = serde_json::from_slice::<CloudAccountProfile>(&raw)
                .map_err(|error| format!("解析云端账号清单失败：{error}"))?;
            validate(&profile)?;
            Ok(profile)
        })();
        let _ = fs::remove_file(&temporary);
        let _ = fs::remove_file(temporary.with_extension("download.tmp"));
        result.map(Some)
    }
}

fn cloud_scope(scope: &SaveScope, game: Option<&Game>) -> CloudSaveScope {
    CloudSaveScope {
        root_type: scope.root_type,
        custom_root_path: (scope.root_type == SaveRootType::Custom)
            .then(|| scope.root_path.clone()),
        sub_path: extract_sub_path(scope, game),
        confirmed_files: scope.confirmed_files.clone(),
        include_directories: scope.include_directories.clone(),
        exclude_exact: scope.exclude_exact.clone(),
        exclude_patterns: scope.exclude_patterns.clone(),
        exclude_directories: scope.exclude_directories.clone(),
        unknown_file_policy: scope.unknown_file_policy,
        max_file_bytes: scope.max_file_bytes,
    }
}

pub fn extract_sub_path(scope: &SaveScope, game: Option<&Game>) -> Option<String> {
    if scope.root_type == SaveRootType::Custom {
        return None;
    }

    let norm_path = scope.root_path.trim().replace('/', "\\");

    if scope.root_type == SaveRootType::ManagedGame {
        if let Some(game) = game {
            let norm_managed = game.managed_path.trim().replace('/', "\\");
            if norm_path.eq_ignore_ascii_case(&norm_managed) {
                return None;
            }
            if norm_path
                .to_ascii_lowercase()
                .starts_with(&(norm_managed.to_ascii_lowercase() + "\\"))
            {
                let sub = &norm_path[norm_managed.len()..].trim_start_matches('\\');
                let sub_normalized = sub.replace('\\', "/");
                if is_valid_relative_path(&sub_normalized) {
                    return Some(sub_normalized);
                }
            }
            let game_folder = Path::new(&game.managed_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if !game_folder.is_empty() {
                let marker = format!("\\{game_folder}\\");
                if let Some(pos) = norm_path
                    .to_ascii_lowercase()
                    .rfind(&marker.to_ascii_lowercase())
                {
                    let sub = &norm_path[pos + marker.len()..].trim_start_matches('\\');
                    let sub_normalized = sub.replace('\\', "/");
                    if is_valid_relative_path(&sub_normalized) {
                        return Some(sub_normalized);
                    }
                }
            }
        }
        return None;
    }

    let env_base = match scope.root_type {
        SaveRootType::AppData => std::env::var_os("APPDATA").map(PathBuf::from),
        SaveRootType::LocalAppData => std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
        SaveRootType::LocalLow => std::env::var_os("USERPROFILE")
            .map(|p| PathBuf::from(p).join("AppData").join("LocalLow")),
        SaveRootType::Documents => {
            std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("Documents"))
        }
        SaveRootType::SavedGames => {
            std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("Saved Games"))
        }
        SaveRootType::UserProfile => std::env::var_os("USERPROFILE").map(PathBuf::from),
        SaveRootType::ManagedGame | SaveRootType::Custom => None,
    };

    if let Some(base) = env_base {
        let norm_base = base.to_string_lossy().replace('/', "\\");
        if norm_path.eq_ignore_ascii_case(&norm_base) {
            return None;
        }
        if norm_path
            .to_ascii_lowercase()
            .starts_with(&(norm_base.to_ascii_lowercase() + "\\"))
        {
            let sub = &norm_path[norm_base.len()..].trim_start_matches('\\');
            let sub_normalized = sub.replace('\\', "/");
            if is_valid_relative_path(&sub_normalized) {
                return Some(sub_normalized);
            }
        }
    }

    let marker = match scope.root_type {
        SaveRootType::LocalLow => Some(r"\appdata\locallow\"),
        SaveRootType::LocalAppData => Some(r"\appdata\local\"),
        SaveRootType::AppData => Some(r"\appdata\roaming\"),
        SaveRootType::Documents => Some(r"\documents\"),
        SaveRootType::SavedGames => Some(r"\saved games\"),
        _ => None,
    };

    if let Some(marker) = marker {
        if let Some(pos) = norm_path.to_ascii_lowercase().rfind(marker) {
            let sub = &norm_path[pos + marker.len()..].trim_start_matches('\\');
            let sub_normalized = sub.replace('\\', "/");
            if is_valid_relative_path(&sub_normalized) {
                return Some(sub_normalized);
            }
        }
    }

    None
}

fn is_valid_remote_body_path(game_key: &str, path: &str) -> bool {
    if !is_valid_game_key(game_key) {
        return false;
    }
    let prefix = format!("/apps/GameSaver/games/{game_key}/body/");
    let Some(name) = path.strip_prefix(&prefix) else {
        return false;
    };
    !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && name != "."
        && name != ".."
        && name.to_ascii_lowercase().ends_with(".zip")
}

fn is_valid_game_key(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
        && !value.chars().any(char::is_control)
}

fn is_valid_relative_path(value: &str) -> bool {
    let normalized = value.trim().replace('\\', "/");
    !normalized.is_empty()
        && !normalized.starts_with('/')
        && !normalized
            .as_bytes()
            .get(1)
            .is_some_and(|value| *value == b':')
        && !normalized.split('/').any(|component| component == "..")
}

fn is_valid_custom_root_path(value: &str) -> bool {
    let normalized = value.trim().replace('\\', "/");
    !normalized.is_empty()
        && (normalized.starts_with('/')
            || normalized
                .as_bytes()
                .get(1)
                .is_some_and(|value| *value == b':'))
}

fn is_valid_game_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn validate(profile: &CloudAccountProfile) -> Result<(), String> {
    if profile.format_version != FORMAT_VERSION {
        return Err(format!(
            "不支持的云端账号清单格式：{}",
            profile.format_version
        ));
    }
    let mut game_ids = HashSet::new();
    let mut game_keys = HashSet::new();
    for game in &profile.games {
        if !is_valid_game_id(game.game_uid.trim())
            || game.display_name.trim().is_empty()
            || game.launch.executable_relative_path.trim().is_empty()
            || !game_ids.insert(game.game_uid.as_str())
            || !is_valid_game_key(&game.game_key)
            || !game_keys.insert(game.game_key.as_str())
            || !is_valid_relative_path(&game.launch.executable_relative_path)
            || game
                .launch
                .working_directory_relative_path
                .as_deref()
                .is_some_and(|path| !is_valid_relative_path(path))
        {
            return Err("云端账号清单包含无效游戏记录".to_string());
        }
    }
    let mut profile_ids = HashSet::new();
    for save_profile in &profile.save_profiles {
        if save_profile.profile_id.trim().is_empty()
            || !game_ids.contains(save_profile.game_uid.as_str())
            || (!save_profile.game_key.trim().is_empty()
                && (!game_keys.contains(save_profile.game_key.as_str())
                    || profile
                        .games
                        .iter()
                        .find(|game| game.game_uid == save_profile.game_uid)
                        .is_none_or(|game| game.game_key != save_profile.game_key)))
            || !profile_ids.insert(save_profile.profile_id.as_str())
            || save_profile.scopes.is_empty()
        {
            return Err("云端账号清单包含无效存档配置".to_string());
        }
        for scope in &save_profile.scopes {
            if scope.root_type == SaveRootType::Custom
                && scope
                    .custom_root_path
                    .as_deref()
                    .is_none_or(|path| !is_valid_custom_root_path(path))
            {
                return Err("云端账号清单包含无效的自定义存档路径".to_string());
            }
            if let Some(sub_path) = scope.sub_path.as_deref() {
                if !is_valid_relative_path(sub_path) {
                    return Err("云端账号清单包含无效的存档范围子路径".to_string());
                }
            }
            if scope
                .confirmed_files
                .iter()
                .any(|path| !is_valid_relative_path(path))
                || scope
                    .include_directories
                    .iter()
                    .any(|path| !is_valid_relative_path(path))
                || scope
                    .exclude_exact
                    .iter()
                    .any(|path| !is_valid_relative_path(path))
                || scope
                    .exclude_directories
                    .iter()
                    .any(|path| !is_valid_relative_path(path))
            {
                return Err("云端账号清单包含无效的存档范围路径".to_string());
            }
        }
    }
    let mut version_ids = HashSet::new();
    for version in &profile.body_versions {
        if version.version_id.trim().is_empty()
            || !game_ids.contains(version.game_uid.as_str())
            || (!version.game_key.trim().is_empty()
                && (!game_keys.contains(version.game_key.as_str())
                    || profile
                        .games
                        .iter()
                        .find(|game| game.game_uid == version.game_uid)
                        .is_none_or(|game| game.game_key != version.game_key)))
            || version.file_count == 0
            || !version_ids.insert((version.game_uid.as_str(), version.version_id.as_str()))
            || version.remote_path.as_deref().is_none_or(|path| {
                profile
                    .games
                    .iter()
                    .find(|game| game.game_uid == version.game_uid)
                    .is_none_or(|game| !is_valid_remote_body_path(&game.game_key, path))
            })
        {
            return Err("云端账号清单包含无效本体版本".to_string());
        }
    }
    Ok(())
}

fn now_iso() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use super::{validate, CloudAccountService, CloudPlatformSettings};
    use crate::domain::{
        game::{Game, GameBodyVersion},
        AppStore, SaveProfile, SaveRootType, SaveScope, UnknownFilePolicy,
    };

    fn active_game(store: &mut AppStore) -> String {
        let mut game =
            Game::new_pending("Test Game", r"E:\GameSaverGames\games\game-1", "game.exe");
        game.activate("profile-1");
        let game_uid = game.game_uid.clone();
        store.games.push(game);
        game_uid
    }

    #[test]
    fn account_profile_excludes_non_active_games() {
        let store = AppStore::default();
        let profile = CloudAccountService::build(&store, true);
        assert!(profile.games.is_empty());
        assert!(profile.settings.auto_upload_body);
    }

    #[test]
    fn account_profile_excludes_unuploaded_body_versions() {
        let mut store = AppStore::default();
        let game_uid = active_game(&mut store);
        store.body_versions.push(GameBodyVersion {
            version_id: "local-only".to_string(),
            game_uid: game_uid.clone(),
            created_at: "1".to_string(),
            archive_path: String::new(),
            file_count: 1,
            total_bytes: 10,
            package_path: Some(r"E:\GameSaverGames\body-packages\local-only.zip".to_string()),
            sha256: None,
            excluded_items: Vec::new(),
            upload_status: Some("local_only".to_string()),
            remote_path: None,
            remote_fs_id: None,
            remote_size: None,
        });
        store.body_versions.push(GameBodyVersion {
            version_id: "remote-v1".to_string(),
            game_uid: game_uid.clone(),
            created_at: "2".to_string(),
            archive_path: String::new(),
            file_count: 1,
            total_bytes: 10,
            package_path: Some(r"E:\GameSaverGames\body-packages\remote-v1.zip".to_string()),
            sha256: Some("a".repeat(64)),
            excluded_items: Vec::new(),
            upload_status: Some("synced".to_string()),
            remote_path: Some(format!(
                "/apps/GameSaver/games/test game/body/remote-v1.zip"
            )),
            remote_fs_id: Some(42),
            remote_size: Some(10),
        });

        let profile = CloudAccountService::build(&store, false);
        assert_eq!(profile.body_versions.len(), 1);
        assert_eq!(profile.body_versions[0].version_id, "remote-v1");
        assert_eq!(profile.body_versions[0].game_key, "test game");
    }

    #[test]
    fn account_profile_carries_game_key_for_save_profile() {
        let mut store = AppStore::default();
        let game_uid = active_game(&mut store);
        store.save_profiles.push(SaveProfile::new(
            game_uid,
            "hash".to_string(),
            vec![SaveScope {
                root_type: SaveRootType::ManagedGame,
                root_path: "E:/GameSaverGames/games/game-1".to_string(),
                confirmed_files: vec!["save.dat".to_string()],
                include_directories: Vec::new(),
                exclude_exact: Vec::new(),
                exclude_patterns: Vec::new(),
                exclude_directories: Vec::new(),
                unknown_file_policy: UnknownFilePolicy::Protect,
                max_file_bytes: Some(10),
            }],
            90,
            "1".to_string(),
        ));

        let profile = CloudAccountService::build(&store, false);
        assert_eq!(profile.save_profiles.len(), 1);
        assert_eq!(profile.save_profiles[0].game_key, "test game");
    }

    #[test]
    fn account_profile_does_not_contain_managed_path_and_keeps_custom_path() {
        let mut store = AppStore::default();
        let game_uid = active_game(&mut store);
        let scope = SaveScope {
            root_type: SaveRootType::Custom,
            root_path: r"C:\Users\Player\Documents\My Saves".to_string(),
            confirmed_files: vec!["save.dat".to_string()],
            include_directories: Vec::new(),
            exclude_exact: Vec::new(),
            exclude_patterns: Vec::new(),
            exclude_directories: Vec::new(),
            unknown_file_policy: UnknownFilePolicy::Protect,
            max_file_bytes: Some(10),
        };
        store.save_profiles.push(SaveProfile::new(
            game_uid,
            "hash".to_string(),
            vec![scope],
            90,
            "1".to_string(),
        ));

        let profile = CloudAccountService::build(&store, false);
        let json = serde_json::to_string(&profile).expect("profile should serialize");
        assert!(!json.contains(r"E:\GameSaverGames\games\game-1"));
        assert_eq!(
            profile.save_profiles[0].scopes[0]
                .custom_root_path
                .as_deref(),
            Some(r"C:\Users\Player\Documents\My Saves")
        );
    }

    #[test]
    fn validate_rejects_invalid_launch_and_remote_package_paths() {
        let mut store = AppStore::default();
        let game_uid = active_game(&mut store);
        let mut profile = CloudAccountService::build(&store, false);
        profile.games[0].launch.executable_relative_path = r"..\outside.exe".to_string();
        assert!(validate(&profile).is_err());

        profile = CloudAccountService::build(&store, false);
        profile.body_versions.push(super::CloudBodyVersionRecord {
            version_id: "bad".to_string(),
            game_key: profile.games[0].game_key.clone(),
            game_uid,
            created_at: "1".to_string(),
            file_count: 1,
            total_bytes: 1,
            sha256: None,
            upload_status: Some("synced".to_string()),
            remote_path: Some("/apps/GameSaver/games/other/body/bad.zip".to_string()),
            remote_fs_id: Some(1),
            remote_size: Some(1),
        });
        assert!(validate(&profile).is_err());
    }

    #[test]
    fn platform_settings_has_safe_defaults() {
        assert!(!CloudPlatformSettings::default().auto_upload_body);
    }

    #[test]
    fn extract_sub_path_extracts_nested_relative_paths() {
        let game = Game::new_pending("Game1", r"E:\Games\Game1", "game.exe");

        // 1. LocalLow with subfolder
        let locallow_scope = SaveScope {
            root_type: SaveRootType::LocalLow,
            root_path: r"C:\Users\Alice\AppData\LocalLow\miHoYo\GenshinImpact".to_string(),
            confirmed_files: vec!["save.dat".to_string()],
            include_directories: Vec::new(),
            exclude_exact: Vec::new(),
            exclude_patterns: Vec::new(),
            exclude_directories: Vec::new(),
            unknown_file_policy: UnknownFilePolicy::Protect,
            max_file_bytes: None,
        };
        assert_eq!(
            super::extract_sub_path(&locallow_scope, Some(&game)),
            Some("miHoYo/GenshinImpact".to_string())
        );

        // 2. Documents with My Games
        let docs_scope = SaveScope {
            root_type: SaveRootType::Documents,
            root_path: r"C:\Users\Alice\Documents\My Games\Skyrim".to_string(),
            ..locallow_scope.clone()
        };
        assert_eq!(
            super::extract_sub_path(&docs_scope, Some(&game)),
            Some("My Games/Skyrim".to_string())
        );

        // 3. ManagedGame subdirectory
        let managed_sub_scope = SaveScope {
            root_type: SaveRootType::ManagedGame,
            root_path: r"E:\Games\Game1\SaveData\Slot1".to_string(),
            ..locallow_scope.clone()
        };
        assert_eq!(
            super::extract_sub_path(&managed_sub_scope, Some(&game)),
            Some("SaveData/Slot1".to_string())
        );

        // 4. ManagedGame exact root returns None
        let managed_exact_scope = SaveScope {
            root_type: SaveRootType::ManagedGame,
            root_path: r"E:\Games\Game1".to_string(),
            ..locallow_scope.clone()
        };
        assert_eq!(
            super::extract_sub_path(&managed_exact_scope, Some(&game)),
            None
        );

        // 5. Custom root returns None
        let custom_scope = SaveScope {
            root_type: SaveRootType::Custom,
            root_path: r"D:\CustomSaves\Game".to_string(),
            ..locallow_scope.clone()
        };
        assert_eq!(super::extract_sub_path(&custom_scope, Some(&game)), None);
    }

    #[test]
    fn validate_rejects_path_traversal_in_sub_path() {
        let mut store = AppStore::default();
        let _game_uid = active_game(&mut store);
        let mut profile = CloudAccountService::build(&store, false);
        profile.save_profiles.push(super::CloudSaveProfile {
            profile_id: "sp1".to_string(),
            game_key: profile.games[0].game_key.clone(),
            game_uid: profile.games[0].game_uid.clone(),
            executable_hash: "hash".to_string(),
            scopes: vec![super::CloudSaveScope {
                root_type: SaveRootType::LocalLow,
                custom_root_path: None,
                sub_path: Some("../../../Windows/System32".to_string()),
                confirmed_files: vec!["save.dat".to_string()],
                include_directories: Vec::new(),
                exclude_exact: Vec::new(),
                exclude_patterns: Vec::new(),
                exclude_directories: Vec::new(),
                unknown_file_policy: UnknownFilePolicy::Protect,
                max_file_bytes: None,
            }],
            detection_evidence: Vec::new(),
            confidence: 90,
            updated_at: "1".to_string(),
        });
        assert!(validate(&profile).is_err());
    }
}
