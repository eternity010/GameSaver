use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};
use uuid::Uuid;
use walkdir::WalkDir;
use zip::{write::SimpleFileOptions, ZipArchive, ZipWriter};

const MANIFEST_PATH: &str = ".gamesaver/body-manifest.json";
const PACKAGE_FORMAT_VERSION: u32 = 1;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BodyPackageFile {
    pub relative_path: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BodyPackageManifest {
    pub format_version: u32,
    pub game_uid: String,
    pub version_id: String,
    pub file_count: usize,
    pub total_bytes: u64,
    pub excluded_items: Vec<String>,
    pub files: Vec<BodyPackageFile>,
}

pub struct BodyPackageResult {
    pub package_path: PathBuf,
    pub manifest: BodyPackageManifest,
    pub sha256: String,
}

pub struct BodyPackageService;

impl BodyPackageService {
    pub fn package_path(cache_root: &Path, game_uid: &str, version_id: &str) -> PathBuf {
        cache_root.join(game_uid).join(format!("{version_id}.zip"))
    }

    pub fn cleanup_temporary_packages(cache_root: &Path) -> Result<usize, String> {
        if !cache_root.is_dir() {
            return Ok(0);
        }
        let mut candidates = Vec::new();
        for entry in WalkDir::new(cache_root).follow_links(false) {
            let entry = entry.map_err(|err| format!("扫描本体包临时文件失败：{err}"))?;
            let name = entry.file_name().to_string_lossy();
            let is_temporary_file = entry.file_type().is_file()
                && name.starts_with('.')
                && (name.contains(".tmp") || name.starts_with(".download-"));
            let is_temporary_manifest =
                entry.file_type().is_dir() && name.starts_with('.') && name.contains(".manifest-");
            if is_temporary_file || is_temporary_manifest {
                candidates.push((entry.path().to_path_buf(), entry.file_type().is_dir()));
            }
        }
        candidates.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
        let mut removed = 0;
        for (path, is_directory) in candidates {
            if is_directory {
                fs::remove_dir_all(&path).map_err(|err| {
                    format!("清理本体包临时目录失败（{}）：{err}", path.display())
                })?;
            } else {
                fs::remove_file(&path).map_err(|err| {
                    format!("清理本体包临时文件失败（{}）：{err}", path.display())
                })?;
            }
            removed += 1;
        }
        Ok(removed)
    }

    pub fn cleanup_orphan_packages(
        cache_root: &Path,
        referenced_paths: &HashSet<String>,
    ) -> Result<usize, String> {
        if !cache_root.is_dir() {
            return Ok(0);
        }
        let mut removed = 0;
        for entry in WalkDir::new(cache_root).follow_links(false) {
            let entry = entry.map_err(|err| format!("扫描孤立本体包失败：{err}"))?;
            if !entry.file_type().is_file()
                || entry
                    .path()
                    .extension()
                    .is_none_or(|extension| !extension.eq_ignore_ascii_case("zip"))
            {
                continue;
            }
            if referenced_paths.contains(&path_key(entry.path())) {
                continue;
            }
            fs::remove_file(entry.path()).map_err(|err| {
                format!("清理孤立本体包失败（{}）：{err}", entry.path().display())
            })?;
            removed += 1;
        }
        Ok(removed)
    }

    pub fn create_package_with_exclusions(
        source_root: &Path,
        cache_root: &Path,
        game_uid: &str,
        version_id: &str,
        executable_relative_path: &str,
        protected_paths: &[String],
        on_progress: impl Fn(u8, &str),
        is_cancelled: impl Fn() -> bool + Sync,
    ) -> Result<BodyPackageResult, String> {
        match Self::create_package_with_7zip(
            source_root,
            cache_root,
            game_uid,
            version_id,
            executable_relative_path,
            protected_paths,
            &on_progress,
            &is_cancelled,
        ) {
            Ok(result) => Ok(result),
            Err(error) if error.starts_with("7ZIP_UNAVAILABLE:") => {
                Self::create_package_with_rust_zip(
                    source_root,
                    cache_root,
                    game_uid,
                    version_id,
                    executable_relative_path,
                    protected_paths,
                    on_progress,
                    is_cancelled,
                )
            }
            Err(error) => Err(error),
        }
    }

    fn create_package_with_7zip(
        source_root: &Path,
        cache_root: &Path,
        game_uid: &str,
        version_id: &str,
        executable_relative_path: &str,
        protected_paths: &[String],
        on_progress: &impl Fn(u8, &str),
        is_cancelled: &(impl Fn() -> bool + Sync),
    ) -> Result<BodyPackageResult, String> {
        let archiver =
            find_bundled_7zip().ok_or_else(|| "7ZIP_UNAVAILABLE:未找到内置 7-Zip".to_string())?;
        let source_root = source_root
            .canonicalize()
            .map_err(|err| format!("解析游戏本体目录失败：{err}"))?;
        if !source_root.is_dir() {
            return Err("游戏本体目录不存在或不可访问".to_string());
        }
        let files = collect_files(&source_root, protected_paths)?;
        if files.is_empty() {
            return Err("游戏本体目录中没有可打包的文件".to_string());
        }
        let executable = normalize_relative(executable_relative_path)?;
        if !files.iter().any(|file| file.relative_path == executable) {
            return Err("游戏启动程序被排除或不存在，无法创建本体包".to_string());
        }
        on_progress(5, &format!("已发现 {} 个本体文件", files.len()));

        let package_path = Self::package_path(cache_root, game_uid, version_id);
        let parent = package_path
            .parent()
            .ok_or_else(|| "本体包路径无父目录".to_string())?;
        fs::create_dir_all(parent).map_err(|err| format!("创建本体包缓存目录失败：{err}"))?;
        let temporary = parent.join(format!(".{version_id}.zip.tmp-{}", Uuid::new_v4().simple()));
        let list_path = parent.join(format!(
            ".{version_id}.files.tmp-{}",
            Uuid::new_v4().simple()
        ));
        let manifest_root = parent.join(format!(
            ".{version_id}.manifest-{}",
            Uuid::new_v4().simple()
        ));
        let result = (|| -> Result<BodyPackageResult, String> {
            if is_cancelled() {
                return Err("任务已取消".to_string());
            }
            for file in &files {
                if file.relative_path.contains(['\r', '\n']) {
                    return Err(format!(
                        "游戏文件路径包含不支持的换行符：{}",
                        file.relative_path
                    ));
                }
            }
            let list_bytes = format!(
                "{}\n",
                files
                    .iter()
                    .map(|file| file.relative_path.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            fs::write(&list_path, list_bytes.as_bytes())
                .map_err(|err| format!("写入 7-Zip 文件清单失败：{err}"))?;

            on_progress(8, "正在使用 7-Zip 压缩游戏本体");
            run_7zip(
                &archiver,
                &source_root,
                [
                    "a".to_string(),
                    "-tzip".to_string(),
                    "-mx=1".to_string(),
                    "-mmt=on".to_string(),
                    "-bso0".to_string(),
                    "-bsp1".to_string(),
                    "-bse2".to_string(),
                    "-y".to_string(),
                    temporary.to_string_lossy().to_string(),
                    format!("@{}", list_path.display()),
                    "-scsUTF-8".to_string(),
                ]
                .to_vec(),
                |percent| {
                    let progress = 8 + (u16::from(percent) * 80 / 100) as u8;
                    on_progress(progress.min(88), &format!("正在压缩游戏本体 {percent}%"));
                },
                is_cancelled,
            )?;
            if is_cancelled() {
                return Err("任务已取消".to_string());
            }
            on_progress(90, "正在生成本体包清单");
            let manifest_files = read_archive_file_index(&temporary)?;
            if !same_relative_paths(&files, &manifest_files) {
                return Err("7-Zip 输出的文件列表与源目录不一致".to_string());
            }
            let total_bytes = manifest_files
                .iter()
                .fold(0u64, |total, file| total.saturating_add(file.size));
            let manifest = BodyPackageManifest {
                format_version: PACKAGE_FORMAT_VERSION,
                game_uid: game_uid.to_string(),
                version_id: version_id.to_string(),
                file_count: manifest_files.len(),
                total_bytes,
                excluded_items: protected_paths
                    .iter()
                    .map(|path| format!("存档范围：{path}"))
                    .collect(),
                files: manifest_files,
            };
            let manifest_bytes = serde_json::to_vec(&manifest)
                .map_err(|err| format!("序列化本体包清单失败：{err}"))?;
            fs::create_dir_all(manifest_root.join(".gamesaver"))
                .map_err(|err| format!("创建本体包清单暂存目录失败：{err}"))?;
            fs::write(manifest_root.join(MANIFEST_PATH), manifest_bytes)
                .map_err(|err| format!("写入本体包清单暂存文件失败：{err}"))?;

            sync_file(&temporary, "刷新 7-Zip 本体包")?;
            on_progress(92, "正在写入本体包清单");
            run_7zip(
                &archiver,
                &manifest_root,
                [
                    "a".to_string(),
                    "-tzip".to_string(),
                    "-mx=0".to_string(),
                    "-bso0".to_string(),
                    "-bsp1".to_string(),
                    "-bse2".to_string(),
                    "-y".to_string(),
                    temporary.to_string_lossy().to_string(),
                    MANIFEST_PATH.to_string(),
                ]
                .to_vec(),
                |percent| {
                    let progress = 92 + (u16::from(percent) * 6 / 100) as u8;
                    on_progress(progress.min(98), &format!("正在写入本体包清单 {percent}%"));
                },
                is_cancelled,
            )?;
            sync_file(&temporary, "刷新本体包清单")?;
            on_progress(98, "正在校验本体 ZIP");
            let package_hash = hash_file(&temporary)?;
            fs::rename(&temporary, &package_path)
                .map_err(|err| format!("提交本体 ZIP 缓存失败：{err}"))?;
            on_progress(
                100,
                &format!("本体 ZIP 已生成，{} 个文件", manifest.file_count),
            );
            Ok(BodyPackageResult {
                package_path,
                manifest,
                sha256: package_hash,
            })
        })();
        let _ = fs::remove_file(&list_path);
        let _ = fs::remove_dir_all(&manifest_root);
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn create_package_with_rust_zip(
        source_root: &Path,
        cache_root: &Path,
        game_uid: &str,
        version_id: &str,
        executable_relative_path: &str,
        protected_paths: &[String],
        on_progress: impl Fn(u8, &str),
        is_cancelled: impl Fn() -> bool,
    ) -> Result<BodyPackageResult, String> {
        let source_root = source_root
            .canonicalize()
            .map_err(|err| format!("解析游戏本体目录失败：{err}"))?;
        if !source_root.is_dir() {
            return Err("游戏本体目录不存在或不可访问".to_string());
        }
        let files = collect_files(&source_root, protected_paths)?;
        if files.is_empty() {
            return Err("游戏本体目录中没有可打包的文件".to_string());
        }
        let executable = normalize_relative(executable_relative_path)?;
        if !files.iter().any(|file| file.relative_path == executable) {
            return Err("游戏启动程序被排除或不存在，无法创建本体包".to_string());
        }
        on_progress(5, &format!("已发现 {} 个本体文件", files.len()));

        let package_path = Self::package_path(cache_root, game_uid, version_id);
        let parent = package_path
            .parent()
            .ok_or_else(|| "本体包路径无父目录".to_string())?;
        fs::create_dir_all(parent).map_err(|err| format!("创建本体包缓存目录失败：{err}"))?;
        let temporary = parent.join(format!(".{version_id}.zip.tmp-{}", Uuid::new_v4().simple()));
        let result = (|| -> Result<BodyPackageResult, String> {
            let file = fs::File::create(&temporary)
                .map_err(|err| format!("创建本体 ZIP 临时文件失败：{err}"))?;
            let mut writer = ZipWriter::new(file);
            let options =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            let mut manifest_files = Vec::with_capacity(files.len());
            let mut total_bytes = 0u64;
            for (index, file) in files.iter().enumerate() {
                if is_cancelled() {
                    return Err("任务已取消".to_string());
                }
                let source = source_root.join(
                    file.relative_path
                        .replace('/', std::path::MAIN_SEPARATOR_STR),
                );
                let mut input = fs::File::open(&source)
                    .map_err(|err| format!("读取游戏文件失败（{}）：{err}", file.relative_path))?;
                let mut size = 0u64;
                writer
                    .start_file(&file.relative_path, options)
                    .map_err(|err| {
                        format!("写入本体 ZIP 条目失败（{}）：{err}", file.relative_path)
                    })?;
                let mut buffer = vec![0u8; 1024 * 1024];
                loop {
                    let read = input.read(&mut buffer).map_err(|err| {
                        format!("读取游戏文件失败（{}）：{err}", file.relative_path)
                    })?;
                    if read == 0 {
                        break;
                    }
                    writer.write_all(&buffer[..read]).map_err(|err| {
                        format!("写入本体 ZIP 数据失败（{}）：{err}", file.relative_path)
                    })?;
                    size = size.saturating_add(read as u64);
                }
                if size != file.size {
                    return Err(format!(
                        "游戏文件在打包过程中发生变化：{}",
                        file.relative_path
                    ));
                }
                total_bytes = total_bytes.saturating_add(size);
                manifest_files.push(BodyPackageFile {
                    relative_path: file.relative_path.clone(),
                    size,
                });
                let progress = 8 + (((index + 1) * 82) / files.len().max(1)) as u8;
                on_progress(
                    progress.min(90),
                    &format!("正在压缩本体文件 {}/{}", index + 1, files.len()),
                );
            }
            let manifest = BodyPackageManifest {
                format_version: PACKAGE_FORMAT_VERSION,
                game_uid: game_uid.to_string(),
                version_id: version_id.to_string(),
                file_count: manifest_files.len(),
                total_bytes,
                excluded_items: protected_paths
                    .iter()
                    .map(|path| format!("存档范围：{path}"))
                    .collect(),
                files: manifest_files,
            };
            let manifest_bytes = serde_json::to_vec(&manifest)
                .map_err(|err| format!("序列化本体包清单失败：{err}"))?;
            writer
                .start_file(MANIFEST_PATH, options)
                .map_err(|err| format!("写入本体包清单失败：{err}"))?;
            writer
                .write_all(&manifest_bytes)
                .map_err(|err| format!("写入本体包清单失败：{err}"))?;
            let output = writer
                .finish()
                .map_err(|err| format!("完成本体 ZIP 失败：{err}"))?;
            output
                .sync_all()
                .map_err(|err| format!("刷新本体 ZIP 失败：{err}"))?;
            on_progress(98, "正在校验本体 ZIP");
            let package_hash = hash_file(&temporary)?;
            fs::rename(&temporary, &package_path)
                .map_err(|err| format!("提交本体 ZIP 缓存失败：{err}"))?;
            on_progress(
                100,
                &format!("本体 ZIP 已生成，{} 个文件", manifest.file_count),
            );
            Ok(BodyPackageResult {
                package_path,
                manifest,
                sha256: package_hash,
            })
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub fn validate_package(
        package_path: &Path,
        game_uid: &str,
        executable_relative_path: &str,
        expected_package_sha256: Option<&str>,
    ) -> Result<BodyPackageManifest, String> {
        let actual_hash = hash_file(package_path)?;
        if let Some(expected) = expected_package_sha256 {
            if actual_hash != expected {
                return Err("本体 ZIP 哈希校验失败".to_string());
            }
        }
        let file =
            fs::File::open(package_path).map_err(|err| format!("打开本体 ZIP 失败：{err}"))?;
        let mut archive =
            ZipArchive::new(file).map_err(|err| format!("读取本体 ZIP 失败：{err}"))?;
        let manifest = read_manifest(&mut archive)?;
        if manifest.format_version != PACKAGE_FORMAT_VERSION {
            return Err(format!("不支持的本体包格式：{}", manifest.format_version));
        }
        if manifest.game_uid != game_uid {
            return Err("本体包不属于当前游戏".to_string());
        }
        let executable = normalize_relative(executable_relative_path)?;
        if manifest.file_count != manifest.files.len() || manifest.file_count == 0 {
            return Err("本体包清单中的文件数量无效".to_string());
        }
        if !manifest
            .files
            .iter()
            .any(|item| item.relative_path == executable)
        {
            return Err("本体包缺少游戏启动程序".to_string());
        }
        validate_archive_entries(&archive, &manifest)?;
        Ok(manifest)
    }

    pub fn validate_package_for_upload(
        package_path: &Path,
        game_uid: &str,
        executable_relative_path: &str,
        expected_package_sha256: Option<&str>,
    ) -> Result<(), String> {
        if let Some(expected) = expected_package_sha256 {
            let actual_hash = hash_file(package_path)?;
            if actual_hash != expected {
                return Err("本体 ZIP 哈希校验失败".to_string());
            }
            return Ok(());
        }
        Self::validate_package(package_path, game_uid, executable_relative_path, None).map(|_| ())
    }

    pub fn extract_package(
        package_path: &Path,
        staging_root: &Path,
        game_uid: &str,
        executable_relative_path: &str,
        expected_package_sha256: Option<&str>,
        on_progress: impl Fn(u8, &str),
        is_cancelled: impl Fn() -> bool,
    ) -> Result<BodyPackageManifest, String> {
        let expected_executable = normalize_relative(executable_relative_path)?;
        let result = (|| -> Result<BodyPackageManifest, String> {
            let expected_package_hash = hash_file(package_path)?;
            if expected_package_hash.is_empty() {
                return Err("本体 ZIP 哈希为空".to_string());
            }
            if let Some(expected) = expected_package_sha256 {
                if expected_package_hash != expected {
                    return Err("本体 ZIP 哈希校验失败".to_string());
                }
            }
            let file =
                fs::File::open(package_path).map_err(|err| format!("打开本体 ZIP 失败：{err}"))?;
            let mut archive =
                ZipArchive::new(file).map_err(|err| format!("读取本体 ZIP 失败：{err}"))?;
            let manifest = read_manifest(&mut archive)?;
            if manifest.format_version != PACKAGE_FORMAT_VERSION {
                return Err(format!("不支持的本体包格式：{}", manifest.format_version));
            }
            if manifest.game_uid != game_uid {
                return Err("本体包不属于当前游戏".to_string());
            }
            if manifest.file_count != manifest.files.len() || manifest.file_count == 0 {
                return Err("本体包清单中的文件数量无效".to_string());
            }
            if !manifest
                .files
                .iter()
                .any(|item| item.relative_path == expected_executable)
            {
                return Err("本体包缺少游戏启动程序".to_string());
            }
            validate_archive_entries(&archive, &manifest)?;
            if staging_root.exists() {
                return Err("本体恢复暂存目录已存在".to_string());
            }
            fs::create_dir_all(staging_root)
                .map_err(|err| format!("创建本体恢复暂存目录失败：{err}"))?;
            let extract_result = (|| -> Result<(), String> {
                let mut written_bytes = 0u64;
                for (index, expected) in manifest.files.iter().enumerate() {
                    if is_cancelled() {
                        return Err("任务已取消".to_string());
                    }
                    let target = safe_join(staging_root, &expected.relative_path)?;
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent)
                            .map_err(|err| format!("创建本体恢复目录失败：{err}"))?;
                    }
                    let temporary = target.with_file_name(format!(
                        ".{}.part-{}",
                        target.file_name().unwrap_or_default().to_string_lossy(),
                        Uuid::new_v4().simple()
                    ));
                    let mut input = archive.by_name(&expected.relative_path).map_err(|err| {
                        format!("读取本体 ZIP 条目失败（{}）：{err}", expected.relative_path)
                    })?;
                    let mut output = fs::File::create(&temporary).map_err(|err| {
                        format!("创建本体恢复文件失败（{}）：{err}", expected.relative_path)
                    })?;
                    let mut size = 0u64;
                    let mut buffer = vec![0u8; 1024 * 1024];
                    loop {
                        let read = input.read(&mut buffer).map_err(|err| {
                            format!("解压本体文件失败（{}）：{err}", expected.relative_path)
                        })?;
                        if read == 0 {
                            break;
                        }
                        output.write_all(&buffer[..read]).map_err(|err| {
                            format!("写入本体恢复文件失败（{}）：{err}", expected.relative_path)
                        })?;
                        size = size.saturating_add(read as u64);
                    }
                    output.sync_all().map_err(|err| {
                        format!("刷新本体恢复文件失败（{}）：{err}", expected.relative_path)
                    })?;
                    if size != expected.size {
                        let _ = fs::remove_file(&temporary);
                        return Err(format!("本体文件大小不一致：{}", expected.relative_path));
                    }
                    fs::rename(&temporary, &target).map_err(|err| {
                        format!("提交本体恢复文件失败（{}）：{err}", expected.relative_path)
                    })?;
                    written_bytes = written_bytes.saturating_add(size);
                    let progress = 8 + (((index + 1) * 90) / manifest.files.len().max(1)) as u8;
                    on_progress(
                        progress.min(98),
                        &format!("正在解压本体文件 {}/{}", index + 1, manifest.files.len()),
                    );
                }
                if written_bytes != manifest.total_bytes {
                    return Err("本体包总大小校验失败".to_string());
                }
                Ok(())
            })();
            if extract_result.is_err() {
                let _ = fs::remove_dir_all(staging_root);
            }
            extract_result?;
            on_progress(100, "本体 ZIP 校验并解压完成");
            Ok(manifest)
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(staging_root);
        }
        result
    }
}

fn find_bundled_7zip() -> Option<PathBuf> {
    let current_exe = std::env::current_exe().ok();
    let exe_parent = current_exe.as_deref().and_then(Path::parent);
    let mut candidates = Vec::new();
    if let Some(parent) = exe_parent {
        candidates.extend([
            parent.join("7z.exe"),
            parent.join("bin/7z.exe"),
            parent.join("resources/7z.exe"),
            parent.join("resources/bin/7z.exe"),
        ]);
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bin/7z.exe"));
    candidates.into_iter().find(|path| {
        path.is_file()
            && path
                .parent()
                .map(|parent| parent.join("7z.dll").is_file())
                .unwrap_or(false)
    })
}

fn run_7zip(
    archiver: &Path,
    working_directory: &Path,
    args: Vec<String>,
    on_progress: impl Fn(u8),
    is_cancelled: &impl Fn() -> bool,
) -> Result<(), String> {
    if is_cancelled() {
        return Err("任务已取消".to_string());
    }
    let mut command = Command::new(archiver);
    command
        .current_dir(working_directory)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command
        .spawn()
        .map_err(|err| format!("启动内置 7-Zip 失败：{err}"))?;
    #[cfg(target_os = "windows")]
    let _cleanup_job = match assign_child_to_cleanup_job(&child) {
        Ok(job) => job,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("配置 7-Zip 异常退出清理失败：{error}"));
        }
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("读取 7-Zip 进度输出失败".to_string());
        }
    };
    let (progress_sender, progress_receiver) = mpsc::channel::<Vec<u8>>();
    let progress_reader = thread::spawn(move || {
        let mut reader = stdout;
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(size) => {
                    if progress_sender.send(buffer[..size].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = progress_reader.join();
            return Err("读取 7-Zip错误输出失败".to_string());
        }
    };
    let error_reader = thread::spawn(move || {
        let mut reader = stderr;
        let mut bytes = Vec::new();
        let _ = reader.read_to_end(&mut bytes);
        bytes
    });
    let mut output_buffer = Vec::new();
    let mut last_percent = None;
    let mut handle_progress = |chunk: Vec<u8>| {
        output_buffer.extend_from_slice(&chunk);
        if output_buffer.len() > 512 {
            let keep_from = output_buffer.len() - 512;
            output_buffer.drain(..keep_from);
        }
        if let Some(percent) = latest_7zip_percent(&output_buffer) {
            if last_percent != Some(percent) {
                last_percent = Some(percent);
                on_progress(percent);
            }
        }
    };
    loop {
        while let Ok(chunk) = progress_receiver.try_recv() {
            handle_progress(chunk);
        }
        if is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = progress_reader.join();
            let _ = error_reader.join();
            return Err("任务已取消".to_string());
        }
        let status = match child.try_wait() {
            Ok(status) => status,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = progress_reader.join();
                let _ = error_reader.join();
                return Err(format!("等待 7-Zip 完成失败：{error}"));
            }
        };
        match status {
            Some(status) => {
                while let Ok(chunk) = progress_receiver.recv() {
                    handle_progress(chunk);
                }
                let _ = progress_reader.join();
                let stderr = error_reader.join().unwrap_or_default();
                if status.success() {
                    on_progress(100);
                    return Ok(());
                }
                let detail = String::from_utf8_lossy(&stderr).trim().to_string();
                return Err(if detail.is_empty() {
                    format!("7-Zip 压缩失败，退出码：{}", status.code().unwrap_or(-1))
                } else {
                    format!(
                        "7-Zip 压缩失败，退出码：{}：{detail}",
                        status.code().unwrap_or(-1)
                    )
                });
            }
            None => thread::sleep(Duration::from_millis(200)),
        }
    }
}

fn sync_file(path: &Path, operation: &str) -> Result<(), String> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| format!("{operation}失败：{error}"))?
        .sync_all()
        .map_err(|error| format!("{operation}失败：{error}"))
}

#[cfg(target_os = "windows")]
struct ChildCleanupJob {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(target_os = "windows")]
impl Drop for ChildCleanupJob {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[cfg(target_os = "windows")]
fn assign_child_to_cleanup_job(child: &std::process::Child) -> Result<ChildCleanupJob, String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{
        Foundation::HANDLE,
        System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
    };

    let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let configured = unsafe {
        SetInformationJobObject(
            handle,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const std::ffi::c_void,
            std::mem::size_of_val(&limits) as u32,
        ) != 0
    };
    if !configured {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(handle);
        }
        return Err(std::io::Error::last_os_error().to_string());
    }
    let assigned =
        unsafe { AssignProcessToJobObject(handle, child.as_raw_handle() as HANDLE) != 0 };
    if !assigned {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(handle);
        }
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(ChildCleanupJob { handle })
}

fn latest_7zip_percent(bytes: &[u8]) -> Option<u8> {
    let mut result = None;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'%' || index == 0 {
            continue;
        }
        let mut start = index;
        while start > 0 && bytes[start - 1].is_ascii_digit() {
            start -= 1;
        }
        if start == index {
            continue;
        }
        if start > 0 && !bytes[start - 1].is_ascii_whitespace() {
            continue;
        }
        if index + 1 < bytes.len() && !bytes[index + 1].is_ascii_whitespace() {
            continue;
        }
        let percent = match std::str::from_utf8(&bytes[start..index])
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
        {
            Some(percent) => percent,
            None => continue,
        };
        if percent <= 100 {
            result = Some(percent as u8);
        }
    }
    result
}

