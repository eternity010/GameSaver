use crate::{
    domain::{Game, SaveFileEntry, SaveProfile, SaveVersion},
    repositories::{BaiduConfigRepository, GameRepository, SaveRepository},
    services::{BaiduNetdiskClient, RemoteFile},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Manager};
use uuid::Uuid;
use zip::{write::SimpleFileOptions, ZipArchive, ZipWriter};

const MANIFEST_VERSION: u32 = 1;
const REMOTE_SAVES_ROOT: &str = "/apps/GameSaver/saves";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudSaveManifest {
    pub format_version: u32,
    pub game_key: String,
    pub game_uid: String,
    pub updated_at: String,
    #[serde(default)]
    pub latest_version_id: Option<String>,
    pub versions: Vec<CloudSaveManifestVersion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudSaveManifestVersion {
    pub version_id: String,
    pub created_at: String,
    pub package_path: String,
    pub package_fs_id: u64,
    pub package_size: u64,
    #[serde(default)]
    pub package_sha256: Option<String>,
    pub file_count: usize,
    pub total_bytes: u64,
    #[serde(default)]
    pub device_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudSavePackageMeta {
    pub version: SaveVersion,
    pub game_key: String,
    pub display_name: String,
    pub created_at: String,
    #[serde(default)]
    pub device_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudSaveSyncStatusView {
    pub auto_sync_save: bool,
    pub local_version_count: usize,
    pub cloud_version_count: usize,
    pub latest_local_created_at: Option<String>,
    pub latest_cloud_created_at: Option<String>,
    pub latest_cloud_version_id: Option<String>,
    pub sync_state: String,
    pub warnings: Vec<String>,
}

pub struct CloudSaveService;

impl CloudSaveService {
    pub fn remote_save_dir(game_key: &str) -> String {
        format!("{REMOTE_SAVES_ROOT}/{game_key}")
    }

    pub fn remote_manifest_path(game_key: &str) -> String {
        format!("{REMOTE_SAVES_ROOT}/{game_key}/manifest.json")
    }

    pub fn remote_package_path(game_key: &str, version_id: &str) -> String {
        format!("{REMOTE_SAVES_ROOT}/{game_key}/{version_id}.zip")
    }

    pub fn package_save_version(
        app: &AppHandle,
        game: &Game,
        profile: &SaveProfile,
        version: &SaveVersion,
    ) -> Result<(PathBuf, String, u64), String> {
        let temp_dir = std::env::temp_dir();
        let zip_path = temp_dir.join(format!(
            "gamesaver-save-pkg-{}.zip",
            Uuid::new_v4().simple()
        ));
        let file = fs::File::create(&zip_path)
            .map_err(|err| format!("创建临时存档压缩文件失败：{err}"))?;
        let writer = BufWriter::with_capacity(512 * 1024, file);
        let mut zip = ZipWriter::new(writer);
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

        let device_name = hostname();
        let meta = CloudSavePackageMeta {
            version: version.clone(),
            game_key: game.game_key.clone(),
            display_name: game.display_name.clone(),
            created_at: version.created_at.clone(),
            device_name: Some(device_name),
        };

        let meta_json = serde_json::to_vec_pretty(&meta)
            .map_err(|err| format!("序列化存档元数据失败：{err}"))?;
        zip.start_file("meta.json", options)
            .map_err(|err| format!("写入 meta.json 失败：{err}"))?;
        zip.write_all(&meta_json)
            .map_err(|err| format!("写入 meta.json 内容失败：{err}"))?;

        for entry in version.files.iter().filter(|f| !f.deleted) {
            let bytes = if let Some(hash) = &entry.object_hash {
                SaveRepository::read_object(app, hash)
                    .or_else(|_| read_file_from_scope(game, profile, entry))?
            } else {
                read_file_from_scope(game, profile, entry)?
            };

            let zip_entry_name = format!("data/{}", entry.relative_path.trim_start_matches('/'));
            zip.start_file(zip_entry_name, options)
                .map_err(|err| format!("写入存档文件项失败：{err}"))?;
            zip.write_all(&bytes)
                .map_err(|err| format!("写入存档文件内容失败：{err}"))?;
        }

        let mut finished_writer = zip
            .finish()
            .map_err(|err| format!("完成存档压缩包打包失败：{err}"))?;
        finished_writer
            .flush()
            .map_err(|err| format!("刷新存档压缩包缓冲区失败：{err}"))?;
        drop(finished_writer);

        let size = fs::metadata(&zip_path)
            .map_err(|err| format!("获取存档压缩包大小失败：{err}"))?
            .len();
        let sha256 = sha256_file(&zip_path)?;

        Ok((zip_path, sha256, size))
    }

    pub fn upload_save_version(
        app: &AppHandle,
        client: &BaiduNetdiskClient,
        game: &Game,
        profile: &SaveProfile,
        version: &SaveVersion,
        keep_limit: usize,
        on_progress: impl Fn(u8, &str) -> bool + Sync,
    ) -> Result<CloudSaveManifestVersion, String> {
        on_progress(10, "正在打包本地存档");
        let (zip_path, sha256, size) = Self::package_save_version(app, game, profile, version)?;

        let remote_dir = Self::remote_save_dir(&game.game_key);
        let remote_pkg = Self::remote_package_path(&game.game_key, &version.version_id);

        let upload_result = (|| -> Result<RemoteFile, String> {
            client.ensure_directory(&remote_dir)?;
            on_progress(30, "正在上传存档包到百度网盘");
            client.upload_file(&zip_path, &remote_pkg, |pct, msg| {
                on_progress(30 + (pct as f32 * 0.5) as u8, msg)
            })
        })();

        let _ = fs::remove_file(&zip_path);
        let remote_file = upload_result?;

        on_progress(85, "正在更新云端存档清单");
        let mut manifest = Self::fetch_manifest(client, &game.game_key, &game.game_uid)?
            .unwrap_or_else(|| CloudSaveManifest {
                format_version: MANIFEST_VERSION,
                game_key: game.game_key.clone(),
                game_uid: game.game_uid.clone(),
                updated_at: now_iso(),
                latest_version_id: None,
                versions: Vec::new(),
            });

        let manifest_version = CloudSaveManifestVersion {
            version_id: version.version_id.clone(),
            created_at: version.created_at.clone(),
            package_path: remote_file.path.clone(),
            package_fs_id: remote_file.fs_id,
            package_size: size,
            package_sha256: Some(sha256),
            file_count: version.files.iter().filter(|f| !f.deleted).count(),
            total_bytes: version.total_bytes,
            device_name: Some(hostname()),
        };

        manifest
            .versions
            .retain(|v| v.version_id != version.version_id);
        manifest.versions.push(manifest_version.clone());
        manifest.versions.sort_by(|left, right| {
            let left_time = left.created_at.as_str();
            let right_time = right.created_at.as_str();
            right_time
                .cmp(left_time)
                .then_with(|| right.version_id.cmp(&left.version_id))
        });
        manifest.latest_version_id = manifest.versions.first().map(|v| v.version_id.clone());
        manifest.updated_at = now_iso();

        if manifest.versions.len() > keep_limit && keep_limit > 0 {
            let pruned = manifest.versions.split_off(keep_limit);
            for old_ver in pruned {
                let _ = client.delete_file(&old_ver.package_path);
            }
        }

        Self::save_manifest(client, &game.game_key, &manifest)?;
        on_progress(100, "存档云端同步完成");
        Ok(manifest_version)
    }

    pub fn download_and_import_save_version(
        app: &AppHandle,
        client: &BaiduNetdiskClient,
        game: &Game,
        remote_version: &CloudSaveManifestVersion,
        on_progress: impl Fn(u8, &str) -> bool,
    ) -> Result<SaveVersion, String> {
        let temp_dir = std::env::temp_dir();
        let target_zip =
            temp_dir.join(format!("gamesaver-save-dl-{}.zip", Uuid::new_v4().simple()));
        let _temporary_zip = TemporaryFileGuard(target_zip.clone());

        on_progress(15, "正在从百度网盘下载存档包");
        let remote_file = RemoteFile {
            path: remote_version.package_path.clone(),
            fs_id: remote_version.package_fs_id,
            size: remote_version.package_size,
            md5: None,
            is_dir: false,
            server_mtime: None,
        };

        let dl_result = client.download_file(&remote_file, &target_zip, |pct, msg| {
            on_progress(15 + (pct as f32 * 0.5) as u8, msg)
        });

        let downloaded_sha256 = match dl_result {
            Ok(hash) => hash,
            Err(err) => {
                let _ = fs::remove_file(&target_zip);
                return Err(err);
            }
        };

        if let Some(expected_hash) = remote_version.package_sha256.as_deref() {
            if !expected_hash.is_empty() && downloaded_sha256 != expected_hash {
                let _ = fs::remove_file(&target_zip);
                return Err("下载的存档压缩包完整性校验失败（SHA-256 不匹配）".to_string());
            }
        }

        on_progress(70, "正在校验并解压存档");
        let file = fs::File::open(&target_zip)
            .map_err(|err| format!("打开下载的存档压缩包失败：{err}"))?;
        let reader = BufReader::with_capacity(512 * 1024, file);
        let mut archive =
            ZipArchive::new(reader).map_err(|err| format!("解析下载的存档压缩包失败：{err}"))?;

        let meta = {
            let mut meta_file = archive
                .by_name("meta.json")
                .map_err(|err| format!("存档压缩包缺少 meta.json：{err}"))?;
            let mut meta_raw = Vec::new();
            meta_file
                .read_to_end(&mut meta_raw)
                .map_err(|err| format!("读取 meta.json 失败：{err}"))?;
            drop(meta_file);
            let meta: CloudSavePackageMeta = serde_json::from_slice(&meta_raw)
                .map_err(|err| format!("解析 meta.json 失败：{err}"))?;
            meta
        };
        let expected_entries = validate_downloaded_save_meta(&meta, game, remote_version)?;
        let total_entries = archive.len();

        let mut imported_version = meta.version;
        imported_version.game_uid = game.game_uid.clone();
        let mut imported_paths = HashSet::new();

        for i in 0..total_entries {
            let mut item = archive
                .by_index(i)
                .map_err(|err| format!("读取压缩项失败：{err}"))?;
            let name = item.name().to_string();
            if name.starts_with("data/") && !name.ends_with('/') {
                let relative = validate_package_data_path(&name)?;
                let (expected_hash, expected_size) = expected_entries
                    .get(&relative)
                    .ok_or_else(|| format!("存档压缩包包含未声明的文件：{relative}"))?;
                if !imported_paths.insert(relative.clone()) {
                    return Err(format!("存档压缩包包含重复文件：{relative}"));
                }
                let mut data = Vec::new();
                item.read_to_end(&mut data)
                    .map_err(|err| format!("解压文件项失败：{err}"))?;
                let hash = sha256_hex(&data);
                if data.len() as u64 != *expected_size || hash != *expected_hash {
                    return Err(format!("存档压缩包中的文件校验失败：{relative}"));
                }
                SaveRepository::write_object(app, &hash, &data)?;
            }
        }
        if imported_paths.len() != expected_entries.len() {
            return Err("存档压缩包缺少版本清单中的文件".to_string());
        }

        drop(archive);
        let _ = fs::remove_file(&target_zip);

        let state = app.state::<crate::app_state::AppState>();
        let mut store = state
            .store
            .lock()
            .map_err(|_| "锁定本地存储失败".to_string())?;
        if !store
            .save_versions
            .iter()
            .any(|v| v.version_id == imported_version.version_id && v.game_uid == game.game_uid)
        {
            store.save_versions.push(imported_version.clone());
            GameRepository::persist(app, &store)?;
        }

        on_progress(95, "存档已成功导入本地版本库");
        Ok(imported_version)
    }

    pub fn download_and_restore_cloud_save(
        app: &AppHandle,
        client: &BaiduNetdiskClient,
        game: &Game,
        profile: &SaveProfile,
        remote_version: &CloudSaveManifestVersion,
        on_progress: impl Fn(u8, &str) -> bool,
    ) -> Result<SaveVersion, String> {
        let imported = Self::download_and_import_save_version(
            app,
            client,
            game,
            remote_version,
            &on_progress,
        )?;
        on_progress(84, "正在保护当前本地存档");
        protect_current_save_version(app, game, profile)?;
        on_progress(90, "正在恢复物理存档文件");
        let receipt = SaveRepository::restore(app, game, profile, &imported, |pct, msg| {
            let _ = on_progress(90 + (pct as f32 * 0.1) as u8, msg);
        })?;
        let state = app.state::<crate::app_state::AppState>();
        let mut candidate = state
            .store
            .lock()
            .map_err(|_| "锁定本地存储失败".to_string())?
            .clone();
        let Some(game_record) = candidate
            .games
            .iter_mut()
            .find(|candidate_game| candidate_game.game_uid == game.game_uid)
        else {
            return match SaveRepository::rollback_restore(receipt) {
                Ok(()) => Err("游戏记录不存在，已回滚云端存档恢复".to_string()),
                Err(error) => Err(format!("游戏记录不存在，且云端存档回滚失败：{error}")),
            };
        };
        game_record.latest_save_version_id = Some(imported.version_id.clone());
        if let Err(error) = GameRepository::persist(app, &candidate) {
            return match SaveRepository::rollback_restore(receipt) {
                Ok(()) => Err(format!("保存云端存档恢复结果失败，已回滚存档：{error}")),
                Err(rollback_error) => Err(format!(
                    "保存云端存档恢复结果失败，且存档回滚失败：{error}；{rollback_error}"
                )),
            };
        }
        SaveRepository::finalize_restore(receipt);
        *state
            .store
            .lock()
            .map_err(|_| "锁定本地存储失败".to_string())? = candidate;
        on_progress(100, "云端存档已成功还原");
        Ok(imported)
    }

    pub fn fetch_manifest(
        client: &BaiduNetdiskClient,
        game_key: &str,
        game_uid: &str,
    ) -> Result<Option<CloudSaveManifest>, String> {
        let manifest_path = Self::remote_manifest_path(game_key);
        let temp_dir = std::env::temp_dir();
        let temp_file = temp_dir.join(format!(
            "save-manifest-fetch-{}.json",
            Uuid::new_v4().simple()
        ));

        let files = client
            .list(&Self::remote_save_dir(game_key))
            .unwrap_or_default();
        let remote_manifest_file = files.iter().find(|f| f.path == manifest_path);

        if let Some(mf) = remote_manifest_file {
            let dl_res = client.download_file(mf, &temp_file, |_, _| true);
            if dl_res.is_ok() {
                if let Ok(raw) = fs::read(&temp_file) {
                    let _ = fs::remove_file(&temp_file);
                    if let Ok(manifest) = serde_json::from_slice::<CloudSaveManifest>(&raw) {
                        return Ok(Some(manifest));
                    }
                }
            }
            let _ = fs::remove_file(&temp_file);
        }

        let zip_files = files
            .iter()
            .filter(|f| !f.is_dir && f.path.ends_with(".zip"))
            .collect::<Vec<_>>();
        if zip_files.is_empty() {
            return Ok(None);
        }

        let versions = zip_files
            .into_iter()
            .map(|f| {
                let v_id = Path::new(&f.path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                CloudSaveManifestVersion {
                    version_id: v_id,
                    created_at: f
                        .server_mtime
                        .map(|t| t.to_string())
                        .unwrap_or_else(now_iso),
                    package_path: f.path.clone(),
                    package_fs_id: f.fs_id,
                    package_size: f.size,
                    package_sha256: None,
                    file_count: 0,
                    total_bytes: f.size,
                    device_name: None,
                }
            })
            .collect::<Vec<_>>();

        Ok(Some(CloudSaveManifest {
            format_version: MANIFEST_VERSION,
            game_key: game_key.to_string(),
            game_uid: game_uid.to_string(),
            updated_at: now_iso(),
            latest_version_id: versions.first().map(|v| v.version_id.clone()),
            versions,
        }))
    }

    pub fn save_manifest(
        client: &BaiduNetdiskClient,
        game_key: &str,
        manifest: &CloudSaveManifest,
    ) -> Result<(), String> {
        let temp_dir = std::env::temp_dir();
        let temp_file = temp_dir.join(format!(
            "save-manifest-put-{}.json",
            Uuid::new_v4().simple()
        ));
        let raw = serde_json::to_vec_pretty(manifest)
            .map_err(|err| format!("序列化云端存档清单失败：{err}"))?;
        fs::write(&temp_file, &raw).map_err(|err| format!("写入临时云端存档清单失败：{err}"))?;

        let remote_manifest = Self::remote_manifest_path(game_key);
        let upload_res = client.upload_file(&temp_file, &remote_manifest, |_, _| true);
        let _ = fs::remove_file(&temp_file);
        upload_res.map(|_| ())
    }

    pub fn delete_cloud_version(
        client: &BaiduNetdiskClient,
        game_key: &str,
        game_uid: &str,
        version_id: &str,
    ) -> Result<CloudSaveManifest, String> {
        let remote_pkg = Self::remote_package_path(game_key, version_id);
        let _ = client.delete_file(&remote_pkg);

        let mut manifest = Self::fetch_manifest(client, game_key, game_uid)?.unwrap_or_else(|| {
            CloudSaveManifest {
                format_version: MANIFEST_VERSION,
                game_key: game_key.to_string(),
                game_uid: game_uid.to_string(),
                updated_at: now_iso(),
                latest_version_id: None,
                versions: Vec::new(),
            }
        });

        manifest.versions.retain(|v| v.version_id != version_id);
        manifest.latest_version_id = manifest.versions.first().map(|v| v.version_id.clone());
        manifest.updated_at = now_iso();
        Self::save_manifest(client, game_key, &manifest)?;
        Ok(manifest)
    }

    pub fn get_sync_status(
        app: &AppHandle,
        game: &Game,
    ) -> Result<CloudSaveSyncStatusView, String> {
        let data_dir = app_data_dir(app)?;
        let baidu_config = BaiduConfigRepository::load(&data_dir)?;
        let auto_sync_save = baidu_config
            .as_ref()
            .map(|c| c.auto_sync_save)
            .unwrap_or(true);

        let state = app.state::<crate::app_state::AppState>();
        let store = state
            .store
            .lock()
            .map_err(|_| "锁定本地存储失败".to_string())?;

        let mut local_versions = store
            .save_versions
            .iter()
            .filter(|v| v.game_uid == game.game_uid)
            .cloned()
            .collect::<Vec<_>>();
        local_versions.sort_by(|a, b| b.created_at.cmp(&a.created_at));

        let latest_local = local_versions.first();
        let latest_local_created_at = latest_local.map(|v| v.created_at.clone());

        let client = BaiduNetdiskClient::load_from_app_data(&data_dir);
        let Ok(client) = client else {
            return Ok(CloudSaveSyncStatusView {
                auto_sync_save,
                local_version_count: local_versions.len(),
                cloud_version_count: 0,
                latest_local_created_at,
                latest_cloud_created_at: None,
                latest_cloud_version_id: None,
                sync_state: "offline".to_string(),
                warnings: vec!["未连接或未授权百度网盘".to_string()],
            });
        };

        let manifest =
            Self::fetch_manifest(&client, &game.game_key, &game.game_uid).unwrap_or(None);
        let cloud_versions = manifest
            .as_ref()
            .map(|m| m.versions.clone())
            .unwrap_or_default();
        let latest_cloud = cloud_versions.first();
        let latest_cloud_created_at = latest_cloud.map(|v| v.created_at.clone());
        let latest_cloud_version_id = latest_cloud.map(|v| v.version_id.clone());

        let sync_state = match (latest_local, latest_cloud) {
            (None, None) => "synced",
            (Some(_), None) => "no_cloud_saves",
            (None, Some(_)) => "cloud_ahead",
            (Some(loc), Some(cld)) => {
                let loc_time = loc.created_at.parse::<u64>().unwrap_or(0);
                let cld_time = cld.created_at.parse::<u64>().unwrap_or(0);
                if loc.version_id == cld.version_id || loc_time == cld_time {
                    "synced"
                } else if loc_time > cld_time {
                    "local_ahead"
                } else {
                    "cloud_ahead"
                }
            }
        };

        Ok(CloudSaveSyncStatusView {
            auto_sync_save,
            local_version_count: local_versions.len(),
            cloud_version_count: cloud_versions.len(),
            latest_local_created_at,
            latest_cloud_created_at,
            latest_cloud_version_id,
            sync_state: sync_state.to_string(),
            warnings: Vec::new(),
        })
    }
}

fn validate_downloaded_save_meta(
    meta: &CloudSavePackageMeta,
    game: &Game,
    remote_version: &CloudSaveManifestVersion,
) -> Result<HashMap<String, (String, u64)>, String> {
    if meta.game_key != game.game_key {
        return Err("下载的存档压缩包不属于当前游戏".to_string());
    }
    if meta.version.version_id != remote_version.version_id {
        return Err("下载的存档压缩包版本与所选版本不一致".to_string());
    }

    let mut expected_entries = HashMap::new();
    for entry in meta.version.files.iter().filter(|entry| !entry.deleted) {
        let relative = normalize_package_relative_path(&entry.relative_path)?;
        let hash = entry
            .object_hash
            .as_deref()
            .filter(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| format!("存档版本缺少有效对象哈希：{relative}"))?
            .to_ascii_lowercase();
        if expected_entries
            .insert(relative.clone(), (hash, entry.size))
            .is_some()
        {
            return Err(format!("存档版本包含重复文件：{relative}"));
        }
    }
    if expected_entries.is_empty() {
        return Err("存档版本没有可恢复的文件".to_string());
    }
    Ok(expected_entries)
}

fn validate_package_data_path(name: &str) -> Result<String, String> {
    let relative = name
        .strip_prefix("data/")
        .ok_or_else(|| "存档压缩包文件路径无效".to_string())?;
    normalize_package_relative_path(relative)
}

fn normalize_package_relative_path(path: &str) -> Result<String, String> {
    let candidate = Path::new(path);
    if path.trim().is_empty()
        || candidate.is_absolute()
        || candidate.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(format!("存档压缩包包含不安全路径：{path}"));
    }
    Ok(path.replace('\\', "/").trim_start_matches('/').to_string())
}

struct TemporaryFileGuard(PathBuf);

impl Drop for TemporaryFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn protect_current_save_version(
    app: &AppHandle,
    game: &Game,
    profile: &SaveProfile,
) -> Result<(), String> {
    let state = app.state::<crate::app_state::AppState>();
    let latest = {
        let store = state
            .store
            .lock()
            .map_err(|_| "锁定本地存储失败".to_string())?;
        let latest_id = store
            .games
            .iter()
            .find(|candidate_game| candidate_game.game_uid == game.game_uid)
            .and_then(|candidate_game| candidate_game.latest_save_version_id.as_ref());
        latest_id.and_then(|version_id| {
            store
                .save_versions
                .iter()
                .find(|version| &version.version_id == version_id)
                .cloned()
        })
    };
    let protected = SaveRepository::commit(app, game, profile, latest.as_ref(), |_, _| {})?;
    let Some(protected) = protected else {
        return Ok(());
    };

    let pending = protected.clone();
    let result = (|| -> Result<(), String> {
        let mut candidate = state
            .store
            .lock()
            .map_err(|_| "锁定本地存储失败".to_string())?
            .clone();
        let protected_id = protected.version_id.clone();
        candidate.save_versions.push(protected);
        let game_record = candidate
            .games
            .iter_mut()
            .find(|candidate_game| candidate_game.game_uid == game.game_uid)
            .ok_or_else(|| "游戏记录不存在".to_string())?;
        game_record.latest_save_version_id = Some(protected_id);
        GameRepository::persist(app, &candidate)?;
        *state
            .store
            .lock()
            .map_err(|_| "锁定本地存储失败".to_string())? = candidate;
        Ok(())
    })();
    crate::repositories::release_pending_objects(&pending);
    result
}

fn read_file_from_scope(
    game: &Game,
    profile: &SaveProfile,
    entry: &SaveFileEntry,
) -> Result<Vec<u8>, String> {
    let scope = profile
        .scopes
        .iter()
        .find(|s| s.root_type == entry.root_type)
        .ok_or_else(|| format!("未找到匹配的作用域：{:?}", entry.root_type))?;
    let root = crate::repositories::save_repository::scope_root(game, scope);
    let file_path = root.join(&entry.relative_path);
    fs::read(&file_path)
        .map_err(|err| format!("读取物理存档文件失败：{} ({err})", file_path.display()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|err| format!("读取存档压缩包失败：{err}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 512 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|err| format!("计算存档压缩包哈希失败：{err}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "Windows Device".to_string())
}

fn now_iso() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

fn app_data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|err| format!("解析应用数据目录失败：{err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote_version(version_id: &str) -> CloudSaveManifestVersion {
        CloudSaveManifestVersion {
            version_id: version_id.to_string(),
            created_at: "0".to_string(),
            package_path: "/apps/GameSaver/saves/test-game/version.zip".to_string(),
            package_fs_id: 1,
            package_size: 1,
            package_sha256: None,
            file_count: 1,
            total_bytes: 1,
            device_name: None,
        }
    }

    fn package_meta(game_key: &str, version_id: &str) -> CloudSavePackageMeta {
        CloudSavePackageMeta {
            version: SaveVersion {
                version_id: version_id.to_string(),
                game_uid: "source-game".to_string(),
                created_at: "0".to_string(),
                files: vec![SaveFileEntry {
                    root_type: crate::domain::SaveRootType::LocalLow,
                    root_path: Some(r"C:\Users\Source\AppData\LocalLow\Studio\Game".to_string()),
                    relative_path: "SaveData/slot1.sav".to_string(),
                    object_hash: Some("a".repeat(64)),
                    size: 1,
                    deleted: false,
                    mtime_ms: None,
                }],
                total_bytes: 1,
            },
            game_key: game_key.to_string(),
            display_name: "Test Game".to_string(),
            created_at: "0".to_string(),
            device_name: None,
        }
    }

    #[test]
    fn builds_expected_remote_paths() {
        assert_eq!(
            CloudSaveService::remote_save_dir("game_test"),
            "/apps/GameSaver/saves/game_test"
        );
        assert_eq!(
            CloudSaveService::remote_manifest_path("game_test"),
            "/apps/GameSaver/saves/game_test/manifest.json"
        );
        assert_eq!(
            CloudSaveService::remote_package_path("game_test", "v123"),
            "/apps/GameSaver/saves/game_test/v123.zip"
        );
    }

    #[test]
    fn manifests_serialization_roundtrip() {
        let manifest = CloudSaveManifest {
            format_version: 1,
            game_key: "black_myth".to_string(),
            game_uid: "uid-1".to_string(),
            updated_at: "1788357696".to_string(),
            latest_version_id: Some("v1".to_string()),
            versions: vec![CloudSaveManifestVersion {
                version_id: "v1".to_string(),
                created_at: "1788357696".to_string(),
                package_path: "/apps/GameSaver/saves/black_myth/v1.zip".to_string(),
                package_fs_id: 123456,
                package_size: 1024,
                package_sha256: Some("sha".to_string()),
                file_count: 3,
                total_bytes: 4096,
                device_name: Some("DESKTOP".to_string()),
            }],
        };

        let raw = serde_json::to_string(&manifest).expect("serialize");
        let parsed: CloudSaveManifest = serde_json::from_str(&raw).expect("deserialize");
        assert_eq!(parsed.game_key, "black_myth");
        assert_eq!(parsed.versions.len(), 1);
        assert_eq!(parsed.versions[0].version_id, "v1");
        assert_eq!(parsed.versions[0].package_size, 1024);
    }

    #[test]
    fn downloaded_save_package_rejects_other_game() {
        let game = Game::new_pending("Test Game", r"E:\Games\TestGame", "game.exe");
        let result = validate_downloaded_save_meta(
            &package_meta("other-game", "version-1"),
            &game,
            &remote_version("version-1"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn save_package_paths_reject_traversal() {
        assert!(validate_package_data_path("data/SaveData/slot1.sav").is_ok());
        assert!(validate_package_data_path("data/../outside.sav").is_err());
        assert!(validate_package_data_path("data/C:\\outside.sav").is_err());
    }
}
