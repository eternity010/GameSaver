use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
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
    pub sha256: String,
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
        let mut removed = 0;
        for entry in WalkDir::new(cache_root).follow_links(false) {
            let entry = entry.map_err(|err| format!("扫描本体包临时文件失败：{err}"))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            if name.starts_with('.') && (name.contains(".tmp") || name.starts_with(".download-")) {
                fs::remove_file(entry.path()).map_err(|err| format!("清理本体包临时文件失败（{}）：{err}", entry.path().display()))?;
                removed += 1;
            }
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
            fs::remove_file(entry.path())
                .map_err(|err| format!("清理孤立本体包失败（{}）：{err}", entry.path().display()))?;
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
        if !files.iter().any(|path| path == &executable) {
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
            for (index, relative) in files.iter().enumerate() {
                if is_cancelled() {
                    return Err("任务已取消".to_string());
                }
                let source = source_root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
                let mut input = fs::File::open(&source)
                    .map_err(|err| format!("读取游戏文件失败（{relative}）：{err}"))?;
                let mut hasher = Sha256::new();
                let mut size = 0u64;
                writer
                    .start_file(relative, options)
                    .map_err(|err| format!("写入本体 ZIP 条目失败（{relative}）：{err}"))?;
                let mut buffer = vec![0u8; 1024 * 1024];
                loop {
                    let read = input
                        .read(&mut buffer)
                        .map_err(|err| format!("读取游戏文件失败（{relative}）：{err}"))?;
                    if read == 0 {
                        break;
                    }
                    hasher.update(&buffer[..read]);
                    writer
                        .write_all(&buffer[..read])
                        .map_err(|err| format!("写入本体 ZIP 数据失败（{relative}）：{err}"))?;
                    size = size.saturating_add(read as u64);
                }
                total_bytes = total_bytes.saturating_add(size);
                manifest_files.push(BodyPackageFile {
                    relative_path: relative.clone(),
                    size,
                    sha256: hex::encode(hasher.finalize()),
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
        let file = fs::File::open(package_path)
            .map_err(|err| format!("打开本体 ZIP 失败：{err}"))?;
        let mut archive = ZipArchive::new(file)
            .map_err(|err| format!("读取本体 ZIP 失败：{err}"))?;
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
        if !manifest.files.iter().any(|item| item.relative_path == executable) {
            return Err("本体包缺少游戏启动程序".to_string());
        }
        validate_archive_entries(&archive, &manifest)?;
        validate_archive_contents(&mut archive, &manifest)?;
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
        Self::validate_package(
            package_path,
            game_uid,
            executable_relative_path,
            None,
        )
        .map(|_| ())
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
                    let mut hasher = Sha256::new();
                    let mut size = 0u64;
                    let mut buffer = vec![0u8; 1024 * 1024];
                    loop {
                        let read = input.read(&mut buffer).map_err(|err| {
                            format!("解压本体文件失败（{}）：{err}", expected.relative_path)
                        })?;
                        if read == 0 {
                            break;
                        }
                        hasher.update(&buffer[..read]);
                        output.write_all(&buffer[..read]).map_err(|err| {
                            format!("写入本体恢复文件失败（{}）：{err}", expected.relative_path)
                        })?;
                        size = size.saturating_add(read as u64);
                    }
                    output.sync_all().map_err(|err| {
                        format!("刷新本体恢复文件失败（{}）：{err}", expected.relative_path)
                    })?;
                    let actual_hash = hex::encode(hasher.finalize());
                    if size != expected.size || actual_hash != expected.sha256 {
                        let _ = fs::remove_file(&temporary);
                        return Err(format!("本体文件校验失败：{}", expected.relative_path));
                    }
                    fs::rename(&temporary, &target).map_err(|err| {
                        format!("提交本体恢复文件失败（{}）：{err}", expected.relative_path)
                    })?;
                    written_bytes = written_bytes.saturating_add(size);
                    let progress = 8 + (((index + 1) * 90) / manifest.files.len().max(1)) as u8;
                    on_progress(
                        progress.min(98),
                        &format!(
                            "正在校验并解压本体文件 {}/{}",
                            index + 1,
                            manifest.files.len()
                        ),
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

fn collect_files(root: &Path, protected_paths: &[String]) -> Result<Vec<String>, String> {
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
        files.push(relative);
    }
    files.sort();
    Ok(files)
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

fn validate_archive_contents<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    manifest: &BodyPackageManifest,
) -> Result<(), String> {
    let mut buffer = vec![0u8; 1024 * 1024];
    for expected in &manifest.files {
        let mut input = archive
            .by_name(&expected.relative_path)
            .map_err(|err| format!("读取本体 ZIP 条目失败（{}）：{err}", expected.relative_path))?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        loop {
            let read = input
                .read(&mut buffer)
                .map_err(|err| format!("校验本体 ZIP 条目失败（{}）：{err}", expected.relative_path))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            size = size.saturating_add(read as u64);
        }
        if size != expected.size || hex::encode(hasher.finalize()) != expected.sha256 {
            return Err(format!("本体 ZIP 文件校验失败：{}", expected.relative_path));
        }
    }
    Ok(())
}

use std::io::Seek;

#[cfg(test)]
mod tests {
    use super::{normalize_relative, path_key, BodyPackageService};
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
        let root = std::env::temp_dir()
            .join(format!("gamesaver-body-package-orphans-{}", Uuid::new_v4()));
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
}
