use crate::{
    domain::{is_safe_path_segment, Game, GameBodyVersion},
    services::{BaiduNetdiskClient, RemoteFile},
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, io::Write, path::Path};
use uuid::Uuid;

const MANIFEST_VERSION: u32 = 1;
const MANIFEST_FILE_NAME: &str = "manifest.json";
const CATALOG_FILE_NAME: &str = "game.json";
const COVER_FILE_NAME: &str = "cover.jpg";

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
    #[serde(default)]
    pub has_cover: bool,
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
    #[serde(default)]
    pub has_cover: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudGamePage {
    pub games: Vec<CloudGameSummary>,
    pub page: usize,
    pub page_size: usize,
    pub total_count: usize,
    pub total_pages: usize,
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

/// `fetch` 失败的两种性质。`rebuild` 必须分别处置；`read` 则一视同仁（都当错误）。
#[derive(Debug)]
enum FetchFailure {
    /// 远端有清单，但**取不下来**（下载失败、大小不符、落盘或读取失败）。
    ///
    /// 远端那份可能完好，这一趟只是不顺 —— 调用方**不得**覆盖它，否则别的设备写下的
    /// `sha256` / `file_count` / `total_bytes` 会被一份没有这些字段的新清单静默顶掉。
    Unavailable(String),
    /// 拿到了**完整字节**却解析不了 —— 远端那份清单本身坏了。
    ///
    /// 为什么能确定「坏的是远端」而不是「我们只下了一半」：`download_file` 收尾会比对
    /// `written != remote.size`（`baidu_netdisk_service.rs:843`），下载不完整在那一步就
    /// 报错了，走不到这里。所以这里只剩「字节数对得上、但内容不是合法清单」这一种可能。
    Unparseable(String),
}

impl FetchFailure {
    fn into_message(self) -> String {
        match self {
            Self::Unavailable(message) | Self::Unparseable(message) => message,
        }
    }
}

/// 把清单字节解析成清单，失败时分类为 [`FetchFailure::Unparseable`]。
///
/// 单独抽出来是为了让「解析失败归成哪一类」也能被离线测到 —— 分类写错（归成
/// `Unavailable`）会让坏清单被当成「取不下来」，于是修复工具又对最坏的情况失效，
/// 而那种错误在 `fetch` 里是要真实网盘才跑得到的。
fn parse_manifest(raw: &[u8]) -> Result<CloudBodyManifest, FetchFailure> {
    serde_json::from_slice::<CloudBodyManifest>(raw)
        .map_err(|error| FetchFailure::Unparseable(format!("解析云端版本清单失败：{error}")))
}

/// `rebuild` 拿到「取旧清单」的结果之后的处置策略。
///
/// 抽成独立函数是为了能**离线测**：三种情况必须分开，而这正是此前 `.ok().flatten()`
/// 的错处 —— 它把前两种都当成了「清单不存在」。
fn existing_manifest_for_rebuild(
    fetched: Result<Option<CloudBodyManifest>, FetchFailure>,
) -> Result<Option<CloudBodyManifest>, String> {
    match fetched {
        // 取不下来：远端那份可能完好 → 不覆盖，让调用方报错。
        Err(FetchFailure::Unavailable(error)) => Err(error),
        // 拿到了完整字节却解析不了：远端清单本身坏了，没有元数据可沿用，**必须继续重建**。
        // 否则「修复清单」这个工具对坏得最彻底的清单反而失效，而下载侧对坏清单是硬停的
        // （`baidu_commands.rs` 的 `download_body_task`），用户就再没有自救手段。
        Err(FetchFailure::Unparseable(_)) => Ok(None),
        // 远端确实没有清单：按远端列表新建（正常路径）。
        Ok(None) => Ok(None),
        // 能解析但校验不过：正是「修复清单」要修的损坏清单，沿用其中元数据重建。
        Ok(Some(manifest)) => Ok(Some(manifest)),
    }
}

impl CloudManifestService {
    pub fn manifest_path(remote_dir: &str) -> String {
        format!("{remote_dir}/{MANIFEST_FILE_NAME}")
    }

    pub fn catalog_path(remote_dir: &str) -> String {
        format!("{remote_dir}/{CATALOG_FILE_NAME}")
    }

    pub fn cover_path(remote_dir: &str) -> String {
        format!("{remote_dir}/{COVER_FILE_NAME}")
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

    pub fn load_cached_cover(
        cache_root: &Path,
        remote_dir: &str,
        remote: &RemoteFile,
    ) -> Option<Vec<u8>> {
        let dir = cache_root.join(Self::cache_folder_name(remote_dir));
        let meta_path = dir.join("cover.cache.json");
        let image_path = dir.join("cover.jpg");
        let meta_bytes = fs::read(meta_path).ok()?;
        let cached = serde_json::from_slice::<CachedEntry<()>>(&meta_bytes).ok()?;
        if cached.fs_id == remote.fs_id
            && cached.size == remote.size
            && cached.server_mtime == remote.server_mtime
            && image_path.is_file()
        {
            fs::read(image_path).ok()
        } else {
            None
        }
    }

    pub fn save_cached_cover(
        cache_root: &Path,
        remote_dir: &str,
        remote: &RemoteFile,
        bytes: &[u8],
    ) -> Result<(), String> {
        let dir = cache_root.join(Self::cache_folder_name(remote_dir));
        fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        let meta_path = dir.join("cover.cache.json");
        let image_path = dir.join("cover.jpg");
        let entry = CachedEntry {
            fs_id: remote.fs_id,
            size: remote.size,
            server_mtime: remote.server_mtime,
            data: (),
        };
        let meta_bytes = serde_json::to_vec(&entry).map_err(|error| error.to_string())?;
        fs::write(meta_path, meta_bytes).map_err(|error| error.to_string())?;
        fs::write(image_path, bytes).map_err(|error| error.to_string())
    }

    pub fn read_cover(
        client: &BaiduNetdiskClient,
        remote_files: &[RemoteFile],
        remote_dir: &str,
        temporary_root: &Path,
        cache_root: Option<&Path>,
    ) -> Result<Option<Vec<u8>>, String> {
        let cover_path = Self::cover_path(remote_dir);
        let Some(remote_cover) = remote_files
            .iter()
            .find(|file| file.path == cover_path && !file.is_dir)
        else {
            return Ok(None);
        };
        if let Some(cache_root) = cache_root {
            if let Some(cached) = Self::load_cached_cover(cache_root, remote_dir, remote_cover) {
                return Ok(Some(cached));
            }
        }
        fs::create_dir_all(temporary_root)
            .map_err(|error| format!("创建云端封面下载目录失败：{error}"))?;
        let temporary = temporary_root.join(format!(
            ".cloud-cover-download-{}.jpg",
            Uuid::new_v4().simple()
        ));
        let result = (|| -> Result<Vec<u8>, String> {
            client.download_file(remote_cover, &temporary, |_, _| true)?;
            let raw = fs::read(&temporary).map_err(|error| format!("读取云端封面失败：{error}"))?;
            if let Some(cache_root) = cache_root {
                let _ = Self::save_cached_cover(cache_root, remote_dir, remote_cover, &raw);
            }
            Ok(raw)
        })();
        let _ = fs::remove_file(&temporary);
        let _ = fs::remove_file(temporary.with_extension("download.tmp"));
        result.map(Some)
    }

    pub fn write_cover(
        client: &BaiduNetdiskClient,
        remote_dir: &str,
        cover_bytes: &[u8],
        temporary_root: &Path,
        cache_root: Option<&Path>,
    ) -> Result<(), String> {
        fs::create_dir_all(temporary_root)
            .map_err(|error| format!("创建云端封面临时目录失败：{error}"))?;
        let temporary =
            temporary_root.join(format!(".cloud-cover-{}.jpg", Uuid::new_v4().simple()));
        let result = (|| -> Result<(), String> {
            let mut file = fs::File::create(&temporary)
                .map_err(|error| format!("创建云端封面临时文件失败：{error}"))?;
            file.write_all(cover_bytes)
                .map_err(|error| format!("写入云端封面临时文件失败：{error}"))?;
            file.sync_all()
                .map_err(|error| format!("刷新云端封面临时文件失败：{error}"))?;
            client.upload_file(&temporary, &Self::cover_path(remote_dir), |_, _| true)?;
            Ok(())
        })();
        let _ = fs::remove_file(&temporary);
        if let Some(cache_root) = cache_root {
            let dir = cache_root.join(Self::cache_folder_name(remote_dir));
            let _ = fs::create_dir_all(&dir);
            let _ = fs::write(dir.join("cover.jpg"), cover_bytes);
        }
        result
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
            has_cover: game.cover.is_some(),
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
        let temporary =
            temporary_root.join(format!(".cloud-game-{}.json", Uuid::new_v4().simple()));
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
            if let Some(cached) = Self::load_cached_catalog(cache_root, remote_dir, remote_catalog)
            {
                return Ok(Some(cached));
            }
        }
        fs::create_dir_all(temporary_root)
            .map_err(|error| format!("创建云端游戏信息下载目录失败：{error}"))?;
        let temporary = temporary_root.join(format!(
            ".cloud-game-download-{}.json",
            Uuid::new_v4().simple()
        ));
        let result = (|| -> Result<CloudGameCatalog, String> {
            client.download_file(remote_catalog, &temporary, |_, _| true)?;
            let raw =
                fs::read(&temporary).map_err(|error| format!("读取云端游戏信息失败：{error}"))?;
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
                    && version
                        .remote_path
                        .as_deref()
                        .is_some_and(|path| !path.is_empty())
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
        // 不能用 `.ok().flatten()`：那会把「取不下来 / 解析不了」当成「清单不存在」，
        // 于是静默用一份新清单覆盖旧的，别的设备写下的 sha256 / file_count /
        // total_bytes 就此丢失 —— 之后该版本下载不再比对哈希，**完整性校验被静默降级**。
        // 三种情况的处置见 `existing_manifest_for_rebuild`。
        let existing = existing_manifest_for_rebuild(Self::fetch(
            client,
            remote_files,
            remote_dir,
            temporary_root,
        ))?;
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
                        local_versions
                            .iter()
                            .find(|local| local.version_id == version.version_id)
                    })
                    .or_else(|| {
                        local_versions.iter().find(|version| {
                            version.version_id == file_name_without_extension(&file.path)
                        })
                    });
                let existing = existing_by_path.get(&file.path).copied();
                CloudBodyManifestVersion {
                    version_id: rebuilt_version_id(existing, local, &file.path),
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
        let temporary =
            temporary_root.join(format!(".cloud-manifest-{}.json", Uuid::new_v4().simple()));
        let result = (|| -> Result<(), String> {
            let mut file = fs::File::create(&temporary)
                .map_err(|error| format!("创建云端版本清单临时文件失败：{error}"))?;
            file.write_all(&bytes)
                .map_err(|error| format!("写入云端版本清单临时文件失败：{error}"))?;
            file.sync_all()
                .map_err(|error| format!("刷新云端版本清单临时文件失败：{error}"))?;
            client.upload_file(&temporary, &Self::manifest_path(remote_dir), |_, _| true)?;
            Ok(())
        })();
        let _ = fs::remove_file(&temporary);
        result
    }

    /// 下载并解析远端清单，**不校验内容**。
    ///
    /// 拆出来的原因：`rebuild` 必须区分「取不下来」与「解析不了」—— 前者说明远端那份
    /// 可能完好，覆盖它等于静默丢掉别的设备写下的校验值；后者说明远端清单本身坏了，
    /// 正是「修复清单」要修的对象。`read` 与 `rebuild` 因此共用这一段。
    ///
    /// 失败时带回 [`FetchFailure`] 让调用方自己决定：`read` 两种都当错误，`rebuild`
    /// 只对「取不下来」报错。
    fn fetch(
        client: &BaiduNetdiskClient,
        remote_files: &[RemoteFile],
        remote_dir: &str,
        temporary_root: &Path,
    ) -> Result<Option<CloudBodyManifest>, FetchFailure> {
        let manifest_path = Self::manifest_path(remote_dir);
        let Some(remote_manifest) = remote_files
            .iter()
            .find(|file| file.path == manifest_path && !file.is_dir)
        else {
            return Ok(None);
        };
        fs::create_dir_all(temporary_root).map_err(|error| {
            FetchFailure::Unavailable(format!("创建云端版本清单下载目录失败：{error}"))
        })?;
        let temporary = temporary_root.join(format!(
            ".cloud-manifest-download-{}.json",
            Uuid::new_v4().simple()
        ));
        let result = (|| -> Result<CloudBodyManifest, FetchFailure> {
            client
                .download_file(remote_manifest, &temporary, |_, _| true)
                .map_err(FetchFailure::Unavailable)?;
            let raw = fs::read(&temporary).map_err(|error| {
                FetchFailure::Unavailable(format!("读取云端版本清单失败：{error}"))
            })?;
            // 走到这里字节数是完整的（`download_file` 已比对 remote.size），
            // 所以解析失败只可能是远端内容本身不合法。
            parse_manifest(&raw)
        })();
        let _ = fs::remove_file(&temporary);
        let _ = fs::remove_file(temporary.with_extension("download.tmp"));
        result.map(Some)
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
            if let Some(cached) =
                Self::load_cached_manifest(cache_root, remote_dir, remote_manifest)
            {
                return Ok(Some(cached));
            }
        }
        // 两种失败对 `read` 一视同仁：解析不了也是错误。绝不能让它退化成「没有清单」——
        // 那会让下载侧以为没有元数据可校验，等于把完整性校验静默降级。
        let Some(manifest) = Self::fetch(client, remote_files, remote_dir, temporary_root)
            .map_err(FetchFailure::into_message)?
        else {
            return Ok(None);
        };
        // 校验通过才入缓存：缓存命中时会重新校验（`load_cached_manifest`），
        // 所以这里存进去的必须是校验过的内容。
        validate(&manifest, remote_dir)?;
        if let Some(cache_root) = cache_root {
            let _ = Self::save_cached_manifest(cache_root, remote_dir, remote_manifest, &manifest);
        }
        Ok(Some(manifest))
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
                        || manifest_version
                            .is_some_and(|item| item.version_id == version.version_id)
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
                    created_at: manifest_version
                        .map(|item| item.created_at.clone())
                        .or_else(|| file.server_mtime.map(|time| time.to_string())),
                    sync_state: sync_state.to_string(),
                    manifest_verified: manifest_version
                        .is_some_and(|item| item.package_sha256.is_some()),
                }
            })
            .collect::<Vec<_>>();
        packages.sort_by(|left, right| {
            let left_time = left.created_at.as_deref().unwrap_or_default();
            let right_time = right.created_at.as_deref().unwrap_or_default();
            right_time
                .cmp(left_time)
                .then_with(|| right.path.cmp(&left.path))
        });
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
                    warnings.push(format!(
                        "云端本体包未登记在版本清单中：{}",
                        package.version_id
                    ));
                }
            }
            for version in manifest.into_iter().flat_map(|value| value.versions.iter()) {
                if !remote_files
                    .iter()
                    .any(|file| file.path == version.package_path)
                {
                    warnings.push(format!(
                        "版本清单记录的本体包不存在：{}",
                        version.version_id
                    ));
                }
            }
        }
        for package in &packages {
            if package.sync_state == "mismatch" {
                warnings.push(format!(
                    "云端本体包与本地版本校验值不一致：{}",
                    package.version_id
                ));
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
        return Err(format!(
            "不支持的云端版本清单格式：{}",
            manifest.format_version
        ));
    }
    let expected_game_key = game_key_from_body_dir(remote_dir);
    if manifest.game_key != expected_game_key
        || manifest.game_key.trim().is_empty()
        || !is_valid_game_uid(&manifest.game_uid)
    {
        return Err("云端版本清单缺少游戏标识".to_string());
    }
    if manifest.versions.iter().any(|version| {
        // `version_id` 会被拼进**本地**缓存路径（`BodyPackageService::package_path`），所以
        // 它必须是一段安全路径段，而不只是「非空」。这条是入口处的拒绝：让被污染的清单在
        // 读取时就失败，而不是等到拼路径时才发现。
        //
        // 顺带堵住 `rebuild` 的洗白：它用 `Self::read(...).ok().flatten()` 读旧清单并沿用其中
        // 的 `version_id` 回写。清单一旦在校验处被拒，`existing` 即为 `None`，新清单的
        // `version_id` 就退回本地记录或文件名，污染不会被回写扩散。
        !is_safe_path_segment(&version.version_id)
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
        return Err(format!(
            "不支持的云端游戏信息格式：{}",
            catalog.format_version
        ));
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

/// 重建清单时，某个条目的版本标识该取哪个来源（按可信度排序）。
///
/// `existing` 来自**可能没过校验**的旧清单 —— `rebuild` 刻意沿用损坏清单里的元数据
/// （否则一个被改坏的清单会让「修复清单」失效）。所以它的 `version_id` 不能直接采信：
/// 这个字段会被拼进**本地缓存路径**（`BodyPackageService::package_path`）。只有安全
/// 路径段才沿用，否则退回本地记录或文件名这两个可信来源。
///
/// 这条判定与 `validate` 里那条同一个谓词，是**出口侧的兜底**：`rebuild` 用的这份清单
/// 绕过了 `validate`，不能只靠入口那一处挡。
fn rebuilt_version_id(
    existing: Option<&CloudBodyManifestVersion>,
    local: Option<&GameBodyVersion>,
    file_path: &str,
) -> String {
    existing
        .filter(|version| is_safe_path_segment(&version.version_id))
        .map(|version| version.version_id.clone())
        .or_else(|| local.map(|version| version.version_id.clone()))
        .unwrap_or_else(|| file_name_without_extension(file_path))
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
        existing_manifest_for_rebuild, game_key_from_body_dir, parse_manifest, rebuilt_version_id,
        validate, CloudBodyManifest, CloudBodyManifestVersion, CloudGameCatalog,
        CloudManifestService, FetchFailure,
    };
    use crate::{
        domain::GameBodyVersion,
        services::{BaiduNetdiskClient, BaiduToken, RemoteFile},
    };
    use std::fs;

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
        let manifest = CloudManifestService::build(
            "game one",
            "game-1",
            &[local.clone(), not_uploaded],
            "2".to_string(),
        );
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

    /// `version_id` 会被拼进**本地**缓存路径（`BodyPackageService::package_path`），所以清单
    /// 校验必须拒绝带分隔符或 `..` 的值，而不只是「非空」。
    ///
    /// 收窄前这里只查 `trim().is_empty()`，于是一个被改写的 `manifest.json` 就能让安装路径
    /// 逃出缓存根，随后被 `remove_file` 与下载写入 —— 这是 F1 在**本地**侧的镜像，且本地文件
    /// 系统对 `..` 的解析是确定的，不必赌服务端行为。
    #[test]
    fn validates_manifest_rejects_unsafe_version_id() {
        let manifest_with = |version_id: &str| CloudBodyManifest {
            format_version: 1,
            game_key: "game-one".to_string(),
            game_uid: "game-1".to_string(),
            updated_at: "2".to_string(),
            versions: vec![CloudBodyManifestVersion {
                version_id: version_id.to_string(),
                created_at: "2".to_string(),
                package_path: "/apps/GameSaver/games/game-one/body/v1.zip".to_string(),
                package_fs_id: 42,
                package_size: 1,
                package_sha256: None,
                file_count: 1,
                total_bytes: 1,
            }],
        };
        let dir = "/apps/GameSaver/games/game-one/body";

        // 反向守卫：正常的版本标识必须仍然通过（含空格与非 ASCII），别把校验做过头。
        for safe in ["v1", "2026-09-14T10-00-00", "版本 1"] {
            assert!(
                validate(&manifest_with(safe), dir).is_ok(),
                "rejected safe version_id: {safe:?}"
            );
        }
        // 远端可控的异常值：一个都不许过。
        for unsafe_id in [
            "..",
            ".",
            "../escape",
            "..\\escape",
            "a/b",
            "a\\b",
            "",
            "   ",
        ] {
            assert!(
                validate(&manifest_with(unsafe_id), dir).is_err(),
                "accepted unsafe version_id: {unsafe_id:?}"
            );
        }
    }

    /// 重建清单时不能盲信旧清单里的 `version_id`。
    ///
    /// `rebuild` 刻意沿用**没过校验**的损坏清单里的元数据（否则一个被改坏的清单会让
    /// 「修复清单」这个工具本身失效），所以这个字段必须在这里再过一次路径段判定 ——
    /// 它会被拼进本地缓存路径。这是出口侧的兜底，不能只靠 `validate` 那一处。
    #[test]
    fn rebuild_adopts_a_version_id_only_when_it_is_a_safe_path_segment() {
        let manifest_version = |version_id: &str| CloudBodyManifestVersion {
            version_id: version_id.to_string(),
            created_at: "2".to_string(),
            package_path: "/apps/GameSaver/games/game-one/body/v9.zip".to_string(),
            package_fs_id: 9,
            package_size: 1,
            package_sha256: None,
            file_count: 1,
            total_bytes: 1,
        };
        let path = "/apps/GameSaver/games/game-one/body/v9.zip";

        // 安全：沿用旧清单的值 —— 它才是版本身份，别的设备靠它对上号。
        assert_eq!(
            rebuilt_version_id(Some(&manifest_version("from-manifest")), None, path),
            "from-manifest"
        );
        // 不安全：不沿用（否则污染会被本体应用自己洗进新清单），退回本地记录。
        assert_eq!(
            rebuilt_version_id(Some(&manifest_version("../escape")), Some(&version()), path),
            "v1"
        );
        // 旧清单不安全且没有本地记录：退回文件名。
        assert_eq!(
            rebuilt_version_id(Some(&manifest_version("../escape")), None, path),
            "v9"
        );
        assert_eq!(rebuilt_version_id(None, None, path), "v9");
    }

    /// `rebuild` 对「取旧清单」的三种结果必须分别处置 —— 这是 H2 的核心，**也是我第一次
    /// 改错的地方**。
    ///
    /// 第一版把「取不下来」与「解析不了」都算作错误、都不覆盖。方向对了一半：取不下来时
    /// 远端那份可能完好，确实不该覆盖；但**解析不了说明远端清单本身坏了**，而
    /// `repair_cloud_body_manifest` 正是要修这种清单，下载侧（`download_body_task`）对坏
    /// 清单又是硬停的 —— 两条加在一起，等于让坏得最彻底的清单**再也修不回来**，用户只能
    /// 去网页端手改。所以这一条必须继续重建。
    ///
    /// 抽成 `existing_manifest_for_rebuild` 就是为了能离线测到这条分支：真跑到 `fetch`
    /// 里需要让「下载成功但内容非法」，那要真实网盘。
    #[test]
    fn rebuild_overwrites_when_the_remote_manifest_is_unparseable_but_not_when_it_is_unavailable() {
        // 取不下来：必须报错中止，否则会静默丢掉别的设备写下的校验值。
        let unavailable = existing_manifest_for_rebuild(Err(FetchFailure::Unavailable(
            "本体包下载大小不匹配".to_string(),
        )));
        assert_eq!(
            unavailable.expect_err("取不下来时必须中止"),
            "本体包下载大小不匹配"
        );

        // 解析不了：必须继续重建 —— 「修复清单」要修的正是这种。
        let unparseable = existing_manifest_for_rebuild(Err(FetchFailure::Unparseable(
            "解析云端版本清单失败".to_string(),
        )))
        .expect("远端清单解析不了时必须继续重建，否则修复工具对最坏的情况反而失效");
        assert!(unparseable.is_none(), "坏清单没有元数据可沿用");

        // 远端确实没有清单：新建。
        assert!(existing_manifest_for_rebuild(Ok(None))
            .expect("清单不存在时应新建")
            .is_none());

        // 能解析但校验不过：沿用其中元数据（旧值才是版本身份，且要留住 sha256）。
        let manifest = CloudBodyManifest {
            format_version: 1,
            game_key: "game-one".to_string(),
            game_uid: "uid-one".to_string(),
            updated_at: "1".to_string(),
            versions: vec![CloudBodyManifestVersion {
                version_id: "from-manifest".to_string(),
                created_at: "2".to_string(),
                package_path: "/apps/GameSaver/games/game-one/body/v9.zip".to_string(),
                package_fs_id: 9,
                package_size: 1,
                package_sha256: Some("a".repeat(64)),
                file_count: 1,
                total_bytes: 1,
            }],
        };
        let reused = existing_manifest_for_rebuild(Ok(Some(manifest)))
            .expect("能解析时应沿用")
            .expect("应带回旧清单");
        assert_eq!(reused.versions[0].version_id, "from-manifest");

        // 再走一遍分类那一段：**坏字节必须被归成「解析不了」**。若归成「取不下来」，
        // 上面那条 Unparseable 分支就永远走不到，坏清单会被拒绝修复 —— 这是接线，
        // 与策略分开测，因为 `fetch` 里那一步要真实网盘才跑得到。
        let classified = parse_manifest(b"{ this is not a manifest");
        assert!(
            matches!(classified, Err(FetchFailure::Unparseable(_))),
            "解析失败必须分类为 Unparseable，否则坏清单会被当成「取不下来」而拒绝修复"
        );
        assert!(
            existing_manifest_for_rebuild(classified.map(Some)).is_ok(),
            "坏字节必须能走通「按远端列表重建」这条路"
        );
    }

    /// `rebuild` 读不到旧清单时**不能**当成「清单不存在」继续覆盖回写。
    ///
    /// 收窄前是 `.ok().flatten()`：读失败 → `None` → 按远端列表重建并写回，别的设备写下
    /// 的 `sha256` / `file_count` / `total_bytes` 静默丢失，该版本此后下载不再比对哈希。
    ///
    /// 判据怎么做到离线的：让 `temporary_root` 指向一个**存在的文件**，`fetch` 会在任何
    /// 网络动作之前就失败。此时「中止」与「继续覆盖」会停在不同阶段，而两处文案不同
    /// （取清单那步是"下载目录"，回写那步是"临时目录"）—— 断言错误来自**取清单**那一步，
    /// 就证明确实中止了，没有走到覆盖回写。
    #[test]
    fn rebuild_aborts_instead_of_overwriting_when_the_old_manifest_cannot_be_read() {
        let placeholder = std::env::temp_dir().join(format!(
            "gamesaver-manifest-rebuild-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::write(&placeholder, b"not a directory").expect("写入占位文件");

        let client = BaiduNetdiskClient::new(BaiduToken {
            access_token: "test-access-token".to_string(),
            expires_at: None,
            refresh_token: None,
        })
        .expect("构造测试客户端");
        let remote_dir = "/apps/GameSaver/games/game-one/body";
        let remote_files = vec![
            RemoteFile {
                path: format!("{remote_dir}/manifest.json"),
                fs_id: 1,
                size: 1,
                md5: None,
                is_dir: false,
                server_mtime: None,
            },
            RemoteFile {
                path: format!("{remote_dir}/v9.zip"),
                fs_id: 9,
                size: 1,
                md5: None,
                is_dir: false,
                server_mtime: None,
            },
        ];

        let error = CloudManifestService::rebuild(
            &client,
            remote_dir,
            "game-one",
            "game-1",
            &remote_files,
            &[],
            &placeholder,
        )
        .expect_err("读不到旧清单时必须中止，而不是覆盖回写");

        assert!(
            error.contains("下载目录"),
            "必须中止在取清单这一步（不覆盖旧清单），实际错误：{error}"
        );
        let _ = fs::remove_file(&placeholder);
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
                server_mtime: Some(2),
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
        let temp_dir = crate::test_support::TempWorkspace::new("cache-test");
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
            has_cover: true,
        };

        CloudManifestService::save_cached_catalog(&temp_dir, remote_dir, &remote_file, &catalog)
            .expect("save cached catalog");

        let loaded = CloudManifestService::load_cached_catalog(&temp_dir, remote_dir, &remote_file);
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().display_name, "Test Game");

        // Mismatched fs_id should return None (cache miss)
        let mut modified_remote = remote_file.clone();
        modified_remote.fs_id = 102;
        assert!(
            CloudManifestService::load_cached_catalog(&temp_dir, remote_dir, &modified_remote)
                .is_none()
        );

        // Cover cache test
        let cover_remote = RemoteFile {
            path: format!("{remote_dir}/cover.jpg"),
            fs_id: 201,
            size: 50,
            md5: None,
            is_dir: false,
            server_mtime: Some(1700000000),
        };
        let cover_bytes = b"fake-jpeg-content";
        CloudManifestService::save_cached_cover(&temp_dir, remote_dir, &cover_remote, cover_bytes)
            .expect("save cached cover");
        let loaded_cover =
            CloudManifestService::load_cached_cover(&temp_dir, remote_dir, &cover_remote);
        assert_eq!(loaded_cover, Some(cover_bytes.to_vec()));

        let _ = fs::remove_dir_all(temp_dir);
    }
}