fn collect_files(root: &Path, protected_paths: &[String]) -> Result<Vec<BodyPackageFile>, String> {
    let mut files = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|err| format!("扫描游戏本体失败：{err}"))?;
        if entry.file_type().is_symlink() {
            return Err(format!(
                "游戏本体包含不支持的符号链接：{}",
                entry.path().display()
            ));
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|err| format!("计算本体相对路径失败：{err}"))?;
        if is_protected_path(
            &relative.to_string_lossy().replace('\\', "/"),
            protected_paths,
        ) {
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = normalize_relative(&relative.to_string_lossy())?;
        let size = entry
            .metadata()
            .map_err(|err| format!("读取游戏文件信息失败（{}）：{err}", entry.path().display()))?
            .len();
        files.push(BodyPackageFile {
            relative_path: relative,
            size,
        });
    }
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(files)
}

fn read_archive_file_index(package_path: &Path) -> Result<Vec<BodyPackageFile>, String> {
    let file = fs::File::open(package_path).map_err(|err| format!("打开本体 ZIP 失败：{err}"))?;
    let mut archive = ZipArchive::new(file).map_err(|err| format!("读取本体 ZIP 失败：{err}"))?;
    let mut files = Vec::new();
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|err| format!("读取本体 ZIP 条目失败：{err}"))?;
        if entry.is_dir() {
            continue;
        }
        let relative_path = normalize_relative(entry.name())?;
        if relative_path == MANIFEST_PATH {
            return Err("本体 ZIP 包含保留清单路径".to_string());
        }
        files.push(BodyPackageFile {
            relative_path,
            size: entry.size(),
        });
    }
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(files)
}

