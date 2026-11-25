use crate::{
    commands::baidu_commands::validate_remote_package_path,
    domain::{
        compare_created_at, is_safe_path_segment, Game, SaveFileEntry, SaveProfile, SaveVersion,
    },
    repositories::{save_repository::RestorePlan, BaiduConfigRepository, GameRepository, SaveRepository},
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

mod manifest;
#[cfg(test)]
mod new_machine_tests;

const MANIFEST_VERSION: u32 = 1;
const REMOTE_SAVES_ROOT: &str = "/apps/GameSaver/saves";

/// 解压单个存档条目时的硬上限。
///
/// 正常的收集侧默认限制是 `max_file_bytes = 10 MiB`（`SaveProfile`），这里取值远超它、
/// 也远超任何正常游戏的单个存档文件；它的作用不是「限制正常存档」，而是在云端清单本身
/// 不可信时兜底 —— 远端清单缺少 `package_sha256` 时不再有哈希锚点，`meta.json` 里的
/// 声明大小可以被随意伪造，没有这道闸门就无法阻止伪造值把内存撑爆。
const MAX_SAVE_PACKAGE_ENTRY_BYTES: u64 = 1024 * 1024 * 1024;

/// `meta.json` 的解压上限。它是条目清单，体量随文件数线性增长，32 MiB 足够容纳
/// 数万个条目，同时把伪造的巨物挡在分配之前。
const MAX_SAVE_PACKAGE_META_BYTES: u64 = 32 * 1024 * 1024;

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
    #[serde(default = "legacy_package_format")]
    pub format_version: u32,
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudSaveOverview {
    pub status: CloudSaveSyncStatusView,
    pub versions: Vec<CloudSaveManifestVersion>,
}

pub struct CloudSaveService;

impl CloudSaveService {
    /// 校验并返回可以直接拼进远程路径的 game_key。
    ///
    /// 为什么必须有这道校验：这些函数的结果会被送进百度的 `path` 参数**直接作用于远端
    /// 文件**（`delete_cloud_version` 拿它去删远程包），而 `game_key` 的来源里有若干条
    /// 是外部文本——显示名、云端清单里的 `game_key`。含 `/`、`\`、`..` 的名字会让拼出来
    /// 的路径离开「本游戏目录」这层，落到别的游戏、甚至 `/apps/GameSaver/` 之外：
    /// `delete_file` 就会去动那段路径。
    ///
    /// 判定复用领域层的 `is_safe_path_segment`（其注释本就写明用于「会被拼进某个根目录
    /// 之下、只占一层的名字」）。注意它**允许空格**，这是有意的：`derive_game_key` 会
    /// 保留空格（如 `"black myth"`），而空格在远程路径里是安全的，收紧成字符白名单会
    /// 误伤这些真实存在的 key。
    ///
    /// 调用方必须让这三个构造函数**在任何远端 IO 之前**求值（`?` 直接向上传播），否则
    /// 输入非法时我们会先动了别的东西才报错。`delete_cloud_version` 就是按这个顺序写的。
    fn game_key_segment(game_key: &str) -> Result<&str, String> {
        if !is_safe_path_segment(game_key) {
            return Err(format!(
                "游戏标识包含不安全的远程路径字符，已拒绝云端存档操作：{game_key:?}"
            ));
        }
        Ok(game_key.trim())
    }

    pub fn remote_save_dir(game_key: &str) -> Result<String, String> {
        Ok(format!(
            "{REMOTE_SAVES_ROOT}/{}",
            Self::game_key_segment(game_key)?
        ))
    }

    pub fn remote_manifest_path(game_key: &str) -> Result<String, String> {
        Ok(format!(
            "{}/manifest.json",
            Self::remote_save_dir(game_key)?
        ))
    }

    pub fn remote_package_path(game_key: &str, version_id: &str) -> Result<String, String> {
        if !is_safe_path_segment(version_id) {
            return Err(format!(
                "存档版本标识包含不安全的远程路径字符，已拒绝云端存档操作：{version_id:?}"
            ));
        }
        Ok(format!(
            "{}/{}.zip",
            Self::remote_save_dir(game_key)?,
            version_id.trim()
        ))
    }

    /// 校验云端清单里的 `package_path` **可证明地**落在本游戏的云存档目录内。
    ///
    /// 上面三个构造函数管的是「我们拼出去」的路径；这一条管的是**从服务端回来的**路径。
    /// 两者必须都管：`fetch_manifest` 把下载到的 `manifest.json` 反序列化后**不校验任何
    /// 字段**就返回，于是 `versions[].package_path` 是纯粹的远端输入，却会原样流进
    /// `delete_file`（超出 `keep_limit` 剪枝时）和下载构造处。
    ///
    /// 为什么不能在出口处「相信百度会归一化 `..`」：服务端对 `..`、`.`、空路径段的解析
    /// 策略无法在本机验证（见 `docs/cloud-transfer-review-2026-09-14.md` §5 存疑项 1），
    /// 所以这里不去猜服务端，而是要求路径**可证明地**只占本游戏目录下的一层。判定复用
    /// `validate_remote_package_path`（本体包同一条规则），不另写第二份安全检查。
    ///
    /// 与本文件 `MAX_SAVE_PACKAGE_ENTRY_BYTES` 的立场一致：远端清单本身不可信 —— 那里防的
    /// 是伪造大小撑爆内存，这里防的是伪造路径作用于目录之外。
    ///
    /// **返回值是有意设计的**：调用方必须用它、而不是回头再读 `package_path` 字段。这样
    /// 「删掉这一行守卫」会让下游编译失败 —— 否则删掉一行 `...?;` 语句既没有测试能发现
    /// （两个出口都吃 `&AppHandle`，无法单元测试），也未必触发 clippy 的 unused 告警
    /// （只要还有别的调用点用到这个函数就不会告警）。
    fn verified_package_path<'a>(game_key: &str, package_path: &'a str) -> Result<&'a str, String> {
        let directory = Self::remote_save_dir(game_key)?;
        validate_remote_package_path(&directory, package_path).map_err(|error| {
            format!("云端存档清单里的包路径不属于当前游戏，已拒绝云端操作：{error}")
        })?;
        Ok(package_path)
    }

    pub fn package_save_version(
        app: &AppHandle,
        game: &Game,
        profile: &SaveProfile,
        version: &SaveVersion,
        on_progress: &(impl Fn(u8, &str) -> bool + Sync),
    ) -> Result<(PathBuf, String, u64), String> {
        let temp_dir = std::env::temp_dir();
        let zip_path = temp_dir.join(format!(
            "gamesaver-save-pkg-{}.zip",
            Uuid::new_v4().simple()
        ));
        let meta = CloudSavePackageMeta {
            format_version: 2,
            version: version.clone(),
            game_key: game.game_key.clone(),
            display_name: game.display_name.clone(),
            created_at: version.created_at.clone(),
            device_name: Some(hostname()),
        };
        write_save_package(
            &zip_path,
            &meta,
            |entry| read_entry_bytes(app, game, profile, entry),
            on_progress,
        )
    }

    pub fn upload_save_version(
        app: &AppHandle,
        client: &BaiduNetdiskClient,
        game: &Game,
        profile: &SaveProfile,
        version: &SaveVersion,
        keep_limit: usize,
        on_progress: impl Fn(u8, &str) -> bool + Sync,
        begin_commit: impl FnOnce() -> Result<(), String>,
    ) -> Result<CloudSaveManifestVersion, String> {
        let remote_dir = Self::remote_save_dir(&game.game_key)?;
        let remote_pkg = Self::remote_package_path(&game.game_key, &version.version_id)?;
        check_progress(&on_progress, 5, "正在读取云端存档清单")?;
        let existing_manifest = Self::fetch_manifest(client, &game.game_key, &game.game_uid)?;
        let first_sync = existing_manifest.is_none();
        let mut manifest = existing_manifest.unwrap_or_else(|| CloudSaveManifest {
            format_version: MANIFEST_VERSION,
            game_key: game.game_key.clone(),
            game_uid: game.game_uid.clone(),
            updated_at: now_iso(),
            latest_version_id: None,
            versions: Vec::new(),
        });
        if let Some(existing) = manifest
            .versions
            .iter()
            .find(|item| item.version_id == version.version_id)
            .cloned()
        {
            check_progress(&on_progress, 85, "该版本已在云端，正在核对保留策略")?;
            begin_commit()?;
            manifest.latest_version_id = Some(existing.version_id.clone());
            manifest.updated_at = now_iso();
            manifest::commit_and_prune(
                &mut manifest,
                keep_limit,
                |value| Self::save_manifest(client, &game.game_key, value),
                |path| client.delete_file(path),
            )?;
            return Ok(existing);
        }
        check_progress(&on_progress, 10, "正在打包本地存档")?;
        let (zip_path, sha256, size) =
            Self::package_save_version(app, game, profile, version, &on_progress)?;
        let _temporary_zip = TemporaryFileGuard(zip_path.clone());

        let upload_result = (|| -> Result<RemoteFile, String> {
            client.ensure_directory(&remote_dir)?;
            if first_sync {
                check_progress(&on_progress, 30, "正在初始化云端存档清单")?;
                Self::save_manifest(client, &game.game_key, &manifest)?;
            }
            check_progress(&on_progress, 30, "正在上传存档包到百度网盘")?;
            client.upload_file(&zip_path, &remote_pkg, Some(&sha256), |pct, msg| {
                on_progress(30 + (pct as f32 * 0.5) as u8, msg)
            })
        })();

        let _ = fs::remove_file(&zip_path);
        let remote_file = upload_result?;

        if let Err(error) =
            check_progress(&on_progress, 85, "正在更新云端存档清单").and_then(|()| begin_commit())
        {
            if let Err(cleanup_error) = client.delete_file(&remote_pkg) {
                crate::logging::warn(format!("取消上传后的存档包清理失败：{cleanup_error}"));
            }
            return Err(error);
        }

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
        manifest::sort_versions(&mut manifest.versions);
        manifest.latest_version_id = Some(version.version_id.clone());
        manifest.updated_at = now_iso();

        manifest::commit_and_prune(
            &mut manifest,
            keep_limit,
            |value| Self::save_manifest(client, &game.game_key, value),
            |path| client.delete_file(path),
        )?;
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
        // 与本文件其它出口保持同一纪律：任何远端 IO 之前先求值，验不过就什么都不动。
        // 下面构造 `RemoteFile` 时必须用这个返回值，不要回头再读 `remote_version.package_path`
        // —— 那会让这一行守卫变成可以悄悄删掉而不破坏编译的东西。
        let verified_package_path =
            Self::verified_package_path(&game.game_key, &remote_version.package_path)?;
        let temp_dir = std::env::temp_dir();
        let target_zip =
            temp_dir.join(format!("gamesaver-save-dl-{}.zip", Uuid::new_v4().simple()));
        let _temporary_zip = TemporaryFileGuard(target_zip.clone());

        check_progress(&on_progress, 15, "正在从百度网盘下载存档包")?;
        let remote_file = RemoteFile {
            path: verified_package_path.to_string(),
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
            if !expected_hash.is_empty() && !downloaded_sha256.eq_ignore_ascii_case(expected_hash) {
                let _ = fs::remove_file(&target_zip);
                return Err("下载的存档压缩包完整性校验失败（SHA-256 不匹配）".to_string());
            }
        }

        check_progress(&on_progress, 70, "正在校验并解压存档")?;
        let file = fs::File::open(&target_zip)
            .map_err(|err| format!("打开下载的存档压缩包失败：{err}"))?;
        let reader = BufReader::with_capacity(512 * 1024, file);
        let mut archive =
            ZipArchive::new(reader).map_err(|err| format!("解析下载的存档压缩包失败：{err}"))?;

        let meta = {
            let mut meta_file = archive
                .by_name("meta.json")
                .map_err(|err| format!("存档压缩包缺少 meta.json：{err}"))?;
            let declared = meta_file.size();
            let meta_raw = read_zip_entry_cancellable(
                &mut meta_file,
                declared,
                None,
                MAX_SAVE_PACKAGE_META_BYTES,
                "meta.json",
                || on_progress(70, "正在校验存档元数据"),
            )?;
            drop(meta_file);
            let meta: CloudSavePackageMeta = serde_json::from_slice(&meta_raw)
                .map_err(|err| format!("解析 meta.json 失败：{err}"))?;
            meta
        };
        let expected_entries = validate_downloaded_save_meta(&meta, game, remote_version)?;

        let mut imported_version = meta.version;
        imported_version.game_uid = game.game_uid.clone();
        // 对象在版本清单提交前必须保持活跃引用。否则另一个游戏的剪枝/回收任务可能在
        // 下载导入过程中把刚写入、但尚未出现在 store.json 的对象当成孤立文件删除。
        let _pending_objects = SaveRepository::hold_pending_objects(&imported_version)?;
        import_archive_objects(
            &mut archive,
            &expected_entries,
            |hash, data| SaveRepository::write_object(app, hash, data),
            &on_progress,
        )?;

        drop(archive);
        let _ = fs::remove_file(&target_zip);

        let state = app.state::<crate::app_state::AppState>();
        check_progress(&on_progress, 83, "正在登记本地存档版本")?;
        state.with_store_mut(|store| {
            if let Some(existing) = store.save_versions.iter().find(|v| {
                v.version_id == imported_version.version_id && v.game_uid == game.game_uid
            }) {
                if serde_json::to_vec(existing).ok() != serde_json::to_vec(&imported_version).ok() {
                    return Err("云端存档版本 ID 与本地已有版本冲突，已拒绝覆盖".to_string());
                }
            } else {
                store.save_versions.push(imported_version.clone());
                GameRepository::persist(app, store)?;
            }
            Ok(())
        })?;

        check_progress(&on_progress, 84, "存档已成功导入本地版本库")?;
        Ok(imported_version)
    }

    pub fn download_and_restore_cloud_save(
        app: &AppHandle,
        client: &BaiduNetdiskClient,
        game: &Game,
        profile: &SaveProfile,
        remote_version: &CloudSaveManifestVersion,
        on_progress: impl Fn(u8, &str) -> bool,
        confirm_targets: impl FnOnce(&RestorePlan) -> Result<(), String>,
        begin_commit: impl FnOnce() -> Result<(), String>,
    ) -> Result<SaveVersion, String> {
        SaveRepository::ensure_no_pending_restore(app, &game.game_uid)?;
        let imported = Self::download_and_import_save_version(
            app,
            client,
            game,
            remote_version,
            &on_progress,
        )?;
        let state = app.state::<crate::app_state::AppState>();
        state.ensure_save_restore_idle(&game.game_uid)?;
        let plan = SaveRepository::plan_restore(app, game, profile, &imported)?;
        check_progress(&on_progress, 84, "等待确认存档恢复目录")?;
        confirm_targets(&plan)?;
        state.ensure_save_restore_idle(&game.game_uid)?;
        check_progress(&on_progress, 85, "正在保护当前本地存档")?;
        let current_backup = protect_current_save_version(app, game, profile)?;
        check_progress(&on_progress, 89, "正在准备恢复物理存档")?;
        begin_commit()?;
        on_progress(90, "正在恢复物理存档文件");
        let receipt = SaveRepository::restore_confirmed(app, game, profile, plan, current_backup.as_ref(), |pct, msg| {
            let _ = on_progress(90 + (pct as f32 * 0.1) as u8, msg);
        })?;
        let state = app.state::<crate::app_state::AppState>();
        state.with_store_mut(|candidate| {
            let Some(game_record) = candidate
                .games
                .iter_mut()
                .find(|candidate_game| candidate_game.game_uid == game.game_uid)
            else {
                return Err(match SaveRepository::rollback_restore(receipt) {
                    Ok(()) => "游戏记录不存在，已回滚云端存档恢复".to_string(),
                    Err(error) => format!("游戏记录不存在，且云端存档回滚失败：{error}"),
                });
            };
            game_record.latest_save_version_id = Some(imported.version_id.clone());
            receipt.record_commit(candidate);
            if let Err(error) = GameRepository::persist(app, candidate) {
                return Err(match SaveRepository::rollback_restore(receipt) {
                    Ok(()) => format!("保存云端存档恢复结果失败，已回滚存档：{error}"),
                    Err(rollback_error) => format!(
                        "保存云端存档恢复结果失败，且存档回滚失败：{error}；{rollback_error}"
                    ),
                });
            }
            SaveRepository::finalize_restore(receipt, candidate);
            Ok(())
        })?;
        on_progress(100, "云端存档已成功还原");
        Ok(imported)
    }

    pub fn fetch_manifest(
        client: &BaiduNetdiskClient,
        game_key: &str,
        _game_uid: &str,
    ) -> Result<Option<CloudSaveManifest>, String> {
        Self::remote_manifest_path(game_key)?;
        let temp_dir = std::env::temp_dir();
        let temp_file = temp_dir.join(format!(
            "save-manifest-fetch-{}.json",
            Uuid::new_v4().simple()
        ));

        let _temporary_file = TemporaryFileGuard(temp_file.clone());
        let files = client.list_if_exists(&Self::remote_save_dir(game_key)?)?;
        manifest::read_manifest(game_key, files, |file| {
            client.download_file(file, &temp_file, |_, _| true)?;
            fs::read(&temp_file).map_err(|error| format!("读取云端存档清单失败：{error}"))
        })
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
        manifest::validate(manifest, game_key)?;
        let _temporary_file = TemporaryFileGuard(temp_file.clone());
        fs::write(&temp_file, &raw).map_err(|err| format!("写入临时云端存档清单失败：{err}"))?;

        let remote_manifest = Self::remote_manifest_path(game_key)?;
        let upload_res = client.upload_file(&temp_file, &remote_manifest, None, |_, _| true);
        let _ = fs::remove_file(&temp_file);
        upload_res.map(|_| ())
    }

    pub fn delete_cloud_version(
        client: &BaiduNetdiskClient,
        game_key: &str,
        game_uid: &str,
        version_id: &str,
    ) -> Result<CloudSaveManifest, String> {
        Self::remote_package_path(game_key, version_id)?;
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

        if let Some(version) = manifest
            .versions
            .iter()
            .find(|version| version.version_id == version_id)
        {
            let path = Self::verified_package_path(game_key, &version.package_path)?;
            client.delete_file(path)?;
        }
        manifest.versions.retain(|v| v.version_id != version_id);
        if manifest.latest_version_id.as_deref() == Some(version_id) {
            manifest.latest_version_id = manifest.versions.first().map(|v| v.version_id.clone());
        }
        manifest.updated_at = now_iso();
        Self::save_manifest(client, game_key, &manifest)?;
        Ok(manifest)
    }

    pub fn get_sync_status(
        app: &AppHandle,
        game: &Game,
    ) -> Result<CloudSaveSyncStatusView, String> {
        Ok(Self::get_overview(app, game)?.status)
    }

    pub fn get_overview(app: &AppHandle, game: &Game) -> Result<CloudSaveOverview, String> {
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
        drop(store);
        local_versions.sort_by(|a, b| compare_created_at(&b.created_at, &a.created_at));

        let latest_local = game.latest_save_version_id.as_deref().and_then(|id| {
            local_versions
                .iter()
                .find(|version| version.version_id == id)
        });
        let latest_local_created_at = latest_local.map(|v| v.created_at.clone());

        let client = BaiduNetdiskClient::load_configured(&data_dir);
        let Ok(client) = client else {
            return Ok(CloudSaveOverview {
                status: CloudSaveSyncStatusView {
                    auto_sync_save,
                    local_version_count: local_versions.len(),
                    cloud_version_count: 0,
                    latest_local_created_at,
                    latest_cloud_created_at: None,
                    latest_cloud_version_id: None,
                    sync_state: "offline".to_string(),
                    warnings: vec!["未连接或未授权百度网盘".to_string()],
                },
                versions: Vec::new(),
            });
        };

        let manifest = Self::fetch_manifest(&client, &game.game_key, &game.game_uid)?;
        let cloud_versions = manifest
            .as_ref()
            .map(|m| m.versions.clone())
            .unwrap_or_default();
        let status = build_sync_status(
            auto_sync_save,
            &local_versions,
            &cloud_versions,
            game.latest_save_version_id.as_deref(),
            manifest
                .as_ref()
                .and_then(|value| value.latest_version_id.as_deref()),
        );

        Ok(CloudSaveOverview {
            status,
            versions: cloud_versions,
        })
    }
}

