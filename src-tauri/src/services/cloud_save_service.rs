use crate::{
    domain::{Game, SaveFileEntry, SaveProfile, SaveRootType, SaveVersion},
    repositories::{BaiduConfigRepository, GameRepository, SaveRepository},
    services::{BaiduNetdiskClient, RemoteFile},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
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
        let zip_path = temp_dir.join(format!("gamesaver-save-pkg-{}.zip", Uuid::new_v4().simple()));
        let file = fs::File::create(&zip_path)
            .map_err(|err| format!("创建临时存档压缩文件失败：{err}"))?;
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);

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
                SaveRepository::read_object(app, hash).or_else(|_| {
                    read_file_from_scope(game, profile, entry)
                })?
            } else {
                read_file_from_scope(game, profile, entry)?
            };

            let zip_entry_name = format!("data/{}", entry.relative_path.trim_start_matches('/'));
            zip.start_file(zip_entry_name, options)
                .map_err(|err| format!("写入存档文件项失败：{err}"))?;
            zip.write_all(&bytes)
                .map_err(|err| format!("写入存档文件内容失败：{err}"))?;
        }

        zip.finish()
            .map_err(|err| format!("完成存档压缩包打包失败：{err}"))?;

        let zip_bytes = fs::read(&zip_path)
            .map_err(|err| format!("读取生成的存档压缩包失败：{err}"))?;
        let sha256 = sha256_hex(&zip_bytes);
        let size = zip_bytes.len() as u64;

        Ok((zip_path, sha256, size))
    }

    pub fn upload_save_version(
        app: &AppHandle,
        client: &BaiduNetdiskClient,
        game: &Game,
        profile: &SaveProfile,
        version: &SaveVersion,
        keep_limit: usize,
        on_progress: impl Fn(u8, &str) -> bool,
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

        manifest.versions.retain(|v| v.version_id != version.version_id);
        manifest.versions.push(manifest_version.clone());
        manifest.versions.sort_by(|left, right| {
            let left_time = left.created_at.as_str();
            let right_time = right.created_at.as_str();
            right_time.cmp(left_time).then_with(|| right.version_id.cmp(&left.version_id))
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
        let target_zip = temp_dir.join(format!("gamesaver-save-dl-{}.zip", Uuid::new_v4().simple()));

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

        if let Err(err) = dl_result {
            let _ = fs::remove_file(&target_zip);
            return Err(err);
        }

        on_progress(70, "正在校验并解压存档");
        let file = fs::File::open(&target_zip)
            .map_err(|err| format!("打开下载的存档压缩包失败：{err}"))?;
        let mut archive = ZipArchive::new(file)
            .map_err(|err| format!("解析下载的存档压缩包失败：{err}"))?;

        let meta = {
            let mut meta_file = archive.by_name("meta.json")
                .map_err(|err| format!("存档压缩包缺少 meta.json：{err}"))?;
            let mut meta_raw = Vec::new();
            meta_file.read_to_end(&mut meta_raw)
                .map_err(|err| format!("读取 meta.json 失败：{err}"))?;
            drop(meta_file);
            let meta: CloudSavePackageMeta = serde_json::from_slice(&meta_raw)
                .map_err(|err| format!("解析 meta.json 失败：{err}"))?;
            meta
        };
        let total_entries = archive.len();

        let mut imported_version = meta.version;
        imported_version.game_uid = game.game_uid.clone();

        for i in 0..total_entries {
            let mut item = archive.by_index(i)
                .map_err(|err| format!("读取压缩项失败：{err}"))?;
            let name = item.name().to_string();
            if name.starts_with("data/") && !name.ends_with('/') {
                let mut data = Vec::new();
                item.read_to_end(&mut data)
                    .map_err(|err| format!("解压文件项失败：{err}"))?;
                let hash = sha256_hex(&data);
                SaveRepository::write_object(app, &hash, &data)?;
            }
        }

        let _ = fs::remove_file(&target_zip);

        let state = app.state::<crate::app_state::AppState>();
        let mut store = state.store.lock().map_err(|_| "锁定本地存储失败".to_string())?;
        if !store.save_versions.iter().any(|v| v.version_id == imported_version.version_id && v.game_uid == game.game_uid) {
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
        let imported = Self::download_and_import_save_version(app, client, game, remote_version, &on_progress)?;
        on_progress(90, "正在恢复物理存档文件");
        let receipt = SaveRepository::restore(app, game, profile, &imported, |pct, msg| {
            let _ = on_progress(90 + (pct as f32 * 0.1) as u8, msg);
        })?;
        SaveRepository::finalize_restore(receipt);
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
        let temp_file = temp_dir.join(format!("save-manifest-fetch-{}.json", Uuid::new_v4().simple()));

        let files = client.list(&Self::remote_save_dir(game_key)).unwrap_or_default();
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

        let zip_files = files.iter().filter(|f| !f.is_dir && f.path.ends_with(".zip")).collect::<Vec<_>>();
        if zip_files.is_empty() {
            return Ok(None);
        }

        let versions = zip_files.into_iter().map(|f| {
            let v_id = Path::new(&f.path).file_stem().and_then(|s| s.to_str()).unwrap_or("unknown").to_string();
            CloudSaveManifestVersion {
                version_id: v_id,
                created_at: f.server_mtime.map(|t| t.to_string()).unwrap_or_else(now_iso),
                package_path: f.path.clone(),
                package_fs_id: f.fs_id,
                package_size: f.size,
                package_sha256: None,
                file_count: 0,
                total_bytes: f.size,
                device_name: None,
            }
        }).collect::<Vec<_>>();

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
        let temp_file = temp_dir.join(format!("save-manifest-put-{}.json", Uuid::new_v4().simple()));
        let raw = serde_json::to_vec_pretty(manifest)
            .map_err(|err| format!("序列化云端存档清单失败：{err}"))?;
        fs::write(&temp_file, &raw)
            .map_err(|err| format!("写入临时云端存档清单失败：{err}"))?;

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

        let mut manifest = Self::fetch_manifest(client, game_key, game_uid)?
            .unwrap_or_else(|| CloudSaveManifest {
                format_version: MANIFEST_VERSION,
                game_key: game_key.to_string(),
                game_uid: game_uid.to_string(),
                updated_at: now_iso(),
                latest_version_id: None,
                versions: Vec::new(),
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
        let auto_sync_save = baidu_config.as_ref().map(|c| c.auto_sync_save).unwrap_or(true);

        let state = app.state::<crate::app_state::AppState>();
        let store = state.store.lock().map_err(|_| "锁定本地存储失败".to_string())?;

        let mut local_versions = store.save_versions.iter()
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

        let manifest = Self::fetch_manifest(&client, &game.game_key, &game.game_uid).unwrap_or(None);
        let cloud_versions = manifest.as_ref().map(|m| m.versions.clone()).unwrap_or_default();
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

fn read_file_from_scope(game: &Game, profile: &SaveProfile, entry: &SaveFileEntry) -> Result<Vec<u8>, String> {
    let scope = profile.scopes.iter().find(|s| s.root_type == entry.root_type)
        .ok_or_else(|| format!("未找到匹配的作用域：{:?}", entry.root_type))?;
    let root = if matches!(scope.root_type, SaveRootType::ManagedGame) {
        PathBuf::from(&game.managed_path)
    } else {
        PathBuf::from(&scope.root_path)
    };
    let file_path = root.join(&entry.relative_path);
    fs::read(&file_path).map_err(|err| format!("读取物理存档文件失败：{} ({err})", file_path.display()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
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
    app.path().app_data_dir().map_err(|err| format!("解析应用数据目录失败：{err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