fn same_relative_paths(
    source_files: &[BodyPackageFile],
    archive_files: &[BodyPackageFile],
) -> bool {
    source_files.len() == archive_files.len()
        && source_files
            .iter()
            .zip(archive_files)
            .all(|(source, archive)| source.relative_path == archive.relative_path)
}

fn is_protected_path(relative: &str, protected_paths: &[String]) -> bool {
    protected_paths.iter().any(|protected| {
        let protected = protected.replace('\\', "/").trim_matches('/').to_string();
        protected == "."
            || relative.eq_ignore_ascii_case(&protected)
            || relative
                .to_ascii_lowercase()
                .starts_with(&(protected.to_ascii_lowercase() + "/"))
    })
}

fn normalize_relative(value: &str) -> Result<String, String> {
    let value = value.replace('\\', "/");
    if value.trim().is_empty() || value.contains('\0') {
        return Err("本体包包含无效相对路径".to_string());
    }
    let path = Path::new(&value);
    if path.is_absolute() {
        return Err(format!("本体包路径不是相对路径：{value}"));
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().to_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("本体包路径包含非法跳转：{value}"))
            }
        }
    }
    if parts.is_empty() {
        return Err(format!("本体包路径无效：{value}"));
    }
    Ok(parts.join("/"))
}