fn build_sync_status(
    auto_sync_save: bool,
    local_versions: &[SaveVersion],
    cloud_versions: &[CloudSaveManifestVersion],
    current_local_id: Option<&str>,
    current_cloud_id: Option<&str>,
) -> CloudSaveSyncStatusView {
    let latest_local = current_local_id.and_then(|id| {
        local_versions
            .iter()
            .find(|version| version.version_id == id)
    });
    let latest_cloud = current_cloud_id
        .and_then(|id| {
            cloud_versions
                .iter()
                .find(|version| version.version_id == id)
        })
        .or_else(|| {
            cloud_versions
                .iter()
                .max_by(|left, right| compare_created_at(&left.created_at, &right.created_at))
        });
    let latest_cloud_created_at = latest_cloud.map(|v| v.created_at.clone());
    let latest_cloud_version_id = latest_cloud.map(|v| v.version_id.clone());

    let sync_state = match (latest_local, latest_cloud) {
        (None, None) => "synced",
        (Some(_), None) => "no_cloud_saves",
        (None, Some(_)) => "cloud_ahead",
        (Some(loc), Some(cld)) => {
            if loc.version_id == cld.version_id {
                "synced"
            } else if compare_created_at(&loc.created_at, &cld.created_at).is_gt() {
                "local_ahead"
            } else if compare_created_at(&loc.created_at, &cld.created_at).is_eq() {
                "conflict"
            } else {
                "cloud_ahead"
            }
        }
    };

    CloudSaveSyncStatusView {
        auto_sync_save,
        local_version_count: local_versions.len(),
        cloud_version_count: cloud_versions.len(),
        latest_local_created_at: latest_local.map(|v| v.created_at.clone()),
        latest_cloud_created_at,
        latest_cloud_version_id,
        sync_state: sync_state.to_string(),
        warnings: Vec::new(),
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

    package_entries(meta)
}

fn legacy_package_format() -> u32 {
    1
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn package_entries(meta: &CloudSavePackageMeta) -> Result<HashMap<String, (String, u64)>, String> {
    if !matches!(meta.format_version, 1 | 2) || meta.version.files.is_empty() {
        return Err("存档版本格式不支持或文件清单为空".to_string());
    }
    let mut identities = HashSet::new();
    let mut expected_entries = HashMap::new();
    for entry in &meta.version.files {
        let relative = normalize_package_relative_path(&entry.relative_path)?;
        let identity = serde_json::to_string(&(
            &entry.root_type,
            entry
                .root_path
                .as_deref()
                .unwrap_or("")
                .replace('\\', "/")
                .to_lowercase(),
            relative.to_lowercase(),
        ))
        .map_err(|error| format!("读取存档范围标识失败：{error}"))?;
        if !identities.insert(identity) {
            return Err(format!("存档版本包含重复的范围文件：{relative}"));
        }
        if entry.deleted {
            continue;
        }
        if entry.size > MAX_SAVE_PACKAGE_ENTRY_BYTES {
            return Err(format!("存档文件过大：{relative}"));
        }
        let hash = entry
            .object_hash
            .as_deref()
            .filter(|hash| valid_hash(hash))
            .ok_or_else(|| format!("存档版本缺少有效对象哈希：{relative}"))?
            .to_ascii_lowercase();
        let key = if meta.format_version == 2 {
            hash.clone()
        } else {
            relative.clone()
        };
        if let Some(previous) = expected_entries.insert(key, (hash.clone(), entry.size)) {
            if meta.format_version == 1 || previous != (hash, entry.size) {
                return Err(format!("存档版本包含冲突文件：{relative}"));
            }
        }
    }
    Ok(expected_entries)
}

fn check_progress(
    on_progress: &impl Fn(u8, &str) -> bool,
    percent: u8,
    message: &str,
) -> Result<(), String> {
    if on_progress(percent, message) {
        Ok(())
    } else {
        Err("任务已取消".to_string())
    }
}

fn import_archive_objects<Reader: Read + std::io::Seek>(
    archive: &mut ZipArchive<Reader>,
    expected: &HashMap<String, (String, u64)>,
    write_object: impl Fn(&str, &[u8]) -> Result<(), String>,
    on_progress: &impl Fn(u8, &str) -> bool,
) -> Result<(), String> {
    let mut imported = HashSet::new();
    let mut meta_count = 0;
    for index in 0..archive.len() {
        let percent = 71 + (imported.len() * 10 / expected.len().max(1)) as u8;
        check_progress(on_progress, percent, "正在校验并导入存档对象")?;
        let mut item = archive
            .by_index(index)
            .map_err(|error| format!("读取存档压缩项失败：{error}"))?;
        let name = item.name().to_string();
        if name == "meta.json" {
            meta_count += 1;
            if meta_count != 1 {
                return Err("存档包包含重复元数据".to_string());
            }
            continue;
        }
        if item.is_dir() {
            continue;
        }
        let key = validate_package_data_path(&name)?;
        let (hash, size) = expected
            .get(&key)
            .ok_or_else(|| format!("存档包包含未声明文件：{key}"))?;
        if !imported.insert(key.clone()) {
            return Err(format!("存档包包含重复文件：{key}"));
        }
        let declared = item.size();
        let data = read_zip_entry_cancellable(
            &mut item,
            declared,
            Some(*size),
            MAX_SAVE_PACKAGE_ENTRY_BYTES,
            &key,
            || on_progress(percent, "正在校验并导入存档对象"),
        )?;
        ensure_bytes_match_hash(&data, hash, &key)?;
        write_object(hash, &data)?;
    }
    if imported.len() != expected.len() || meta_count != 1 {
        return Err("存档包缺少清单声明的文件".to_string());
    }
    Ok(())
}

fn write_save_package(
    zip_path: &Path,
    meta: &CloudSavePackageMeta,
    read_entry: impl Fn(&SaveFileEntry) -> Result<Vec<u8>, String>,
    on_progress: &impl Fn(u8, &str) -> bool,
) -> Result<(PathBuf, String, u64), String> {
    let expected = package_entries(meta)?;
    let temporary = TemporaryFileGuard(zip_path.to_path_buf());
    let file =
        fs::File::create(zip_path).map_err(|error| format!("创建临时存档压缩文件失败：{error}"))?;
    let mut zip = ZipWriter::new(BufWriter::with_capacity(512 * 1024, file));
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("meta.json", options)
        .map_err(|error| format!("写入元数据失败：{error}"))?;
    let raw = serde_json::to_vec(meta).map_err(|error| format!("序列化存档元数据失败：{error}"))?;
    if raw.len() as u64 > MAX_SAVE_PACKAGE_META_BYTES {
        return Err("存档元数据过大，无法创建可恢复的存档包".to_string());
    }
    zip.write_all(&raw)
        .map_err(|error| format!("写入元数据失败：{error}"))?;
    let mut written = HashSet::new();
    for entry in meta.version.files.iter().filter(|entry| !entry.deleted) {
        let percent = 10 + (written.len() * 18 / expected.len().max(1)) as u8;
        check_progress(on_progress, percent, "正在打包本地存档")?;
        let hash = entry
            .object_hash
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase();
        let key = if meta.format_version == 2 {
            hash.clone()
        } else {
            normalize_package_relative_path(&entry.relative_path)?
        };
        if !written.insert(key.clone()) {
            continue;
        }
        let bytes = read_entry(entry)?;
        if bytes.len() as u64 != entry.size {
            return Err("本地存档对象大小与清单不符".to_string());
        }
        ensure_bytes_match_hash(&bytes, &hash, &entry.relative_path)?;
        zip.start_file(format!("data/{key}"), options)
            .map_err(|error| format!("写入存档文件项失败：{error}"))?;
        for chunk in bytes.chunks(256 * 1024) {
            check_progress(on_progress, percent, "正在压缩存档文件")?;
            zip.write_all(chunk)
                .map_err(|error| format!("写入存档文件失败：{error}"))?;
        }
    }
    let mut writer = zip
        .finish()
        .map_err(|error| format!("完成存档压缩包失败：{error}"))?;
    writer
        .flush()
        .map_err(|error| format!("刷新存档压缩包失败：{error}"))?;
    drop(writer);
    check_progress(on_progress, 29, "正在校验存档包")?;
    let size = fs::metadata(zip_path)
        .map_err(|error| error.to_string())?
        .len();
    let hash = sha256_file_cancellable(zip_path, || on_progress(29, "正在校验存档包"))?;
    temporary.disarm();
    Ok((zip_path.to_path_buf(), hash, size))
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
        || path.contains([':', '\0'])
        || path
            .replace('\\', "/")
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
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

/// 读取一个压缩项，并把读取量硬性限制在其声明大小之内。
///
/// **大小校验必须发生在读取之前**：高度压缩的条目会先把内存吃光，之后才轮到大小/哈希
/// 校验 —— 而这两个大小在读取前都已经知道（zip 头部 + 版本清单）。所以这里先比对
/// `declared_size`（zip 头部）与 `expected_size`（清单）再决定要不要读，接着用
/// `Read::take` 把实际读取量钉死：即便压缩包头部谎报了大小，也不可能读出超过声明值的
/// 字节（多读一个字节就会被 `take` 截断，随即在这里报错）。
///
/// `declared_size` 本身也来自压缩包，因此还需要 `max_size` 这道与清单无关的闸门
/// （代码审查 P2-2 / 存档管理审查 V6）。
#[cfg(test)]
fn read_zip_entry_bounded(
    item: &mut dyn Read,
    declared_size: u64,
    expected_size: Option<u64>,
    max_size: u64,
    label: &str,
) -> Result<Vec<u8>, String> {
    read_zip_entry_cancellable(item, declared_size, expected_size, max_size, label, || true)
}

fn read_zip_entry_cancellable(
    item: &mut dyn Read,
    declared_size: u64,
    expected_size: Option<u64>,
    max_size: u64,
    label: &str,
    should_continue: impl Fn() -> bool,
) -> Result<Vec<u8>, String> {
    if declared_size > max_size {
        return Err(format!(
            "{label} 过大（声明 {declared_size} 字节，上限 {max_size} 字节）"
        ));
    }
    if let Some(expected) = expected_size {
        if declared_size != expected {
            return Err(format!(
                "{label} 大小与清单不一致（压缩包声明 {declared_size} 字节，清单 {expected} 字节）"
            ));
        }
    }
    let mut data = Vec::new();
    let mut limited = item.take(declared_size.saturating_add(1));
    let mut buffer = vec![0; 256 * 1024];
    loop {
        if !should_continue() {
            return Err("任务已取消".to_string());
        }
        let count = limited
            .read(&mut buffer)
            .map_err(|err| format!("解压 {label} 失败：{err}"))?;
        if count == 0 {
            break;
        }
        data.extend_from_slice(&buffer[..count]);
    }
    if data.len() as u64 != declared_size {
        return Err(format!(
            "{label} 实际大小与声明不一致（声明 {declared_size} 字节，实际 {} 字节）",
            data.len()
        ));
    }
    Ok(data)
}

struct TemporaryFileGuard(PathBuf);

impl TemporaryFileGuard {
    fn disarm(mut self) {
        self.0 = PathBuf::new();
    }
}

impl Drop for TemporaryFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn protect_current_save_version(
    app: &AppHandle,
    game: &Game,
    profile: &SaveProfile,
) -> Result<Option<SaveVersion>, String> {
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
                .find(|version| &version.version_id == version_id && version.game_uid == game.game_uid)
                .cloned()
        })
    };
    let protected = SaveRepository::commit(app, game, profile, latest.as_ref(), |_, _| {})?;
    let Some(protected) = protected else {
        return Ok(latest);
    };

    let pending = protected.clone();
    let result = state.with_store_mut(|candidate| {
        let protected_id = protected.version_id.clone();
        candidate.save_versions.push(protected);
        let game_record = candidate
            .games
            .iter_mut()
            .find(|candidate_game| candidate_game.game_uid == game.game_uid)
            .ok_or_else(|| "游戏记录不存在".to_string())?;
        game_record.latest_save_version_id = Some(protected_id);
        GameRepository::persist(app, candidate)?;
        Ok(())
    });
    crate::repositories::release_pending_objects(&pending);
    result.map(|()| Some(pending))
}

