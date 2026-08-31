use crate::{
    domain::{Game, GameBodyVersion},
    services::{BaiduNetdiskClient, RemoteFile},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::Path,
};
use uuid::Uuid;

const MANIFEST_VERSION: u32 = 1;
const MANIFEST_FILE_NAME: &str = "manifest.json";
const CATALOG_FILE_NAME: &str = "game.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudBodyManifest {
    pub format_version: u32,
    #[serde(default)]
    pub game_key: String,
    pub game_uid: String,
    pub updated_at: String,
    pub versions: Vec<CloudBodyManifestVersion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudGameCatalog {
    pub format_version: u32,
    #[serde(default)]
    pub game_key: String,
    pub game_uid: String,
    pub display_name: String,
    pub executable_relative_path: String,
    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default)]
    pub working_directory_relative_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudBodyManifestVersion {
    pub version_id: String,
    pub created_at: String,
    pub package_path: String,
    pub package_fs_id: u64,
    pub package_size: u64,
    pub package_sha256: Option<String>,
    pub file_count: usize,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteBodyPackage {
    pub version_id: String,
    pub path: String,
    pub fs_id: u64,
    pub size: u64,
    pub md5: Option<String>,
    pub is_dir: bool,
    pub server_mtime: Option<u64>,
    pub package_sha256: Option<String>,
    pub file_count: Option<usize>,
    pub total_bytes: Option<u64>,
    pub created_at: Option<String>,
    pub sync_state: String,
    pub manifest_verified: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteBodyPackageList {
    pub packages: Vec<RemoteBodyPackage>,
    pub manifest_available: bool,
    pub manifest_status: String,
    pub manifest_updated_at: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudGameSummary {
    pub game_key: String,
    pub game_uid: String,
    pub display_name: String,
    pub executable_relative_path: Option<String>,
    pub arguments: Vec<String>,
    pub working_directory_relative_path: Option<String>,
    pub version_id: String,
    pub package_path: String,
    pub package_fs_id: u64,
    pub package_size: u64,
    pub package_sha256: Option<String>,
    pub file_count: Option<usize>,
    pub total_bytes: Option<u64>,
    pub created_at: Option<String>,
    pub installed: bool,
    #[serde(default)]
    pub versions: Vec<RemoteBodyPackage>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudGamePage {
    pub games: Vec<CloudGameSummary>,
    pub page: usize,
    pub page_size: usize,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedEntry<T> {
    pub fs_id: u64,
    pub size: u64,
    pub server_mtime: Option<u64>,
    pub data: T,
}

pub struct CloudManifestService;

impl CloudManifestService {
    pub fn manifest_path(remote_dir: &str) -> String {
        format!("{remote_dir}/{MANIFEST_FILE_NAME}")
    }

    pub fn catalog_path(remote_dir: &str) -> String {
        format!("{remote_dir}/{CATALOG_FILE_NAME}")
    }

    pub fn cache_folder_name(remote_dir: &str) -> String {
        remote_dir
            .replace('/', "_")
            .replace('\\', "_")
            .replace(':', "_")
            .trim_matches('_')
            .to_string()
    }

    pub fn load_cached_catalog(
        cache_root: &Path,
        remote_dir: &str,
        remote: &RemoteFile,
    ) -> Option<CloudGameCatalog> {
        let path = cache_root
            .join(Self::cache_folder_name(remote_dir))
            .join("catalog.cache.json");
        let bytes = fs::read(path).ok()?;
        let cached = serde_json::from_slice::<CachedEntry<CloudGameCatalog>>(&bytes).ok()?;
        if cached.fs_id == remote.fs_id
            && cached.size == remote.size
            && cached.server_mtime == remote.server_mtime
            && validate_catalog(&cached.data, remote_dir).is_ok()
        {
            Some(cached.data)
        } else {
            None
        }
    }

    pub fn save_cached_catalog(
        cache_root: &Path,
        remote_dir: &str,
        remote: &RemoteFile,
        data: &CloudGameCatalog,
    ) -> Result<(), String> {
        let dir = cache_root.join(Self::cache_folder_name(remote_dir));
        fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        let path = dir.join("catalog.cache.json");
        let entry = CachedEntry {
            fs_id: remote.fs_id,
            size: remote.size,
            server_mtime: remote.server_mtime,
            data: data.clone(),
        };
        let bytes = serde_json::to_vec(&entry).map_err(|error| error.to_string())?;
        fs::write(path, bytes).map_err(|error| error.to_string())
    }

    pub fn load_cached_manifest(
        cache_root: &Path,
        remote_dir: &str,
        remote: &RemoteFile,
    ) -> Option<CloudBodyManifest> {
        let path = cache_root
            .join(Self::cache_folder_name(remote_dir))
            .join("manifest.cache.json");
        let bytes = fs::read(path).ok()?;
        let cached = serde_json::from_slice::<CachedEntry<CloudBodyManifest>>(&bytes).ok()?;
        if cached.fs_id == remote.fs_id
            && cached.size == remote.size
            && cached.server_mtime == remote.server_mtime
            && validate(&cached.data, remote_dir).is_ok()
        {
            Some(cached.data)
        } else {
            None
        }
    }

    pub fn save_cached_manifest(
        cache_root: &Path,
        remote_dir: &str,
        remote: &RemoteFile,
        data: &CloudBodyManifest,
    ) -> Result<(), String> {
        let dir = cache_root.join(Self::cache_folder_name(remote_dir));
        fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        let path = dir.join("manifest.cache.json");
        let entry = CachedEntry {
            fs_id: remote.fs_id,
            size: remote.size,
            server_mtime: remote.server_mtime,
            data: data.clone(),
        };
        let bytes = serde_json::to_vec(&entry).map_err(|error| error.to_string())?;
        fs::write(path, bytes).map_err(|error| error.to_string())
    }

    pub fn catalog_from_game(game: &Game) -> CloudGameCatalog {
        CloudGameCatalog {
            format_version: MANIFEST_VERSION,
            game_key: game.game_key.clone(),
            game_uid: game.game_uid.clone(),
            display_name: game.display_name.clone(),
            executable_relative_path: game.launch.executable_relative_path.clone(),
            arguments: game.launch.arguments.clone(),
            working_directory_relative_path: game.launch.working_directory_relative_path.clone(),
        }
    }

    pub fn write_catalog(
        client: &BaiduNetdiskClient,
        remote_dir: &str,
        catalog: &CloudGameCatalog,
        temporary_root: &Path,
    ) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(catalog)
            .map_err(|error| format!("序列化云端游戏信息失败：{error}"))?;
        fs::create_dir_all(temporary_root)
            .map_err(|error| format!("创建云端游戏信息临时目录失败：{error}"))?;
        let temporary = temporary_root.join(format!(".cloud-game-{}.json", Uuid::new_v4().simple()));
        let result = (|| -> Result<(), String> {
            let mut file = fs::File::create(&temporary)
                .map_err(|error| format!("创建云端游戏信息临时文件失败：{error}"))?;
            file.write_all(&bytes)
                .map_err(|error| format!("写入云端游戏信息临时文件失败：{error}"))?;
            file.sync_all()
                .map_err(|error| format!("刷新云端游戏信息临时文件失败：{error}"))?;
            client.upload_file(&temporary, &Self::catalog_path(remote_dir), |_, _| true)?;
            Ok(())
        })();
        let _ = fs::remove_file(&temporary);
        result
    }

    pub fn read_catalog(
        client: &BaiduNetdiskClient,
        remote_files: &[RemoteFile],
        remote_dir: &str,
        temporary_root: &Path,
        cache_root: Option<&Path>,
    ) -> Result<Option<CloudGameCatalog>, String> {
        let catalog_path = Self::catalog_path(remote_dir);
        let Some(remote_catalog) = remote_files
            .iter()
            .find(|file| file.path == catalog_path && !file.is_dir)
        else {
            return Ok(None);
        };
        if let Some(cache_root) = cache_root {
            if let Some(cached) = Self::load_cached_catalog(cache_root, remote_dir, remote_catalog) {
                return Ok(Some(cached));
            }
        }
        fs::create_dir_all(temporary_root)
            .map_err(|error| format!("创建云端游戏信息下载目录失败：{error}"))?;
        let temporary = temporary_root.join(format!(".cloud-game-download-{}.json", Uuid::new_v4().simple()));
        let result = (|| -> Result<CloudGameCatalog, String> {
            client.download_file(remote_catalog, &temporary, |_, _| true)?;
            let raw = fs::read(&temporary)
                .map_err(|error| format!("读取云端游戏信息失败：{error}"))?;
            let catalog = serde_json::from_slice::<CloudGameCatalog>(&raw)
                .map_err(|error| format!("解析云端游戏信息失败：{error}"))?;
            validate_catalog(&catalog, remote_dir)?;
            if let Some(cache_root) = cache_root {
                let _ = Self::save_cached_catalog(cache_root, remote_dir, remote_catalog, &catalog);
            }
            Ok(catalog)
        })();
        let _ = fs::remove_file(&temporary);
        let _ = fs::remove_file(temporary.with_extension("download.tmp"));
        result.map(Some)
    }

    pub fn build(
        game_key: &str,
        game_uid: &str,
        body_versions: &[GameBodyVersion],
        updated_at: String,
    ) -> CloudBodyManifest {
        let mut versions = body_versions
            .iter()
            .filter(|version| {
                version.game_uid == game_uid
                    && version.remote_path.as_deref().is_some_and(|path| !path.is_empty())
                    && version.remote_fs_id.is_some()
            })
            .map(|version| CloudBodyManifestVersion {
                version_id: version.version_id.clone(),
                created_at: version.created_at.clone(),
                package_path: version.remote_path.clone().unwrap_or_default(),
                package_fs_id: version.remote_fs_id.unwrap_or_default(),
                package_size: version.remote_size.unwrap_or(version.total_bytes),
                package_sha256: version.sha256.clone(),
                file_count: version.file_count,
                total_bytes: version.total_bytes,
            })
            .collect::<Vec<_>>();
        versions.sort_by(|left, right| left.version_id.cmp(&right.version_id));
        CloudBodyManifest {
            format_version: MANIFEST_VERSION,
            game_key: game_key.to_string(),
            game_uid: game_uid.to_string(),
            updated_at,
            versions,
        }
    }

    pub fn rebuild(
        client: &BaiduNetdiskClient,
        remote_dir: &str,
        game_key: &str,
        game_uid: &str,
        remote_files: &[RemoteFile],
        local_versions: &[GameBodyVersion],
        temporary_root: &Path,
    ) -> Result<CloudBodyManifest, String> {
        let existing = Self::read(client, remote_files, remote_dir, temporary_root, None)
            .ok()
            .flatten();
        let existing_by_path = existing
            .as_ref()
            .map(|manifest| {
                manifest
                    .versions
                    .iter()
                    .map(|version| (version.package_path.clone(), version))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        let local_manifest = Self::build(game_key, game_uid, local_versions, now_iso());
        let local_by_path = local_manifest
            .versions
            .iter()
            .map(|version| (version.package_path.clone(), version))
            .collect::<HashMap<_, _>>();
        let mut versions = remote_files
            .iter()
            .filter(|file| !file.is_dir && file.path.to_ascii_lowercase().ends_with(".zip"))
            .map(|file| {
                let local = local_by_path
                    .get(&file.path)
                    .and_then(|version| {
                        local_versions.iter().find(|local| {
                            local.version_id == version.version_id
                        })
                    })
                    .or_else(|| {
                        local_versions.iter().find(|version| {
                            version.version_id == file_name_without_extension(&file.path)
                        })
                    });
                let existing = existing_by_path.get(&file.path).copied();
                CloudBodyManifestVersion {
                    version_id: existing
                        .map(|version| version.version_id.clone())
                        .or_else(|| local.map(|version| version.version_id.clone()))
                        .unwrap_or_else(|| file_name_without_extension(&file.path)),
                    created_at: existing
                        .map(|version| version.created_at.clone())
                        .or_else(|| local.map(|version| version.created_at.clone()))
                        .unwrap_or_else(now_iso),
                    package_path: file.path.clone(),
                    package_fs_id: file.fs_id,
                    package_size: file.size,
                    package_sha256: existing
                        .and_then(|version| version.package_sha256.clone())
                        .or_else(|| local.and_then(|version| version.sha256.clone())),
                    file_count: existing
                        .map(|version| version.file_count)
                        .or_else(|| local.map(|version| version.file_count))
                        .unwrap_or_default(),
                    total_bytes: existing
                        .map(|version| version.total_bytes)
                        .or_else(|| local.map(|version| version.total_bytes))
                        .unwrap_or_default(),
                }
            })
            .collect::<Vec<_>>();
        versions.sort_by(|left, right| left.version_id.cmp(&right.version_id));
        let manifest = CloudBodyManifest {
            format_version: MANIFEST_VERSION,
            game_key: game_key.to_string(),
            game_uid: game_uid.to_string(),
            updated_at: now_iso(),
            versions,
        };
        Self::write_manifest(client, remote_dir, manifest.clone(), temporary_root)?;
        Ok(manifest)
    }

    fn write_manifest(
        client: &BaiduNetdiskClient,
        remote_dir: &str,
        manifest: CloudBodyManifest,
        temporary_root: &Path,
    ) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|error| format!("序列化云端本体版本清单失败：{error}"))?;
        fs::create_dir_all(temporary_root)
            .map_err(|error| format!("创建云端版本清单临时目录失败：{error}"))?;
        let temporary = temporary_root.join(format!(".cloud-manifest-{}.json", Uuid::new_v4().simple()));
        let result = (|| -> Result<(), String> {
            let mut file = fs::File::create(&temporary)
                .map_err(|error| format!("创建云端版本清单临时文件失败：{error}"))?;
            file.write_all(&bytes)
                .map_err(|error| format!("写入云端版本清单临时文件失败：{error}"))?;
            file.sync_all()
                .map_err(|error| format!("刷新云端版本清单临时文件失败：{error}"))?;
            client.upload_file(
                &temporary,
                &Self::manifest_path(remote_dir),
                |_, _| true,
            )?;
            Ok(())
        })();
        let _ = fs::remove_file(&temporary);
        result
    }

    pub fn read(
        client: &BaiduNetdiskClient,
        remote_files: &[RemoteFile],
        remote_dir: &str,
        temporary_root: &Path,
        cache_root: Option<&Path>,
    ) -> Result<Option<CloudBodyManifest>, String> {
        let manifest_path = Self::manifest_path(remote_dir);
        let Some(remote_manifest) = remote_files
            .iter()
            .find(|file| file.path == manifest_path && !file.is_dir)
        else {
            return Ok(None);
        };
        if let Some(cache_root) = cache_root {
            if let Some(cached) = Self::load_cached_manifest(cache_root, remote_dir, remote_manifest) {
                return Ok(Some(cached));
            }
        }
        fs::create_dir_all(temporary_root)
            .map_err(|error| format!("创建云端版本清单下载目录失败：{error}"))?;
        let temporary = temporary_root.join(format!(".cloud-manifest-download-{}.json", Uuid::new_v4().simple()));
        let result = (|| -> Result<CloudBodyManifest, String> {
            client.download_file(remote_manifest, &temporary, |_, _| true)?;
            let raw = fs::read(&temporary)
                .map_err(|error| format!("读取云端版本清单失败：{error}"))?;
            let manifest = serde_json::from_slice::<CloudBodyManifest>(&raw)
                .map_err(|error| format!("解析云端版本清单失败：{error}"))?;
            validate(&manifest, remote_dir)?;
            if let Some(cache_root) = cache_root {
                let _ = Self::save_cached_manifest(cache_root, remote_dir, remote_manifest, &manifest);
            }
            Ok(manifest)
        })();
        let _ = fs::remove_file(&temporary);
        let _ = fs::remove_file(temporary.with_extension("download.tmp"));
        result.map(Some)
    }

    pub fn project(
        remote_files: &[RemoteFile],
        manifest: Option<&CloudBodyManifest>,
        local_versions: &[GameBodyVersion],
    ) -> RemoteBodyPackageList {
        let manifest_versions = manifest
            .map(|value| {
                value
                    .versions
                    .iter()
                    .map(|version| (version.package_path.clone(), version))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        let mut packages = remote_files
            .iter()
            .filter(|file| !file.is_dir && file.path.to_ascii_lowercase().ends_with(".zip"))
            .map(|file| {
                let manifest_version = manifest_versions.get(&file.path).copied();
                let local_version = local_versions.iter().find(|version| {
                    version.remote_path.as_deref() == Some(file.path.as_str())
                        || manifest_version.is_some_and(|item| item.version_id == version.version_id)
                        || version.version_id == file_name_without_extension(&file.path)
                });
                let package_sha256 = manifest_version.and_then(|item| item.package_sha256.clone());
                let sync_state = if let Some(local) = local_version {
                    match (local.sha256.as_deref(), package_sha256.as_deref()) {
                        (Some(local_hash), Some(remote_hash)) if local_hash != remote_hash => {
                            "mismatch"
                        }
                        (Some(_), Some(_)) => "synced",
                        (_, Some(_)) if manifest_version.is_some() => "unverified",
                        (_, None) if manifest_version.is_some() => "unverified",
                        (_, None) if manifest.is_some() => "manifest_pending",
                        _ => "unverified",
                    }
                } else {
                    "remote_only"
                };
                RemoteBodyPackage {
                    version_id: manifest_version
                        .map(|item| item.version_id.clone())
                        .unwrap_or_else(|| file_name_without_extension(&file.path)),
                    path: file.path.clone(),
                    fs_id: file.fs_id,
                    size: file.size,
                    md5: file.md5.clone(),
                    is_dir: file.is_dir,
                    server_mtime: file.server_mtime,
                    package_sha256,
                    file_count: manifest_version.map(|item| item.file_count),
                    total_bytes: manifest_version.map(|item| item.total_bytes),
                    created_at: manifest_version.map(|item| item.created_at.clone()),
                    sync_state: sync_state.to_string(),
                    manifest_verified: manifest_version.is_some_and(|item| item.package_sha256.is_some()),
                }
            })
            .collect::<Vec<_>>();
        packages.sort_by(|left, right| right.path.cmp(&left.path));
        let mut warnings = Vec::new();
        let manifest_present = remote_files
            .iter()
            .any(|file| file.path.to_ascii_lowercase().ends_with("/manifest.json"));
        if manifest.is_none() && !packages.is_empty() {
            if manifest_present {
                warnings.push("云端版本清单无法读取，当前本体包等待重建清单。".to_string());
            } else {
                warnings.push("云端版本清单不存在，当前本体包只能按文件名识别。".to_string());
            }
        }
        if manifest.is_some() {
            for package in &packages {
                if package.sync_state == "manifest_pending" {
                    warnings.push(format!("云端本体包未登记在版本清单中：{}", package.version_id));
                }
            }
            for version in manifest
                .into_iter()
                .flat_map(|value| value.versions.iter())
            {
                if !remote_files.iter().any(|file| file.path == version.package_path) {
                    warnings.push(format!("版本清单记录的本体包不存在：{}", version.version_id));
                }
            }
        }
        for package in &packages {
            if package.sync_state == "mismatch" {
                warnings.push(format!("云端本体包与本地版本校验值不一致：{}", package.version_id));
            }
        }
        RemoteBodyPackageList {
            packages,
            manifest_available: manifest.is_some(),
            manifest_status: if manifest.is_some() {
                "synced".to_string()
            } else if manifest_present {
                "invalid".to_string()
            } else {
                "missing".to_string()
            },
            manifest_updated_at: manifest.map(|value| value.updated_at.clone()),
            warnings,
        }
    }
}

fn validate(manifest: &CloudBodyManifest, remote_dir: &str) -> Result<(), String> {
    if manifest.format_version != MANIFEST_VERSION {
        return Err(format!("不支持的云端版本清单格式：{}", manifest.format_version));
    }
    let expected_game_key = game_key_from_body_dir(remote_dir);
    if manifest.game_key != expected_game_key
        || manifest.game_key.trim().is_empty()
        || !is_valid_game_uid(&manifest.game_uid)
    {
        return Err("云端版本清单缺少游戏标识".to_string());
    }
    if manifest.versions.iter().any(|version| {
        version.version_id.trim().is_empty()
            || version.package_path.trim().is_empty()
            || version.package_fs_id == 0
            || !version.package_path.to_ascii_lowercase().ends_with(".zip")
    }) {
        return Err("云端版本清单包含无效本体包记录".to_string());
    }
    Ok(())
}

fn validate_catalog(catalog: &CloudGameCatalog, remote_dir: &str) -> Result<(), String> {
    let expected_game_key = game_key_from_body_dir(remote_dir);
    if catalog.format_version != MANIFEST_VERSION {
        return Err(format!("不支持的云端游戏信息格式：{}", catalog.format_version));
    }
    if catalog.game_key != expected_game_key || !is_valid_game_uid(&catalog.game_uid) {
        return Err("云端游戏信息不属于当前游戏".to_string());
    }
    if catalog.game_key.trim().is_empty()
        || catalog.display_name.trim().is_empty()
        || catalog.executable_relative_path.trim().is_empty()
    {
        return Err("云端游戏信息缺少启动配置".to_string());
    }
    Ok(())
}

fn game_key_from_body_dir(remote_dir: &str) -> &str {
    remote_dir
        .trim_end_matches('/')
        .strip_suffix("/body")
        .and_then(|parent| parent.rsplit('/').next())
        .filter(|value| !value.is_empty())
        .unwrap_or_default()
}

fn is_valid_game_uid(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn file_name_without_extension(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_string()
}

fn now_iso() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        game_key_from_body_dir, validate, CloudBodyManifest, CloudBodyManifestVersion,
        CloudGameCatalog, CloudManifestService,
    };
    use crate::{domain::GameBodyVersion, services::RemoteFile};
    use std::fs;
    use uuid::Uuid;

    fn version() -> GameBodyVersion {
        GameBodyVersion {
            version_id: "v1".to_string(),
            game_uid: "game-1".to_string(),
            created_at: "1".to_string(),
            archive_path: String::new(),
            file_count: 2,
            total_bytes: 20,
            package_path: Some("cache/v1.zip".to_string()),
            sha256: Some("abc".to_string()),
            excluded_items: Vec::new(),
            upload_status: Some("synced".to_string()),
            remote_path: Some("/apps/GameSaver/games/game-1/v1.zip".to_string()),
            remote_fs_id: Some(10),
            remote_size: Some(30),
        }
    }

    #[test]
    fn manifest_contains_only_remote_versions() {
        let local = version();
        let mut not_uploaded = version();
        not_uploaded.version_id = "v2".to_string();
        not_uploaded.remote_path = None;
        not_uploaded.remote_fs_id = None;
        let manifest = CloudManifestService::build("game one", "game-1", &[local.clone(), not_uploaded], "2".to_string());
        assert_eq!(manifest.versions.len(), 1);
        assert_eq!(manifest.versions[0].package_size, 30);
    }

    #[test]
    fn extracts_game_key_from_body_directory() {
        assert_eq!(
            game_key_from_body_dir("/apps/GameSaver/games/game-one/body"),
            "game-one"
        );
        assert_eq!(
            game_key_from_body_dir("/apps/GameSaver/games/game-one/body/"),
            "game-one"
        );
    }

    #[test]
    fn validates_manifest_using_game_key_and_remote_uid() {
        let manifest = CloudBodyManifest {
            format_version: 1,
            game_key: "game-one".to_string(),
            game_uid: "game-1".to_string(),
            updated_at: "2".to_string(),
            versions: Vec::new(),
        };
        assert!(validate(&manifest, "/apps/GameSaver/games/game-one/body").is_ok());
        assert!(validate(&manifest, "/apps/GameSaver/games/other-game/body").is_err());
    }

    #[test]
    fn project_marks_remote_only_and_checksum_mismatch() {
        let local = version();
        let manifest = CloudBodyManifest {
            format_version: 1,
            game_key: "game one".to_string(),
            game_uid: "game-1".to_string(),
            updated_at: "2".to_string(),
            versions: vec![CloudBodyManifestVersion {
                version_id: "v1".to_string(),
                created_at: "1".to_string(),
                package_path: "/apps/GameSaver/games/game-1/v1.zip".to_string(),
                package_fs_id: 10,
                package_size: 30,
                package_sha256: Some("different".to_string()),
                file_count: 2,
                total_bytes: 20,
            }],
        };
        let remote_files = vec![
            RemoteFile {
                path: "/apps/GameSaver/games/game-1/v1.zip".to_string(),
                fs_id: 10,
                size: 30,
                md5: None,
                is_dir: false,
                server_mtime: None,
            },
            RemoteFile {
                path: "/apps/GameSaver/games/game-1/v2.zip".to_string(),
                fs_id: 11,
                size: 40,
                md5: None,
                is_dir: false,
                server_mtime: None,
            },
        ];
        let result = CloudManifestService::project(&remote_files, Some(&manifest), &[local]);
        assert_eq!(result.packages[0].version_id, "v2");
        assert_eq!(result.packages[0].sync_state, "remote_only");
        assert_eq!(result.packages[1].version_id, "v1");
        assert_eq!(result.packages[1].sync_state, "mismatch");
        assert!(result.warnings.iter().any(|warning| warning.contains("v1")));
    }

    #[test]
    fn project_does_not_compare_remote_zip_with_local_directory_version() {
        let remote_path = "/apps/GameSaver/games/game-1/body/directory-version.zip";
        let mut local_directory = version();
        local_directory.version_id = "directory-version".to_string();
        local_directory.sha256 = None;
        local_directory.package_path = None;
        local_directory.archive_path = "E:/GameSaverGames/games/.versions/old".to_string();
        local_directory.remote_path = Some(remote_path.to_string());
        let remote = RemoteFile {
            path: remote_path.to_string(),
            fs_id: 12,
            size: 30,
            md5: None,
            is_dir: false,
            server_mtime: None,
        };
        let manifest = CloudBodyManifest {
            format_version: 1,
            game_key: "game one".to_string(),
            game_uid: "game-1".to_string(),
            updated_at: "2".to_string(),
            versions: vec![CloudBodyManifestVersion {
                version_id: "directory-version".to_string(),
                created_at: "2".to_string(),
                package_path: remote_path.to_string(),
                package_fs_id: remote.fs_id,
                package_size: remote.size,
                package_sha256: Some("remote-zip-hash".to_string()),
                file_count: 2,
                total_bytes: 20,
            }],
        };

        let result = CloudManifestService::project(&[remote], Some(&manifest), &[local_directory]);
        assert_eq!(result.packages[0].sync_state, "unverified");
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn cache_roundtrip_loads_matching_catalog_and_manifest() {
        let temp_dir = std::env::temp_dir().join(format!("cloud-manifest-cache-test-{}", Uuid::new_v4()));
        let remote_dir = "/apps/GameSaver/games/game-test/body";
        let remote_file = RemoteFile {
            path: format!("{remote_dir}/game.json"),
            fs_id: 101,
            size: 200,
            md5: None,
            is_dir: false,
            server_mtime: Some(1700000000),
        };
        let catalog = CloudGameCatalog {
            format_version: 1,
            game_key: "game-test".to_string(),
            game_uid: "uid-1".to_string(),
            display_name: "Test Game".to_string(),
            executable_relative_path: "game.exe".to_string(),
            arguments: vec!["--debug".to_string()],
            working_directory_relative_path: None,
        };

        CloudManifestService::save_cached_catalog(&temp_dir, remote_dir, &remote_file, &catalog)
            .expect("save cached catalog");

        let loaded = CloudManifestService::load_cached_catalog(&temp_dir, remote_dir, &remote_file);
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().display_name, "Test Game");

        // Mismatched fs_id should return None (cache miss)
        let mut modified_remote = remote_file.clone();
        modified_remote.fs_id = 102;
        assert!(CloudManifestService::load_cached_catalog(&temp_dir, remote_dir, &modified_remote).is_none());

        let _ = fs::remove_dir_all(temp_dir);
    }
}