fn safe_join(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let relative = normalize_relative(relative)?;
    Ok(root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR)))
}

fn hash_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|err| format!("读取本体包失败：{err}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|err| format!("计算本体包哈希失败：{err}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_ascii_lowercase()
}

fn read_manifest<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
) -> Result<BodyPackageManifest, String> {
    let mut file = archive
        .by_name(MANIFEST_PATH)
        .map_err(|err| format!("本体 ZIP 缺少清单：{err}"))?;
    let mut raw = Vec::new();
    file.read_to_end(&mut raw)
        .map_err(|err| format!("读取本体包清单失败：{err}"))?;
    serde_json::from_slice(&raw).map_err(|err| format!("解析本体包清单失败：{err}"))
}

fn validate_archive_entries<R: Read + Seek>(
    archive: &ZipArchive<R>,
    manifest: &BodyPackageManifest,
) -> Result<(), String> {
    let expected = manifest
        .files
        .iter()
        .map(|file| file.relative_path.clone())
        .collect::<HashSet<_>>();
    if expected.len() != manifest.files.len() {
        return Err("本体包清单包含重复文件".to_string());
    }
    let mut actual = HashSet::new();
    for name in archive.file_names() {
        if name == MANIFEST_PATH || name.ends_with('/') {
            continue;
        }
        let normalized = normalize_relative(name)?;
        if !actual.insert(normalized.clone()) || !expected.contains(&normalized) {
            return Err(format!("本体 ZIP 包含未登记文件：{normalized}"));
        }
    }
    if actual != expected {
        return Err("本体 ZIP 文件与清单不一致".to_string());
    }
    Ok(())
}