/// 取出版本中某个条目的字节，并保证返回的字节与条目记录的哈希一致。
///
/// 优先读内容寻址存储（`read_object` 自带 SHA-256 校验）。对象缺失或损坏时回退读磁盘上的
/// 活文件 —— 但**必须复算哈希**：包内清单写的是条目原哈希（`version.clone()`），下载侧
/// 正是拿它逐项校验，这里若放行一份内容不符的数据，就会产出一份「看着存在、永远还原不了」
/// 的云端备份。宁可让上传失败，也不要留下假备份。
fn read_entry_bytes(
    app: &AppHandle,
    game: &Game,
    profile: &SaveProfile,
    entry: &SaveFileEntry,
) -> Result<Vec<u8>, String> {
    let hash = entry.object_hash.as_deref();
    let object = hash.map(|hash| SaveRepository::read_object(app, hash));
    resolve_entry_bytes(hash, &entry.relative_path, object, || {
        read_file_from_scope(game, profile, entry)
    })
}

/// 依据「对象读取结果」决定最终采用的字节，并保证结果与记录哈希一致。
///
/// 抽成纯函数是为了让「回退必须复算哈希」这条规则可以被测试守住 —— 对象读取本身依赖
/// `AppHandle`，单测里构造不出来。
///
/// - `hash`：条目记录的哈希；`None` 表示旧条目没有记录哈希（如 `deleted: true` 的墓碑）。
/// - `object`：`read_object` 的结果；`None` 表示没有记录哈希、无需读对象。
/// - `read_fallback`：回退读取磁盘活文件。
fn resolve_entry_bytes(
    hash: Option<&str>,
    relative_path: &str,
    object: Option<Result<Vec<u8>, String>>,
    read_fallback: impl FnOnce() -> Result<Vec<u8>, String>,
) -> Result<Vec<u8>, String> {
    let Some(hash) = hash else {
        return read_fallback();
    };
    match object {
        Some(Ok(bytes)) => Ok(bytes),
        Some(Err(object_error)) => {
            let bytes = read_fallback().map_err(|read_error| {
                format!("读取存档对象失败（{object_error}），回退读取磁盘存档也失败：{read_error}")
            })?;
            ensure_bytes_match_hash(&bytes, hash, relative_path)?;
            Ok(bytes)
        }
        None => read_fallback(),
    }
}

fn ensure_bytes_match_hash(bytes: &[u8], hash: &str, relative_path: &str) -> Result<(), String> {
    let expected = hash.to_ascii_lowercase();
    let actual = sha256_hex(bytes);
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "存档文件内容与版本记录不一致，已中止上传以免留下无法还原的云端备份：{relative_path}（记录 {expected}，实际 {actual}）"
        ))
    }
}

fn read_file_from_scope(
    game: &Game,
    profile: &SaveProfile,
    entry: &SaveFileEntry,
) -> Result<Vec<u8>, String> {
    let scope = crate::repositories::save_repository::find_scope_for_read(profile, entry)?;
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

fn sha256_file_cancellable(
    path: &Path,
    should_continue: impl Fn() -> bool,
) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|err| format!("读取存档压缩包失败：{err}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 512 * 1024];
    loop {
        if !should_continue() {
            return Err("任务已取消".to_string());
        }
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
            format_version: 1,
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
            CloudSaveService::remote_save_dir("game_test").unwrap(),
            "/apps/GameSaver/saves/game_test"
        );
        assert_eq!(
            CloudSaveService::remote_manifest_path("game_test").unwrap(),
            "/apps/GameSaver/saves/game_test/manifest.json"
        );
        assert_eq!(
            CloudSaveService::remote_package_path("game_test", "v123").unwrap(),
            "/apps/GameSaver/saves/game_test/v123.zip"
        );
    }

    /// 带空格的 game_key 是真实存在的（`derive_game_key` 保留空格），校验不能误伤它。
    #[test]
    fn remote_paths_still_accept_keys_with_spaces() {
        assert_eq!(
            CloudSaveService::remote_save_dir("black myth wukong").unwrap(),
            "/apps/GameSaver/saves/black myth wukong"
        );
    }

    /// 这些构造函数的结果会被送进百度的 `path` 参数直接作用于远端文件
    /// （`delete_cloud_version` 用它删远程包），所以含目录分隔符或 `..` 的输入必须被拒绝，
    /// 否则路径会离开「本游戏目录」这层。
    #[test]
    fn remote_paths_reject_segments_that_escape_the_game_directory() {
        for game_key in [
            "../../evil",
            "..",
            ".",
            "a/b",
            "a\\b",
            "",
            "  ",
            "game\nname",
        ] {
            assert!(
                CloudSaveService::remote_save_dir(game_key).is_err(),
                "accepted unsafe game_key: {game_key:?}"
            );
            assert!(
                CloudSaveService::remote_manifest_path(game_key).is_err(),
                "accepted unsafe game_key: {game_key:?}"
            );
            assert!(
                CloudSaveService::remote_package_path(game_key, "v1").is_err(),
                "accepted unsafe game_key: {game_key:?}"
            );
        }
        // version_id 同样是一段会被拼进远程路径的文本，且它经 IPC 从前端传入。
        for version_id in ["../../../other-game/x", "..", "a/b", "a\\b", "", "  "] {
            assert!(
                CloudSaveService::remote_package_path("game_test", version_id).is_err(),
                "accepted unsafe version_id: {version_id:?}"
            );
        }
    }

    /// 出口守卫：云端清单里的 `package_path` 是**下载来的 JSON 字段** —— `fetch_manifest`
    /// 反序列化后不校验任何字段就返回。它必须被证明落在本游戏的云存档目录内，才允许用于
    /// 剪枝删除与下载导入，否则就是把远端输入直接作用到远端文件上。
    #[test]
    fn manifest_package_paths_must_stay_inside_the_games_save_directory() {
        let directory = CloudSaveService::remote_save_dir("black myth wukong").unwrap();
        assert_eq!(directory, "/apps/GameSaver/saves/black myth wukong");

        // 规范形状：`upload_save_version` 回填的是上传返回的路径，而 `upload_file` 原样回显
        // 我们传进去的 `remote_package_path`（baidu_netdisk_service.rs 的 `path: remote_path`），
        // 所以正常清单就是长这样。含空格的目录必须能过 —— 非 ASCII 同理。
        for path in [
            format!("{directory}/v1.zip"),
            format!("{directory}/2026-09-14T10-00-00.zip"),
        ] {
            assert!(
                CloudSaveService::verified_package_path("black myth wukong", &path).is_ok(),
                "rejected a legitimate package path: {path:?}"
            );
            // 返回的必须是原样可用的路径 —— 调用方直接拿它去构造 RemoteFile，
            // 而不是回头再读字段（那会让守卫变成可以悄悄删掉的东西）。
            assert_eq!(
                CloudSaveService::verified_package_path("black myth wukong", &path).unwrap(),
                path.as_str(),
                "守卫必须回传原路径"
            );
        }

        // 远端可控的异常值：一个都不许过。
        for path in [
            format!("{directory}/../../../other/evil.zip"),
            format!("{directory}/../evil.zip"),
            format!("{directory}/sub/v1.zip"),
            format!("{directory}/v1.txt"),
            format!("{directory}/"),
            "/apps/GameSaver/saves/other game/v1.zip".to_string(),
            "/apps/GameSaver/games/black myth wukong/body/v1.zip".to_string(),
            "v1.zip".to_string(),
            String::new(),
        ] {
            assert!(
                CloudSaveService::verified_package_path("black myth wukong", &path).is_err(),
                "accepted an untrusted package path: {path:?}"
            );
        }
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

    #[test]
    fn sync_status_uses_shared_local_and_cloud_versions() {
        let local = vec![SaveVersion {
            version_id: "local-new".to_string(),
            game_uid: "game-1".to_string(),
            created_at: "200".to_string(),
            files: Vec::new(),
            total_bytes: 0,
        }];
        let mut cloud = remote_version("cloud-old");
        cloud.created_at = "100".to_string();

        let status =
            build_sync_status(true, &local, &[cloud], Some("local-new"), Some("cloud-old"));

        assert_eq!(status.sync_state, "local_ahead");
        assert_eq!(status.local_version_count, 1);
        assert_eq!(status.cloud_version_count, 1);
    }

    fn readable_meta() -> CloudSavePackageMeta {
        let mut meta = package_meta("test-game", "version-1");
        meta.format_version = 2;
        meta.version.files[0].relative_path = "slot.dat".into();
        meta.version.files[0].object_hash = Some(sha256_hex(b"first"));
        meta.version.files[0].size = 5;
        meta.version.total_bytes = 5;
        meta
    }

    fn read_test_meta(archive: &mut ZipArchive<fs::File>) -> CloudSavePackageMeta {
        serde_json::from_reader(archive.by_name("meta.json").unwrap()).unwrap()
    }

    #[test]
    fn cloud_save_package_roundtrip_keeps_same_named_files_in_different_scopes() {
        let dir = crate::test_support::TempWorkspace::new("cloud-save-roundtrip");
        let path = dir.join("save.zip");
        let mut meta = readable_meta();
        let mut second = meta.version.files[0].clone();
        second.root_path = Some(r"C:\Users\Source\AppData\LocalLow\OtherScope".into());
        second.object_hash = Some(sha256_hex(b"second"));
        second.size = 6;
        meta.version.files.push(second);
        let (written_path, hash, size) = write_save_package(
            &path,
            &meta,
            |entry| {
                Ok(if entry.size == 5 {
                    b"first".to_vec()
                } else {
                    b"second".to_vec()
                })
            },
            &|_, _| true,
        )
        .unwrap();
        assert_eq!(written_path, path);
        assert_eq!(size, fs::metadata(&path).unwrap().len());
        assert_eq!(hash, sha256_file_cancellable(&path, || true).unwrap());
        let mut archive = ZipArchive::new(fs::File::open(&path).unwrap()).unwrap();
        let decoded = read_test_meta(&mut archive);
        let game = Game::new_pending("Test", "E:/new-machine/game", "game.exe");
        let mut game = game;
        game.game_key = meta.game_key.clone();
        let expected =
            validate_downloaded_save_meta(&decoded, &game, &remote_version("version-1")).unwrap();
        let objects = std::cell::RefCell::new(HashMap::new());
        import_archive_objects(
            &mut archive,
            &expected,
            |hash, data| {
                objects.borrow_mut().insert(hash.to_string(), data.to_vec());
                Ok(())
            },
            &|_, _| true,
        )
        .unwrap();
        assert_eq!(objects.borrow().len(), 2);
        assert_eq!(objects.borrow()[&sha256_hex(b"first")], b"first");
        assert_eq!(objects.borrow()[&sha256_hex(b"second")], b"second");
        assert_ne!(
            decoded.version.files[0].root_path,
            decoded.version.files[1].root_path
        );
    }

    #[test]
    fn cloud_save_package_deduplicates_same_object_across_scopes() {
        let dir = crate::test_support::TempWorkspace::new("cloud-save-dedup");
        let mut meta = readable_meta();
        let mut second = meta.version.files[0].clone();
        second.root_path = Some("D:/other".into());
        meta.version.files.push(second);
        let path = dir.join("save.zip");
        write_save_package(&path, &meta, |_| Ok(b"first".to_vec()), &|_, _| true).unwrap();
        let mut archive = ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
        assert_eq!(archive.len(), 2);
        import_archive_objects(
            &mut archive,
            &package_entries(&meta).unwrap(),
            |_, _| Ok(()),
            &|_, _| true,
        )
        .unwrap();
        meta.version.files[1].size = 9;
        assert!(package_entries(&meta).is_err());
    }

    #[test]
    fn cloud_save_legacy_package_still_roundtrips() {
        let dir = crate::test_support::TempWorkspace::new("cloud-save-legacy");
        let mut meta = readable_meta();
        meta.format_version = 1;
        let path = dir.join("save.zip");
        write_save_package(&path, &meta, |_| Ok(b"first".to_vec()), &|_, _| true).unwrap();
        let mut archive = ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
        assert!(archive.by_name("data/slot.dat").is_ok());
        import_archive_objects(
            &mut archive,
            &package_entries(&meta).unwrap(),
            |_, data| {
                assert_eq!(data, b"first");
                Ok(())
            },
            &|_, _| true,
        )
        .unwrap();
        let mut raw = serde_json::to_value(&meta).unwrap();
        raw.as_object_mut().unwrap().remove("formatVersion");
        assert_eq!(
            serde_json::from_value::<CloudSavePackageMeta>(raw)
                .unwrap()
                .format_version,
            1
        );
    }

    #[test]
    fn cloud_save_failed_and_cancelled_packaging_removes_temporary_zip() {
        let dir = crate::test_support::TempWorkspace::new("cloud-save-cleanup");
        let path = dir.join("save.zip");
        let meta = readable_meta();
        assert!(write_save_package(
            &path,
            &meta,
            |_| Err("missing CAS object".into()),
            &|_, _| true
        )
        .is_err());
        assert!(!path.exists());
        assert!(write_save_package(&path, &meta, |_| Ok(b"wrong".to_vec()), &|_, _| true).is_err());
        assert!(!path.exists());
        let calls = std::cell::Cell::new(0);
        let result = write_save_package(&path, &meta, |_| Ok(b"first".to_vec()), &|_, _| {
            calls.set(calls.get() + 1);
            calls.get() < 2
        });
        assert_eq!(result.unwrap_err(), "任务已取消");
        assert!(!path.exists());
    }

    #[test]
    fn cloud_save_deletion_only_version_roundtrips_but_empty_or_unsafe_meta_is_rejected() {
        let dir = crate::test_support::TempWorkspace::new("cloud-save-tombstone");
        let path = dir.join("save.zip");
        let mut meta = readable_meta();
        meta.version.files[0].deleted = true;
        meta.version.files[0].object_hash = None;
        write_save_package(
            &path,
            &meta,
            |_| panic!("tombstones do not read objects"),
            &|_, _| true,
        )
        .unwrap();
        let mut archive = ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
        let decoded = read_test_meta(&mut archive);
        let expected = package_entries(&decoded).unwrap();
        assert!(expected.is_empty());
        import_archive_objects(
            &mut archive,
            &expected,
            |_, _| panic!("no objects"),
            &|_, _| true,
        )
        .unwrap();
        assert!(decoded.version.files[0].deleted);
        meta.version.files[0].relative_path = "../outside.dat".into();
        assert!(package_entries(&meta).is_err());
        meta.version.files.clear();
        assert!(package_entries(&meta).is_err());
    }

    #[test]
    fn cloud_save_import_cancellation_never_writes_an_object() {
        let dir = crate::test_support::TempWorkspace::new("cloud-save-cancel-import");
        let path = dir.join("save.zip");
        let meta = readable_meta();
        write_save_package(&path, &meta, |_| Ok(b"first".to_vec()), &|_, _| true).unwrap();
        let mut archive = ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
        let result = import_archive_objects(
            &mut archive,
            &package_entries(&meta).unwrap(),
            |_, _| panic!("cancelled"),
            &|_, _| false,
        );
        assert_eq!(result.unwrap_err(), "任务已取消");
        let mut reader = ProbeReader::new(b"first");
        assert_eq!(
            read_zip_entry_cancellable(&mut reader, 5, Some(5), 100, "slot.dat", || false)
                .unwrap_err(),
            "任务已取消"
        );
        assert_eq!(reader.read_calls, 0);
    }

    #[test]
    fn cloud_save_sync_uses_current_baseline_instead_of_protective_snapshot() {
        let mut old = readable_meta().version;
        old.version_id = "current".into();
        old.created_at = "100".into();
        let mut protective = old.clone();
        protective.version_id = "protective".into();
        protective.created_at = "200".into();
        let mut remote = remote_version("current");
        remote.created_at = "100".into();
        let status = build_sync_status(
            true,
            &[protective, old],
            &[remote],
            Some("current"),
            Some("current"),
        );
        assert_eq!(status.sync_state, "synced");
        assert_eq!(status.local_version_count, 2);
        assert_eq!(status.latest_local_created_at.as_deref(), Some("100"));
    }

    #[test]
    fn cloud_save_equal_timestamps_do_not_prove_equal_content() {
        let local = readable_meta().version;
        let remote = remote_version("different-id");
        let status = build_sync_status(
            true,
            &[local],
            &[remote],
            Some("version-1"),
            Some("different-id"),
        );
        assert_eq!(status.sync_state, "conflict");
    }

    #[test]
    fn resolve_entry_bytes_verifies_the_fallback_against_the_recorded_hash() {
        let bytes = b"save-data".to_vec();
        let hash = sha256_hex(&bytes);

        // 对象还在 → 直接用对象字节，不该回退读盘。
        let from_object =
            resolve_entry_bytes(Some(&hash), "save.dat", Some(Ok(bytes.clone())), || {
                panic!("对象可用时不应回退读盘")
            });
        assert_eq!(from_object.expect("对象可用应当成功"), bytes);

        // 对象缺失 + 磁盘内容与记录一致 → 允许回退。
        let recovered = resolve_entry_bytes(
            Some(&hash),
            "save.dat",
            Some(Err("存档对象不存在".to_string())),
            || Ok(bytes.clone()),
        );
        assert_eq!(recovered.expect("磁盘内容一致应当回退成功"), bytes);

        // 对象缺失 + 磁盘内容已漂移 → 必须失败，绝不能打包出一份假备份。
        let drifted = b"save-data-after-drift".to_vec();
        let result = resolve_entry_bytes(
            Some(&hash),
            "save.dat",
            Some(Err("存档对象不存在".to_string())),
            || Ok(drifted.clone()),
        );
        assert!(result.is_err(), "磁盘内容与记录不符时必须中止上传");

        // 旧条目没有记录哈希（无对象可读）→ 直接读盘。
        let legacy = resolve_entry_bytes(None, "save.dat", None, || Ok(bytes.clone()));
        assert_eq!(legacy.expect("无哈希条目应当直接读盘"), bytes);
    }

    #[test]
    fn recorded_hash_comparison_ignores_letter_case() {
        let bytes = b"save-data".to_vec();
        let hash = sha256_hex(&bytes);
        assert!(ensure_bytes_match_hash(&bytes, &hash, "save.dat").is_ok());
        assert!(ensure_bytes_match_hash(&bytes, &hash.to_ascii_uppercase(), "save.dat").is_ok());
        assert!(ensure_bytes_match_hash(&bytes, &"0".repeat(64), "save.dat").is_err());
    }

    /// 记录 `read` 调用次数的读取器，用来证明「大小校验发生在读取之前」。
    struct ProbeReader {
        data: Vec<u8>,
        position: usize,
        read_calls: usize,
    }

    impl ProbeReader {
        fn new(data: &[u8]) -> Self {
            Self {
                data: data.to_vec(),
                position: 0,
                read_calls: 0,
            }
        }
    }

    impl Read for ProbeReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.read_calls += 1;
            let remaining = &self.data[self.position..];
            let take = remaining.len().min(buf.len());
            buf[..take].copy_from_slice(&remaining[..take]);
            self.position += take;
            Ok(take)
        }
    }

    #[test]
    fn bounded_read_rejects_size_mismatch_without_reading_any_byte() {
        // 压缩包头部声明 8 字节，版本清单写的是 4 字节 —— 在校验之前就该被拦下。
        let mut reader = ProbeReader::new(b"abcdefgh");
        let result = read_zip_entry_bounded(&mut reader, 8, Some(4), 1024, "slot1.sav");
        let error = result.expect_err("大小与清单不一致时必须拒绝");
        assert!(error.contains("大小与清单不一致"), "实际错误：{error}");
        assert_eq!(
            reader.read_calls, 0,
            "拒绝必须发生在读取之前，不能先分配内存"
        );
    }

    #[test]
    fn bounded_read_rejects_oversized_entry_without_reading() {
        // 与清单无关的最后一道闸门：压缩包自称的大小超过上限。
        let mut reader = ProbeReader::new(b"");
        let result = read_zip_entry_bounded(
            &mut reader,
            MAX_SAVE_PACKAGE_ENTRY_BYTES + 1,
            None,
            MAX_SAVE_PACKAGE_ENTRY_BYTES,
            "slot1.sav",
        );
        let error = result.expect_err("超过硬上限时必须拒绝");
        assert!(error.contains("过大"), "实际错误：{error}");
        assert_eq!(reader.read_calls, 0, "拒绝必须发生在读取之前");
    }

    #[test]
    fn bounded_read_stops_at_declared_size_when_the_header_lies() {
        // 头部声明 4 字节，实际解压流有 6 字节 —— `take` 截断，长度校验随即失败，
        // 而不是把 6 字节全读进内存。
        let mut reader = ProbeReader::new(b"abcdef");
        let result = read_zip_entry_bounded(&mut reader, 4, Some(4), 1024, "slot1.sav");
        let error = result.expect_err("实际长度超出声明时必须拒绝");
        assert!(error.contains("实际大小与声明不一致"), "实际错误：{error}");
        // 关键：不是「读完再发现超了」，而是读取量本身被钉在声明值上（最多多读 1 字节
        // 用来判定超出）。少了 `take`，这里会读到 6 字节。
        assert!(
            reader.position <= 5,
            "读取量必须被截断在声明大小附近，实际读了 {} 字节",
            reader.position
        );
    }

    #[test]
    fn bounded_read_returns_exact_bytes_when_sizes_agree() {
        let mut reader = ProbeReader::new(b"abcdef");
        let bytes = read_zip_entry_bounded(&mut reader, 6, Some(6), 1024, "slot1.sav")
            .expect("应当读取成功");
        assert_eq!(bytes, b"abcdef");
    }

    #[test]
    fn bounded_read_without_a_manifest_size_only_applies_the_cap() {
        // meta.json 没有清单可对，只受上限约束。
        let mut reader = ProbeReader::new(b"{}");
        let bytes = read_zip_entry_bounded(
            &mut reader,
            2,
            None,
            MAX_SAVE_PACKAGE_META_BYTES,
            "meta.json",
        )
        .expect("应当读取成功");
        assert_eq!(bytes, b"{}");
    }
}