use std::io::Seek;

#[cfg(test)]
mod tests {
    use super::{latest_7zip_percent, normalize_relative, path_key, BodyPackageService};
    use std::{collections::HashSet, fs};
    use uuid::Uuid;

    #[test]
    fn package_keeps_game_files_without_name_or_suffix_filters() {
        let root = std::env::temp_dir().join(format!("gamesaver-body-package-{}", Uuid::new_v4()));
        let source = root.join("source");
        let cache = root.join("cache");
        fs::create_dir_all(source.join("localization_work")).expect("create excluded directory");
        fs::create_dir_all(source.join("存档说明")).expect("create unicode directory");
        fs::write(source.join("game.exe"), b"exe").expect("write executable");
        fs::write(source.join("readme.txt"), "日文パス").expect("write unicode file");
        fs::write(source.join("localization_work\u{005c}draft.txt"), b"draft")
            .expect("write excluded file");
        fs::write(source.join("debug.log"), b"log").expect("write excluded log");

        let result = BodyPackageService::create_package_with_exclusions(
            &source,
            &cache,
            "game-1",
            "version-1",
            "game.exe",
            &[],
            |_, _| {},
            || false,
        )
        .expect("create package");
        let staging = root.join("staging");
        let manifest = BodyPackageService::extract_package(
            &result.package_path,
            &staging,
            "game-1",
            "game.exe",
            Some(&result.sha256),
            |_, _| {},
            || false,
        )
        .expect("extract package");
        assert_eq!(manifest.file_count, 4);
        assert!(staging.join("readme.txt").is_file());
        assert!(staging.join("debug.log").is_file());
        assert!(staging.join("localization_work/draft.txt").is_file());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn archive_paths_reject_traversal_and_absolute_paths() {
        assert!(normalize_relative("../escape.txt").is_err());
        assert!(normalize_relative("C:/escape.txt").is_err());
        assert_eq!(
            normalize_relative("存档\\slot.dat").expect("normalize unicode path"),
            "存档/slot.dat"
        );
    }

    #[test]
    fn orphan_packages_are_removed_but_referenced_packages_are_kept() {
        let root =
            std::env::temp_dir().join(format!("gamesaver-body-package-orphans-{}", Uuid::new_v4()));
        let cache = root.join("cache");
        fs::create_dir_all(cache.join("game-1")).expect("create cache");
        let kept = cache.join("game-1/kept.zip");
        let orphan = cache.join("game-1/orphan.zip");
        fs::write(&kept, b"kept").expect("write kept package");
        fs::write(&orphan, b"orphan").expect("write orphan package");

        let referenced = HashSet::from([path_key(&kept)]);
        let removed = BodyPackageService::cleanup_orphan_packages(&cache, &referenced)
            .expect("cleanup orphan packages");

        assert_eq!(removed, 1);
        assert!(kept.is_file());
        assert!(!orphan.exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn corrupted_package_is_rejected_before_extraction() {
        let root =
            std::env::temp_dir().join(format!("gamesaver-body-package-corrupt-{}", Uuid::new_v4()));
        let source = root.join("source");
        let cache = root.join("cache");
        fs::create_dir_all(&source).expect("create source");
        fs::write(source.join("game.exe"), b"exe").expect("write executable");
        let result = BodyPackageService::create_package_with_exclusions(
            &source,
            &cache,
            "game-1",
            "version-1",
            "game.exe",
            &[],
            |_, _| {},
            || false,
        )
        .expect("create package");
        fs::write(&result.package_path, b"corrupted").expect("corrupt package");
        let staging = root.join("staging");
        let error = BodyPackageService::extract_package(
            &result.package_path,
            &staging,
            "game-1",
            "game.exe",
            Some(&result.sha256),
            |_, _| {},
            || false,
        )
        .expect_err("corruption should fail");
        assert!(error.contains("哈希校验失败"));
        assert!(!staging.exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn cancelled_extraction_removes_staging_directory() {
        let root =
            std::env::temp_dir().join(format!("gamesaver-body-package-cancel-{}", Uuid::new_v4()));
        let source = root.join("source");
        let cache = root.join("cache");
        fs::create_dir_all(&source).expect("create source");
        fs::write(source.join("game.exe"), b"exe").expect("write executable");
        let result = BodyPackageService::create_package_with_exclusions(
            &source,
            &cache,
            "game-1",
            "version-1",
            "game.exe",
            &[],
            |_, _| {},
            || false,
        )
        .expect("create package");
        let staging = root.join("staging");
        let error = BodyPackageService::extract_package(
            &result.package_path,
            &staging,
            "game-1",
            "game.exe",
            Some(&result.sha256),
            |_, _| {},
            || true,
        )
        .expect_err("cancellation should fail");
        assert_eq!(error, "任务已取消");
        assert!(!staging.exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn parses_latest_7zip_progress_percentage() {
        assert_eq!(
            latest_7zip_percent(b"\r  0% 1 - file\r 42% 1 - file"),
            Some(42)
        );
        assert_eq!(latest_7zip_percent("\r 100% 完成".as_bytes()), Some(100));
        assert_eq!(latest_7zip_percent(b"no progress"), None);
        assert_eq!(latest_7zip_percent(b"\r 101% invalid"), None);
        assert_eq!(latest_7zip_percent(b"file-99%.txt"), None);
    }

    #[test]
    fn temporary_package_cleanup_removes_manifest_directories() {
        let root =
            std::env::temp_dir().join(format!("gamesaver-body-package-temp-{}", Uuid::new_v4()));
        let cache = root.join("cache");
        let manifest_root = cache.join("game-1/.version.manifest-test/.gamesaver");
        fs::create_dir_all(&manifest_root).expect("create manifest temp directory");
        fs::write(manifest_root.join("body-manifest.json"), b"{}").expect("write manifest");
        fs::write(cache.join("game-1/.version.zip.tmp-test"), b"partial")
            .expect("write package temp file");
        fs::write(cache.join("game-1/real.zip"), b"package").expect("write real package");

        let removed = BodyPackageService::cleanup_temporary_packages(&cache)
            .expect("cleanup temporary package artifacts");

        assert_eq!(removed, 2);
        assert!(!cache.join("game-1/.version.manifest-test").exists());
        assert!(!cache.join("game-1/.version.zip.tmp-test").exists());
        assert!(cache.join("game-1/real.zip").is_file());
        fs::remove_dir_all(root).expect("cleanup");
    }
}
