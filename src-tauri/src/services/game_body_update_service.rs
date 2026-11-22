use crate::domain::{path_utils::normalize_path, Game, GameBodyVersion};
use crate::services::disk_space;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;
use walkdir::WalkDir;

mod workflow;
pub(crate) use workflow::{
    finish_update_task, pending_updates_at_startup, record_pending_updates, retry_update, run_update,
};

pub struct GameBodyUpdateService;

pub struct UpdatePlan {
    pub source: PathBuf,
    pub executable_relative_path: String,
    pub file_count: usize,
    pub total_bytes: u64,
}

pub struct BodySwap {
    pub archive_path: PathBuf,
    pub failed_path: PathBuf,
    pub had_original: bool,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum UpdatePhase {
    #[default]
    Prepared,
    Committed,
    Complete,
}

#[derive(Debug, Serialize, Deserialize)]
struct UpdateJournal {
    managed_path: String,
    staging_path: String,
    archive_path: String,
    #[serde(default)]
    phase: UpdatePhase,
    #[serde(default = "default_had_original")]
    had_original: bool,
    #[serde(default)]
    root_location: Option<String>,
}

fn default_had_original() -> bool {
    true
}

impl GameBodyUpdateService {
    pub fn journal_path(games_root: &Path, game_uid: &str) -> PathBuf {
        games_root.join(format!(".{game_uid}.update.json"))
    }

    pub fn write_journal(
        games_root: &Path,
        game_uid: &str,
        managed_path: &Path,
        staging_path: &Path,
        archive_path: &Path,
    ) -> Result<PathBuf, String> {
        validate_uid(game_uid)?;
        let path = Self::journal_path(games_root, game_uid);
        if fs::symlink_metadata(&path).is_ok() {
            return Err("该游戏仍有更新事务待恢复，请先重试恢复".into());
        }
        let journal = UpdateJournal {
            managed_path: managed_path.to_string_lossy().to_string(),
            staging_path: staging_path.to_string_lossy().to_string(),
            archive_path: archive_path.to_string_lossy().to_string(),
            phase: UpdatePhase::Prepared,
            had_original: managed_path.is_dir(),
            root_location: Some(physical_root(games_root)?),
        };
        validate_journal(games_root, game_uid, &journal)?;
        let bytes =
            serde_json::to_vec(&journal).map_err(|err| format!("序列化游戏更新日志失败：{err}"))?;
        atomic_write(&path, &bytes)?;
        Ok(path)
    }

    pub fn clear_journal(path: &Path) -> Result<(), String> {
        if !path
            .try_exists()
            .map_err(|error| format!("检查更新事务失败：{error}"))?
        {
            return Ok(());
        }
        let root = path.parent().ok_or("更新事务路径无效")?;
        let uid = journal_uid(path).ok_or("更新事务名称无效")?;
        let mut journal = read_journal(root, &uid)?;
        journal.phase = UpdatePhase::Complete;
        write_journal_record(path, &journal)?;
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("清理游戏更新日志失败：{error}")),
        }
    }

    #[cfg(test)]
    pub fn recover_pending_updates(
        games_root: &Path,
        committed_archives: &HashSet<String>,
    ) -> Result<(), String> {
        Self::recover_pending_updates_with(games_root, committed_archives, |_, _| Ok(()))
    }

    pub fn pending_game_uids(games_root: &Path) -> Result<Vec<String>, String> {
        validate_directory(games_root)?;
        let entries = match fs::read_dir(games_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let library_root = games_root.parent().ok_or("游戏库路径无效")?;
                require_available_update_root(library_root)?;
                return match fs::symlink_metadata(games_root) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
                    _ => Err("游戏本体目录在检查期间发生变化，请重试".into()),
                };
            }
            Err(error) => return Err(format!("读取更新事务失败：{error}")),
        };
        let mut pending = Vec::new();
        for entry in entries {
            let path = entry
                .map_err(|error| format!("读取更新事务失败：{error}"))?
                .path();
            if let Some(uid) = journal_uid(&path) {
                validate_uid(&uid)?;
                pending.push(uid);
            }
        }
        pending.sort();
        Ok(pending)
    }

    pub(crate) fn has_pending_update(games_root: &Path, game_uid: &str) -> Result<bool, String> {
        validate_uid(game_uid)?;
        require_available_update_root(games_root)?;
        match fs::symlink_metadata(Self::journal_path(games_root, game_uid)) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                require_available_update_root(games_root)?;
                Ok(false)
            }
            Err(error) => Err(format!("读取游戏更新事务失败：{error}")),
        }
    }

    pub(crate) fn ensure_no_pending_update(games_root: &Path, game_uid: &str) -> Result<(), String> {
        if Self::has_pending_update(games_root, game_uid)? {
            return Err("该游戏仍有更新事务待恢复，请在传输中心重试".into());
        }
        Ok(())
    }

    pub fn recover_pending_updates_with(
        games_root: &Path,
        committed_archives: &HashSet<String>,
        mut settle: impl FnMut(&str, bool) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut errors = Vec::new();
        for uid in Self::pending_game_uids(games_root)? {
            if let Err(error) =
                Self::recover_game_update(games_root, &uid, committed_archives, |committed| {
                    settle(&uid, committed)
                })
            {
                errors.push(format!("{uid}：{error}"));
            }
        }
        if let Err(error) = cleanup_stale_update_artifacts(games_root) {
            errors.push(error);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("；"))
        }
    }

    pub fn recover_game_update(
        games_root: &Path,
        game_uid: &str,
        committed_archives: &HashSet<String>,
        settle: impl FnOnce(bool) -> Result<(), String>,
    ) -> Result<bool, String> {
        validate_uid(game_uid)?;
        recover_journal(
            games_root,
            &Self::journal_path(games_root, game_uid),
            committed_archives,
            settle,
        )
    }

    pub fn validate_managed_game(games_root: &Path, game: &Game) -> Result<(), String> {
        validate_uid(&game.game_uid)?;
        let expected = games_root.join(&game.game_uid);
        require_same_path(Path::new(&game.managed_path), &expected)?;
        validate_directory(&expected)
    }

    pub fn cleanup_removed_restore_archives(games_root: &Path) -> Result<Vec<PathBuf>, String> {
        let pending = Self::pending_game_uids(games_root)?;
        let versions_root = games_root.join(".versions");
        validate_directory(&versions_root)?;
        if !versions_root.is_dir() {
            return Ok(Vec::new());
        }
        let mut removed = Vec::new();
        for game_entry in
            fs::read_dir(&versions_root).map_err(|err| format!("读取历史本体目录失败：{err}"))?
        {
            let game_path = game_entry
                .map_err(|err| format!("读取历史本体目录失败：{err}"))?
                .path();
            let uid = game_path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or("历史本体游戏标识无效")?;
            validate_uid(uid)?;
            if pending.iter().any(|pending_uid| pending_uid == uid) {
                continue;
            }
            validate_directory(&game_path)?;
            if !game_path.is_dir() {
                continue;
            }
            for archive_entry in
                fs::read_dir(&game_path).map_err(|err| format!("读取历史本体版本失败：{err}"))?
            {
                let archive_path = archive_entry
                    .map_err(|err| format!("读取历史本体版本失败：{err}"))?
                    .path();
                let Some(name) = archive_path.file_name().and_then(|value| value.to_str()) else {
                    continue;
                };
                if name.starts_with("restore-") && archive_path.is_dir() {
                    validate_uid(name)?;
                    validate_directory(&archive_path)?;
                    fs::remove_dir_all(&archive_path)
                        .map_err(|err| format!("清理历史本体恢复副本失败：{err}"))?;
                    removed.push(archive_path);
                }
            }
            if fs::read_dir(&game_path)
                .map_err(|err| format!("检查历史本体目录失败：{err}"))?
                .next()
                .is_none()
            {
                fs::remove_dir(&game_path)
                    .map_err(|err| format!("清理空历史本体目录失败：{err}"))?;
            }
        }
        Ok(removed)
    }

    pub fn cleanup_archived_body_versions(
        games_root: &Path,
        versions: &mut Vec<GameBodyVersion>,
    ) -> Result<usize, String> {
        let pending = Self::pending_game_uids(games_root)?;
        for version in versions
            .iter()
            .filter(|version| !version.archive_path.trim().is_empty())
        {
            validate_archive(
                games_root,
                &version.game_uid,
                &version.version_id,
                Path::new(&version.archive_path),
            )?;
        }
        let mut removed = 0;
        for version in versions.iter_mut() {
            if version.archive_path.trim().is_empty() {
                continue;
            }
            if pending.contains(&version.game_uid) {
                continue;
            }
            let archive = PathBuf::from(&version.archive_path);
            if archive.is_dir() {
                fs::remove_dir_all(&archive)
                    .map_err(|err| format!("清理历史游戏本体失败：{err}"))?;
                removed += 1;
            } else if archive.is_file() {
                fs::remove_file(&archive)
                    .map_err(|err| format!("清理历史游戏本体文件失败：{err}"))?;
                removed += 1;
            }
            version.archive_path.clear();
        }
        versions.retain(|version| {
            version.package_path.is_some() || !version.archive_path.trim().is_empty()
        });
        Ok(removed)
    }

    pub fn clear_missing_archives(
        games_root: &Path,
        game_uid: &str,
        versions: &mut Vec<GameBodyVersion>,
    ) -> Result<(), String> {
        validate_uid(game_uid)?;
        for version in versions
            .iter_mut()
            .filter(|version| version.game_uid == game_uid && !version.archive_path.is_empty())
        {
            let archive = Path::new(&version.archive_path);
            validate_archive(games_root, game_uid, &version.version_id, archive)?;
            if !archive
                .try_exists()
                .map_err(|error| format!("检查历史本体失败：{error}"))?
            {
                version.archive_path.clear();
            }
        }
        versions
            .retain(|version| version.package_path.is_some() || !version.archive_path.is_empty());
        Ok(())
    }

    #[cfg(test)]
    pub fn validate_source(source: &Path, game: &Game) -> Result<UpdatePlan, String> {
        Self::validate_source_with(source, game, |_, _| {}, || false)
    }

    pub fn validate_source_with(
        source: &Path,
        game: &Game,
        on_progress: impl Fn(usize, u64),
        is_cancelled: impl Fn() -> bool,
    ) -> Result<UpdatePlan, String> {
        if is_cancelled() {
            return Err("任务已取消".into());
        }
        let source = source
            .canonicalize()
            .map_err(|err| format!("解析新版游戏目录失败：{err}"))?;
        let managed = Path::new(&game.managed_path);
        let games_root = managed.parent().ok_or("受管游戏目录无效")?;
        validate_directory(games_root)?;
        let physical_games = games_root
            .canonicalize()
            .map_err(|error| format!("解析游戏库位置失败：{error}"))?;
        if paths_overlap(&source, &physical_games) {
            return Err("新版游戏目录不能是当前受管游戏目录，或位于其内部".to_string());
        }
        if !source.is_dir() {
            return Err("新版游戏目录不存在或不可访问".to_string());
        }
        let relative = validate_relative(&game.launch.executable_relative_path)?;
        if !source.join(&relative).is_file() {
            return Err(format!("新版游戏目录中找不到启动程序：{}", relative));
        }
        let mut file_count = 0usize;
        let mut total_bytes = 0u64;
        for entry in WalkDir::new(&source).follow_links(false) {
            if is_cancelled() {
                return Err("任务已取消".into());
            }
            let entry = entry.map_err(|err| format!("扫描新版游戏目录失败：{err}"))?;
            if metadata_is_link(
                &fs::symlink_metadata(entry.path())
                    .map_err(|error| format!("读取新版游戏路径失败：{error}"))?,
            ) {
                return Err(format!(
                    "新版游戏目录包含不支持的符号链接：{}",
                    entry.path().display()
                ));
            }
            if entry.file_type().is_file() {
                let metadata = entry
                    .metadata()
                    .map_err(|err| format!("读取新版游戏文件信息失败：{err}"))?;
                file_count += 1;
                total_bytes = total_bytes.saturating_add(metadata.len());
                if file_count == 1 || file_count % 128 == 0 {
                    on_progress(file_count, total_bytes);
                }
            }
        }
        if file_count == 0 {
            return Err("新版游戏目录中没有可复制的文件".to_string());
        }
        if is_cancelled() {
            return Err("任务已取消".into());
        }
        disk_space::ensure_available_space(managed, total_bytes)?;
        on_progress(file_count, total_bytes);
        if is_cancelled() {
            return Err("任务已取消".into());
        }
        Ok(UpdatePlan {
            source,
            executable_relative_path: relative,
            file_count,
            total_bytes,
        })
    }

    pub fn copy_to_staging(
        plan: &UpdatePlan,
        games_root: &Path,
        game_uid: &str,
        on_progress: impl Fn(u8, &str),
        is_cancelled: impl Fn() -> bool,
    ) -> Result<PathBuf, String> {
        validate_uid(game_uid)?;
        validate_directory(games_root)?;
        if Self::journal_path(games_root, game_uid)
            .try_exists()
            .map_err(|error| format!("检查更新事务失败：{error}"))?
        {
            return Err("该游戏仍有更新事务待恢复，请先重试恢复".into());
        }
        let physical_games = games_root
            .canonicalize()
            .map_err(|error| format!("解析游戏库位置失败：{error}"))?;
        if paths_overlap(
            &plan
                .source
                .canonicalize()
                .map_err(|error| format!("解析新版游戏目录失败：{error}"))?,
            &physical_games,
        ) {
            return Err("新版游戏目录不能位于受管游戏库内".into());
        }
        let staging = games_root.join(format!(".{game_uid}.updating"));
        validate_directory(&staging)?;
        if staging.exists() {
            fs::remove_dir_all(&staging).map_err(|err| format!("清理上次未完成更新失败：{err}"))?;
        }
        fs::create_dir_all(&staging).map_err(|err| format!("创建游戏更新暂存目录失败：{err}"))?;
        let result = (|| -> Result<PathBuf, String> {
            let mut file_index = 0usize;
            let mut copied_bytes = 0u64;
            for entry in WalkDir::new(&plan.source).follow_links(false) {
                if is_cancelled() {
                    return Err("任务已取消".to_string());
                }
                let entry = entry.map_err(|err| format!("扫描新版游戏目录失败：{err}"))?;
                if metadata_is_link(
                    &fs::symlink_metadata(entry.path())
                        .map_err(|error| format!("检查新版游戏路径失败：{error}"))?,
                ) {
                    return Err("复制过程中发现链接或重解析目录，已停止更新".into());
                }
                if !entry.file_type().is_file() {
                    continue;
                }
                file_index += 1;
                let relative = entry
                    .path()
                    .strip_prefix(&plan.source)
                    .map_err(|err| format!("计算新版游戏相对路径失败：{err}"))?;
                let target = staging.join(relative);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|err| format!("创建游戏更新目录失败：{err}"))?;
                }
                let copied = fs::copy(entry.path(), &target).map_err(|err| {
                    format!("复制新版游戏文件失败（{}）：{err}", relative.display())
                })?;
                copied_bytes = copied_bytes.saturating_add(copied);
                on_progress(
                    10 + ((file_index.saturating_mul(78) / plan.file_count.max(1)).min(78)) as u8,
                    &format!("正在复制新版游戏文件 {}/{}", file_index, plan.file_count),
                );
            }
            if is_cancelled() {
                return Err("任务已取消".to_string());
            }
            if file_index != plan.file_count || copied_bytes != plan.total_bytes {
                return Err(
                    "复制过程中新版来源的文件数量或大小发生变化，请停止修改来源目录后重试".into(),
                );
            }
            if !staging.join(&plan.executable_relative_path).is_file() {
                return Err("新版游戏启动程序复制后不存在".to_string());
            }
            Ok(staging.clone())
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&staging);
        }
        result
    }

    pub fn swap(
        managed_path: &Path,
        staging: &Path,
        archive_path: &Path,
    ) -> Result<BodySwap, String> {
        let (games_root, uid, version_id) =
            validate_swap_paths(managed_path, staging, archive_path)?;
        let failed_path = failed_path(&games_root, &uid, &version_id);
        validate_directory(&failed_path)?;
        if archive_path
            .try_exists()
            .map_err(|error| format!("检查旧本体路径失败：{error}"))?
            || failed_path.exists()
        {
            return Err("更新事务的暂存位置已存在，请先恢复未完成事务".into());
        }
        if !staging.is_dir() {
            return Err("新版游戏暂存目录不存在".into());
        }
        let had_original = managed_path.is_dir();
        if had_original {
            if let Some(parent) = archive_path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|err| format!("创建旧游戏版本目录失败：{err}"))?;
            }
            fs::rename(managed_path, archive_path)
                .map_err(|err| format!("暂存当前游戏本体失败：{err}"))?;
        }
        if let Some(parent) = managed_path.parent() {
            fs::create_dir_all(parent).map_err(|err| format!("创建游戏安装目录失败：{err}"))?;
        }
        if let Err(error) = fs::rename(staging, managed_path) {
            if archive_path.exists() {
                let rollback = fs::rename(archive_path, managed_path);
                return if let Err(rollback_error) = rollback {
                    Err(format!(
                        "提交新版游戏失败：{error}；恢复旧游戏本体失败：{rollback_error}"
                    ))
                } else {
                    Err(format!("提交新版游戏失败：{error}"))
                };
            }
            return Err(format!("提交新版游戏失败：{error}"));
        }
        Ok(BodySwap {
            archive_path: archive_path.to_path_buf(),
            failed_path,
            had_original,
        })
    }

    pub fn rollback(managed_path: &Path, swap: &BodySwap) -> Result<(), String> {
        let games_root = managed_path.parent().ok_or("受管游戏目录无效")?;
        let uid = managed_path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or("游戏标识无效")?;
        let staging = games_root.join(format!(".{uid}.updating"));
        let (_, _, version) = validate_swap_paths(managed_path, &staging, &swap.archive_path)?;
        require_same_path(&swap.failed_path, &failed_path(games_root, uid, &version))?;
        validate_directory(&swap.failed_path)?;
        if managed_path.exists() && swap.failed_path.exists() {
            return Err("回滚暂存路径已存在，已保留本体与事务，请检查后重试".into());
        }
        if !swap.archive_path.is_dir() {
            if swap.had_original {
                return Err("旧本体暂存目录缺失，已保留当前本体和事务以供恢复".into());
            }
            remove_directory(managed_path)?;
            return Ok(());
        }
        if managed_path.exists() {
            fs::rename(managed_path, &swap.failed_path)
                .map_err(|err| format!("暂存失败的新游戏本体失败：{err}"))?;
        }
        if let Err(error) = fs::rename(&swap.archive_path, managed_path) {
            if swap.failed_path.exists() {
                let _ = fs::rename(&swap.failed_path, managed_path);
            }
            return Err(format!("恢复旧游戏本体失败：{error}"));
        }
        if swap.failed_path.exists() {
            fs::remove_dir_all(&swap.failed_path)
                .map_err(|err| format!("清理失败的新游戏本体失败：{err}"))?;
        }
        Ok(())
    }
}

fn recover_journal(
    games_root: &Path,
    journal_path: &Path,
    committed_archives: &HashSet<String>,
    settle: impl FnOnce(bool) -> Result<(), String>,
) -> Result<bool, String> {
    let uid = journal_uid(journal_path).ok_or("游戏更新事务名称无效")?;
    let mut journal = read_journal(games_root, &uid)?;
    let managed = PathBuf::from(&journal.managed_path);
    let staging = PathBuf::from(&journal.staging_path);
    let archive = PathBuf::from(&journal.archive_path);
    let version = archive
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or("更新版本标识无效")?;
    let failed = failed_path(games_root, &uid, version);
    validate_directory(&failed)?;
    if journal.phase == UpdatePhase::Complete {
        settle(true)?;
        fs::remove_file(journal_path).map_err(|error| format!("清理更新完成记录失败：{error}"))?;
        return Ok(true);
    }
    let committed = journal.phase == UpdatePhase::Committed
        || committed_archives
            .iter()
            .any(|path| normalize_path(Path::new(path)) == normalize_path(&archive));
    if committed {
        if !managed.is_dir() && staging.is_dir() {
            fs::rename(&staging, &managed)
                .map_err(|err| format!("恢复已提交的游戏本体失败：{err}"))?;
        } else if !managed.is_dir() {
            return Err("已提交的游戏本体更新缺少当前目录和暂存目录".to_string());
        }
        journal.phase = UpdatePhase::Committed;
        write_journal_record(journal_path, &journal)?;
        remove_directory(&staging)?;
        remove_directory(&archive)?;
    } else if archive.is_dir() {
        GameBodyUpdateService::rollback(
            &managed,
            &BodySwap {
                archive_path: archive.clone(),
                failed_path: failed.clone(),
                had_original: true,
            },
        )?;
        remove_directory(&staging)?;
    } else if managed.is_dir() {
        if !journal.had_original {
            remove_directory(&managed)?;
        }
        remove_directory(&failed)?;
        remove_directory(&staging)?;
    } else if staging.is_dir() {
        if journal.had_original {
            return Err("旧本体缺失，已保留新版暂存目录和事务，请检查后重试".into());
        }
        remove_directory(&staging)?;
    } else if !journal.had_original {
    } else {
        return Err("游戏更新中断，找不到可恢复的游戏本体或暂存目录".to_string());
    }
    settle(committed)?;
    GameBodyUpdateService::clear_journal(journal_path)?;
    Ok(committed)
}

fn cleanup_stale_update_artifacts(games_root: &Path) -> Result<(), String> {
    let pending = GameBodyUpdateService::pending_game_uids(games_root)?;
    let entries = match fs::read_dir(games_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && pending.is_empty() => {
            return require_available_update_root(games_root.parent().ok_or("游戏库路径无效")?);
        }
        Err(error) => return Err(format!("读取游戏更新暂存目录失败：{error}")),
    };
    for entry in entries {
        let path = entry
            .map_err(|err| format!("读取游戏更新暂存目录失败：{err}"))?
            .path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if !name.starts_with('.') || !path.is_dir() {
            continue;
        }
        if pending
            .iter()
            .any(|uid| name.starts_with(&format!(".{uid}.")))
        {
            continue;
        }
        validate_directory(&path)?;
        if let Some(uid) = name
            .strip_prefix('.')
            .and_then(|value| value.strip_suffix(".updating"))
        {
            validate_uid(uid)?;
            if games_root.join(uid).is_dir() {
                fs::remove_dir_all(&path)
                    .map_err(|err| format!("清理残留游戏更新暂存目录失败：{err}"))?;
            } else {
                fs::remove_dir_all(&path)
                    .map_err(|err| format!("清理孤立游戏更新暂存目录失败：{err}"))?;
            }
        } else if let Some((base, _)) = name
            .strip_prefix('.')
            .and_then(|value| value.split_once(".failed-"))
        {
            validate_uid(base)?;
            if games_root.join(base).is_dir() {
                fs::remove_dir_all(&path)
                    .map_err(|err| format!("清理残留游戏更新目录失败：{err}"))?;
            }
        }
    }
    Ok(())
}

fn validate_uid(uid: &str) -> Result<(), String> {
    if !crate::domain::is_safe_path_segment(uid)
        || uid.starts_with('.')
        || uid.ends_with(['.', ' '])
        || uid.chars().any(|value| "<>:\"|?*".contains(value))
    {
        return Err("更新游戏或版本标识不是安全的单层目录名称".into());
    }
    Ok(())
}

fn metadata_is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn validate_path(path: &Path, directory: bool) -> Result<(), String> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err(format!(
            "更新路径必须是无目录跳转的绝对路径：{}",
            path.display()
        ));
    }
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                if metadata_is_link(&metadata)
                    || (ancestor != path || directory) && !metadata.is_dir()
                    || ancestor == path && !directory && !metadata.is_file()
                {
                    return Err(format!(
                        "更新路径包含链接、重解析目录或错误类型：{}",
                        ancestor.display()
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "检查更新路径失败（{}）：{error}",
                    ancestor.display()
                ))
            }
        }
    }
    Ok(())
}

fn validate_directory(path: &Path) -> Result<(), String> {
    validate_path(path, true)
}

fn require_available_update_root(path: &Path) -> Result<(), String> {
    validate_directory(path)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata_is_link(&metadata) => Ok(()),
        Ok(_) => Err(format!("游戏库位置不是普通目录：{}", path.display())),
        Err(error) => Err(format!("游戏库暂不可用（{}）：{error}", path.display())),
    }
}

fn require_same_path(actual: &Path, expected: &Path) -> Result<(), String> {
    if actual.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::CurDir
        )
    }) || normalize_path(actual) != normalize_path(expected)
    {
        return Err(format!("更新目标不属于当前受管游戏：{}", actual.display()));
    }
    Ok(())
}

fn physical_root(root: &Path) -> Result<String, String> {
    validate_directory(root)?;
    root.canonicalize()
        .map(|root| normalize_path(&root))
        .map_err(|error| format!("解析更新游戏库真实位置失败：{error}"))
}

fn validate_archive(root: &Path, uid: &str, version: &str, archive: &Path) -> Result<(), String> {
    validate_uid(uid)?;
    validate_uid(version)?;
    require_same_path(archive, &root.join(".versions").join(uid).join(version))?;
    validate_directory(archive)
}

fn validate_swap_paths(
    managed: &Path,
    staging: &Path,
    archive: &Path,
) -> Result<(PathBuf, String, String), String> {
    let root = managed.parent().ok_or("更新游戏库路径无效")?;
    let uid = managed
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or("更新游戏标识无效")?;
    let version = archive
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or("更新版本标识无效")?;
    validate_uid(uid)?;
    require_same_path(staging, &root.join(format!(".{uid}.updating")))?;
    validate_directory(managed)?;
    validate_directory(staging)?;
    validate_archive(root, uid, version, archive)?;
    Ok((root.to_path_buf(), uid.to_string(), version.to_string()))
}

fn failed_path(root: &Path, uid: &str, version: &str) -> PathBuf {
    root.join(format!(".{uid}.failed-{version}"))
}

fn journal_uid(path: &Path) -> Option<String> {
    path.file_name()?
        .to_str()?
        .strip_prefix('.')?
        .strip_suffix(".update.json")
        .map(str::to_string)
}

fn validate_journal(root: &Path, uid: &str, journal: &UpdateJournal) -> Result<(), String> {
    validate_uid(uid)?;
    require_same_path(Path::new(&journal.managed_path), &root.join(uid))?;
    validate_swap_paths(
        Path::new(&journal.managed_path),
        Path::new(&journal.staging_path),
        Path::new(&journal.archive_path),
    )?;
    if let Some(location) = &journal.root_location {
        if *location != physical_root(root)? {
            return Err("游戏库真实位置已改变，更新事务现场已保留".into());
        }
    }
    Ok(())
}

fn read_journal(root: &Path, uid: &str) -> Result<UpdateJournal, String> {
    let path = GameBodyUpdateService::journal_path(root, uid);
    validate_path(&path, false)?;
    let raw = fs::read(&path).map_err(|error| format!("读取更新事务失败：{error}"))?;
    let journal =
        serde_json::from_slice(&raw).map_err(|error| format!("解析更新事务失败：{error}"))?;
    validate_journal(root, uid, &journal)?;
    Ok(journal)
}

fn write_journal_record(path: &Path, journal: &UpdateJournal) -> Result<(), String> {
    let bytes =
        serde_json::to_vec(journal).map_err(|error| format!("序列化更新事务失败：{error}"))?;
    atomic_write(path, &bytes)
}

fn remove_directory(path: &Path) -> Result<(), String> {
    validate_directory(path)?;
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("清理更新目录失败（{}）：{error}", path.display())),
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temporary = path.with_file_name(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        Uuid::new_v4().simple()
    ));
    let result = (|| -> Result<(), String> {
        let mut file = fs::File::create(&temporary)
            .map_err(|err| format!("创建游戏更新日志临时文件失败：{err}"))?;
        std::io::Write::write_all(&mut file, bytes)
            .map_err(|err| format!("写入游戏更新日志失败：{err}"))?;
        file.sync_all()
            .map_err(|err| format!("刷新游戏更新日志失败：{err}"))?;
        drop(file);
        fs::rename(&temporary, path).map_err(|err| format!("提交游戏更新日志失败：{err}"))?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result
}

fn validate_relative(value: &str) -> Result<String, String> {
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err("启动程序相对路径无效".to_string());
    }
    Ok(value.replace('\\', "/").trim_matches('/').to_string())
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    let left = normalize_path(left);
    let right = normalize_path(right);
    left == right || left.starts_with(&(right.clone() + "\\")) || right.starts_with(&(left + "\\"))
}

#[cfg(test)]
mod tests {
    use super::GameBodyUpdateService;
    use crate::domain::{Game, GameBodyVersion};
    use crate::test_support::TempWorkspace;
    use std::fs;

    /// 建一棵带 `Drop` 清理的测试目录树，理由见 `crate::test_support::TempWorkspace`。
    ///
    /// 这些用例都要真实目录（原子交换、日志、回滚都要落盘）。此前清理靠各测试末尾
    /// `remove_dir_all(root).expect("cleanup")`，`assert!` 一失败就执行不到；
    /// 而且 `.expect()` 让「清理失败」也能把一次通过的测试判失败，两头都不对。
    ///
    /// `label` 保留各用例原本的语义（`swap` / `recovery` / …），失败现场据此找回是哪个用例。
    fn temp_workspace(label: &str) -> TempWorkspace {
        TempWorkspace::new(label)
    }

    #[test]
    fn update_source_must_contain_existing_executable() {
        let root = temp_workspace("gamesaver-update");
        let source = root.join("source");
        fs::create_dir_all(&source).expect("create source");
        let game = Game::new_pending("Game", root.join("managed").to_string_lossy(), "game.exe");
        assert!(GameBodyUpdateService::validate_source(&source, &game).is_err());
    }

    #[test]
    fn swap_and_rollback_restore_the_original_directory() {
        let root = temp_workspace("gamesaver-swap");
        let managed = root.join("managed");
        let staging = root.join(".managed.updating");
        let archive = root.join(".versions").join("managed").join("old");
        fs::create_dir_all(&managed).expect("create managed");
        fs::create_dir_all(&staging).expect("create staging");
        fs::write(managed.join("state.txt"), b"old").expect("write old");
        fs::write(staging.join("state.txt"), b"new").expect("write new");
        let swap = GameBodyUpdateService::swap(&managed, &staging, &archive).expect("swap");
        assert_eq!(
            fs::read(managed.join("state.txt")).expect("read new"),
            b"new"
        );
        GameBodyUpdateService::rollback(&managed, &swap).expect("rollback");
        assert_eq!(
            fs::read(managed.join("state.txt")).expect("read old"),
            b"old"
        );
    }

    #[test]
    fn recovery_restores_original_directory_after_interrupted_swap() {
        let root = temp_workspace("gamesaver-recovery");
        let games_root = root.join("games");
        let managed = games_root.join("game-1");
        let staging = games_root.join(".game-1.updating");
        let archive = games_root
            .join(".versions")
            .join("game-1")
            .join("version-1");
        fs::create_dir_all(&archive).expect("create archive");
        fs::create_dir_all(&staging).expect("create staging");
        fs::write(archive.join("state.txt"), b"old").expect("write old");
        fs::write(staging.join("state.txt"), b"new").expect("write new");
        let journal = GameBodyUpdateService::write_journal(
            &games_root,
            "game-1",
            &managed,
            &staging,
            &archive,
        )
        .expect("write journal");

        GameBodyUpdateService::recover_pending_updates(
            &games_root,
            &std::collections::HashSet::new(),
        )
        .expect("recover update");

        assert_eq!(
            fs::read(managed.join("state.txt")).expect("read restored old"),
            b"old"
        );
        assert!(!staging.exists());
        assert!(!archive.exists());
        assert!(!journal.exists());
    }

    #[test]
    fn update_recovery_preserves_discard_until_library_records_are_checked() {
        let root = temp_workspace("gamesaver-discard-recovery");
        let games_root = root.join("games");
        let quarantined = games_root.join(".game-1.discarding");
        let live_game = games_root.join("game-2");
        fs::create_dir_all(quarantined.join("data")).expect("create quarantine");
        fs::create_dir_all(&live_game).expect("create live game");
        fs::write(live_game.join("state.txt"), b"keep").expect("write live file");

        GameBodyUpdateService::recover_pending_updates(
            &games_root,
            &std::collections::HashSet::new(),
        )
        .expect("recover pending updates");

        assert!(quarantined.exists());
        crate::services::AddGameService::recover_interrupted(
            &games_root,
            &crate::domain::AppStore::default(),
            &[],
        )
        .unwrap();
        assert!(!quarantined.exists());
        assert!(
            live_game.join("state.txt").exists(),
            "真实游戏目录不得被清扫碰掉"
        );
    }

    #[test]
    fn recovery_keeps_new_directory_after_committed_swap() {
        let root = temp_workspace("gamesaver-committed-recovery");
        let games_root = root.join("games");
        let managed = games_root.join("game-1");
        let staging = games_root.join(".game-1.updating");
        let archive = games_root
            .join(".versions")
            .join("game-1")
            .join("version-1");
        fs::create_dir_all(&managed).expect("create managed");
        fs::create_dir_all(&archive).expect("create archive");
        fs::write(managed.join("state.txt"), b"new").expect("write new");
        fs::write(archive.join("state.txt"), b"old").expect("write old");
        let journal = GameBodyUpdateService::write_journal(
            &games_root,
            "game-1",
            &managed,
            &staging,
            &archive,
        )
        .expect("write journal");
        let committed = std::collections::HashSet::from([archive
            .to_string_lossy()
            .replace('/', "\\")
            .to_ascii_lowercase()]);

        GameBodyUpdateService::recover_pending_updates(&games_root, &committed)
            .expect("recover update");

        assert_eq!(
            fs::read(managed.join("state.txt")).expect("read new"),
            b"new"
        );
        assert!(!journal.exists());
    }

    #[test]
    fn cleanup_archived_body_versions_removes_full_old_copy() {
        let root = temp_workspace("gamesaver-cleanup");
        let games_root = root.join("games");
        let archive = games_root
            .join(".versions")
            .join("game-1")
            .join("version-1");
        fs::create_dir_all(&archive).expect("create archive");
        fs::write(archive.join("game.exe"), b"old body").expect("write archive");
        let mut versions = vec![GameBodyVersion {
            version_id: "version-1".to_string(),
            game_uid: "game-1".to_string(),
            created_at: "1".to_string(),
            archive_path: archive.to_string_lossy().to_string(),
            file_count: 1,
            total_bytes: 8,
            package_path: None,
            sha256: None,
            excluded_items: Vec::new(),
            upload_status: None,
            remote_path: None,
            remote_fs_id: None,
            remote_size: None,
        }];

        let removed =
            GameBodyUpdateService::cleanup_archived_body_versions(&games_root, &mut versions)
                .expect("cleanup archived body version");

        assert_eq!(removed, 1);
        assert!(!archive.exists());
        assert!(versions.is_empty());
    }

    #[test]
    fn recovery_removes_orphan_update_staging_directory() {
        let root = temp_workspace("gamesaver-orphan-update");
        let games_root = root.join("games");
        let staging = games_root.join(".game-1.updating");
        fs::create_dir_all(&staging).expect("create staging");
        fs::write(staging.join("partial.bin"), b"partial").expect("write partial");

        GameBodyUpdateService::recover_pending_updates(
            &games_root,
            &std::collections::HashSet::new(),
        )
        .expect("recover update");

        assert!(!staging.exists());
    }

    #[test]
    fn swap_succeeds_when_managed_path_does_not_exist() {
        let root = temp_workspace("gamesaver-missing-body-swap");
        let managed = root.join("games").join("game-1");
        let staging = root.join("games").join(".game-1.updating");
        let archive = root
            .join("games")
            .join(".versions")
            .join("game-1")
            .join("version-1");
        fs::create_dir_all(&staging).expect("create staging");
        fs::write(staging.join("game.exe"), b"new game").expect("write new game");

        assert!(!managed.exists());
        let _swap = GameBodyUpdateService::swap(&managed, &staging, &archive)
            .expect("swap without original managed body");

        assert!(managed.join("game.exe").is_file());
        assert!(!archive.exists());
        assert!(!staging.exists());
        assert_eq!(
            fs::read(managed.join("game.exe")).expect("read new"),
            b"new game"
        );
        let _ = fs::remove_dir_all(root);
    }

    struct UpdateFixture {
        root: TempWorkspace,
        games: std::path::PathBuf,
        managed: std::path::PathBuf,
        staging: std::path::PathBuf,
        archive: std::path::PathBuf,
    }

    impl UpdateFixture {
        fn new(label: &str) -> Self {
            let root = temp_workspace(label);
            let games = root.join("games");
            let managed = games.join("game-1");
            let staging = games.join(".game-1.updating");
            let archive = games.join(".versions/game-1/version-1");
            fs::create_dir_all(&managed).unwrap();
            fs::create_dir_all(&staging).unwrap();
            fs::write(managed.join("game.exe"), b"old body").unwrap();
            fs::write(staging.join("game.exe"), b"new body").unwrap();
            Self {
                root,
                games,
                managed,
                staging,
                archive,
            }
        }

        fn journal(&self) -> std::path::PathBuf {
            GameBodyUpdateService::write_journal(
                &self.games,
                "game-1",
                &self.managed,
                &self.staging,
                &self.archive,
            )
            .unwrap()
        }

        fn swap(&self) -> super::BodySwap {
            GameBodyUpdateService::swap(&self.managed, &self.staging, &self.archive).unwrap()
        }

        fn committed(&self) -> std::collections::HashSet<String> {
            std::collections::HashSet::from([crate::domain::path_utils::normalize_path(
                &self.archive,
            )])
        }

        fn version(&self) -> GameBodyVersion {
            GameBodyVersion {
                version_id: "version-1".into(),
                game_uid: "game-1".into(),
                created_at: "1".into(),
                archive_path: self.archive.to_string_lossy().into_owned(),
                file_count: 1,
                total_bytes: 8,
                package_path: None,
                sha256: None,
                excluded_items: Vec::new(),
                upload_status: None,
                remote_path: None,
                remote_fs_id: None,
                remote_size: None,
            }
        }
    }

    #[test]
    fn pending_update_probe_fails_closed_when_library_disconnects_and_reconnects() {
        let fixture = UpdateFixture::new("update-library-detached");
        let game_uid = "game-1";
        assert!(!GameBodyUpdateService::has_pending_update(&fixture.games, game_uid).unwrap());
        let journal = fixture.journal();
        let detached = fixture.root.join("detached");
        fs::rename(&fixture.games, &detached).unwrap();
        assert!(GameBodyUpdateService::has_pending_update(&fixture.games, game_uid).is_err());
        assert!(GameBodyUpdateService::ensure_no_pending_update(&fixture.games, game_uid).is_err());
        assert!(
            GameBodyUpdateService::pending_game_uids(&detached.join("missing-library/games"))
                .is_err()
        );
        fs::rename(&detached, &fixture.games).unwrap();
        assert!(GameBodyUpdateService::ensure_no_pending_update(&fixture.games, game_uid).is_err());
        fs::remove_file(journal).unwrap();
        GameBodyUpdateService::ensure_no_pending_update(&fixture.games, game_uid).unwrap();
    }

    #[test]
    fn malformed_update_journal_keeps_all_recovery_candidates() {
        let fixture = UpdateFixture::new("update-bad-journal");
        let journal = fixture.journal();
        fs::write(&journal, b"{broken").unwrap();
        assert!(GameBodyUpdateService::recover_pending_updates(
            &fixture.games,
            &Default::default()
        )
        .is_err());
        assert_eq!(
            fs::read(fixture.staging.join("game.exe")).unwrap(),
            b"new body"
        );
        assert_eq!(
            fs::read(fixture.managed.join("game.exe")).unwrap(),
            b"old body"
        );
        assert_eq!(
            GameBodyUpdateService::pending_game_uids(&fixture.games).unwrap(),
            vec!["game-1"]
        );
    }

    #[test]
    fn failed_recovery_does_not_allow_global_archive_cleanup() {
        let fixture = UpdateFixture::new("update-recovery-conflict");
        let journal = fixture.journal();
        fs::create_dir_all(fixture.archive.parent().unwrap()).unwrap();
        fs::rename(&fixture.managed, &fixture.archive).unwrap();
        fs::write(&fixture.managed, b"conflicting file").unwrap();
        assert!(GameBodyUpdateService::recover_pending_updates(
            &fixture.games,
            &fixture.committed()
        )
        .is_err());
        let mut versions = vec![fixture.version()];
        assert_eq!(
            GameBodyUpdateService::cleanup_archived_body_versions(&fixture.games, &mut versions)
                .unwrap(),
            0
        );
        assert!(journal.is_file());
        assert_eq!(
            fs::read(fixture.staging.join("game.exe")).unwrap(),
            b"new body"
        );
        assert_eq!(
            fs::read(fixture.archive.join("game.exe")).unwrap(),
            b"old body"
        );
    }

    #[test]
    fn metadata_settle_failure_retains_committed_phase_for_retry() {
        let fixture = UpdateFixture::new("update-settle-failure");
        let journal = fixture.journal();
        fixture.swap();
        assert!(GameBodyUpdateService::recover_game_update(
            &fixture.games,
            "game-1",
            &fixture.committed(),
            |_| Err("store write failed".into())
        )
        .is_err());
        assert!(journal.is_file());
        assert_eq!(
            super::read_journal(&fixture.games, "game-1").unwrap().phase,
            super::UpdatePhase::Committed
        );
        assert_eq!(
            fs::read(fixture.managed.join("game.exe")).unwrap(),
            b"new body"
        );
        assert!(GameBodyUpdateService::recover_game_update(
            &fixture.games,
            "game-1",
            &Default::default(),
            |_| Ok(())
        )
        .unwrap());
        assert!(!journal.exists());
        assert!(!fixture.archive.exists());
    }

    #[cfg(windows)]
    #[test]
    fn locked_old_body_keeps_journal_and_can_be_retried() {
        use std::os::windows::fs::OpenOptionsExt;
        let fixture = UpdateFixture::new("update-locked-archive");
        let journal = fixture.journal();
        fixture.swap();
        let locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(fixture.archive.join("game.exe"))
            .unwrap();
        assert!(GameBodyUpdateService::recover_game_update(
            &fixture.games,
            "game-1",
            &fixture.committed(),
            |_| Ok(())
        )
        .is_err());
        assert!(journal.is_file());
        assert!(fixture.archive.join("game.exe").is_file());
        drop(locked);
        assert!(GameBodyUpdateService::recover_game_update(
            &fixture.games,
            "game-1",
            &Default::default(),
            |_| Ok(())
        )
        .unwrap());
        assert!(!journal.exists());
        assert!(!fixture.archive.exists());
        assert_eq!(
            fs::read(fixture.managed.join("game.exe")).unwrap(),
            b"new body"
        );
    }

    #[test]
    fn missing_original_archive_never_deletes_the_new_body_on_rollback() {
        let fixture = UpdateFixture::new("update-missing-original");
        let swap = fixture.swap();
        fs::remove_dir_all(&fixture.archive).unwrap();
        assert!(GameBodyUpdateService::rollback(&fixture.managed, &swap).is_err());
        assert_eq!(
            fs::read(fixture.managed.join("game.exe")).unwrap(),
            b"new body"
        );
    }

    #[test]
    fn archive_cleanup_rejects_live_games_and_parent_traversal() {
        let fixture = UpdateFixture::new("update-archive-boundary");
        let outside = fixture.root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.txt"), b"keep").unwrap();
        fs::create_dir_all(fixture.games.join(".versions")).unwrap();
        for target in [
            &fixture.managed,
            &fixture.games.join(".versions/../../outside"),
        ] {
            let mut version = fixture.version();
            version.archive_path = target.to_string_lossy().into_owned();
            assert!(GameBodyUpdateService::cleanup_archived_body_versions(
                &fixture.games,
                &mut vec![version]
            )
            .is_err());
        }
        assert!(outside.join("keep.txt").is_file());
        assert!(fixture.managed.join("game.exe").is_file());
    }

    #[test]
    fn update_target_must_match_the_current_library_and_game() {
        let fixture = UpdateFixture::new("update-target-boundary");
        let mut game = Game::new_pending("Game", fixture.managed.to_string_lossy(), "game.exe");
        game.game_uid = "game-1".into();
        assert!(GameBodyUpdateService::validate_managed_game(&fixture.games, &game).is_ok());
        game.managed_path = fixture
            .root
            .join("outside/game-1")
            .to_string_lossy()
            .into_owned();
        assert!(GameBodyUpdateService::validate_managed_game(&fixture.games, &game).is_err());
        game.game_uid = "../outside".into();
        assert!(GameBodyUpdateService::validate_managed_game(&fixture.games, &game).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn canonical_alias_of_managed_source_is_rejected() {
        let fixture = UpdateFixture::new("update-source-alias");
        let game = Game::new_pending("Game", fixture.managed.to_string_lossy(), "game.exe");
        assert!(GameBodyUpdateService::validate_source(
            &fixture.managed.canonicalize().unwrap(),
            &game
        )
        .is_err());
    }

    #[test]
    fn preflight_can_be_cancelled_while_counting_files() {
        let fixture = UpdateFixture::new("update-preflight-cancel");
        let source = fixture.root.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("game.exe"), b"new").unwrap();
        let game = Game::new_pending("Game", fixture.managed.to_string_lossy(), "game.exe");
        let cancelled = std::cell::Cell::new(false);
        let result = GameBodyUpdateService::validate_source_with(
            &source,
            &game,
            |_, _| cancelled.set(true),
            || cancelled.get(),
        );
        assert_eq!(result.err().as_deref(), Some("任务已取消"));
        assert_eq!(
            fs::read(fixture.managed.join("game.exe")).unwrap(),
            b"old body"
        );
    }

    #[test]
    fn pending_update_cannot_be_overwritten_by_a_new_copy() {
        let fixture = UpdateFixture::new("update-no-overwrite");
        let source = fixture.root.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("game.exe"), b"new").unwrap();
        let game = Game::new_pending("Game", fixture.managed.to_string_lossy(), "game.exe");
        let plan = GameBodyUpdateService::validate_source(&source, &game).unwrap();
        let journal = fixture.journal();
        let original = fs::read(&journal).unwrap();
        assert!(GameBodyUpdateService::write_journal(
            &fixture.games,
            "game-1",
            &fixture.managed,
            &fixture.staging,
            &fixture.archive
        )
        .is_err());
        assert!(GameBodyUpdateService::copy_to_staging(
            &plan,
            &fixture.games,
            "game-1",
            |_, _| {},
            || false
        )
        .is_err());
        assert_eq!(fs::read(journal).unwrap(), original);
        assert_eq!(
            fs::read(fixture.staging.join("game.exe")).unwrap(),
            b"new body"
        );
    }

    #[cfg(windows)]
    #[test]
    fn archive_cleanup_rejects_a_junction_in_its_parent_path() {
        use std::os::windows::process::CommandExt;

        let fixture = UpdateFixture::new("update-junction-boundary");
        let outside = fixture.root.join("outside");
        fs::create_dir_all(outside.join("version-1")).unwrap();
        fs::write(outside.join("version-1/keep.txt"), b"keep").unwrap();
        fs::create_dir_all(fixture.games.join(".versions")).unwrap();
        let link = fixture.games.join(".versions").join("game-1");
        let result = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "junction fixture creation failed: {result:?}"
        );
        let result = GameBodyUpdateService::cleanup_archived_body_versions(
            &fixture.games,
            &mut vec![fixture.version()],
        );
        assert!(result.is_err());
        assert_eq!(
            fs::read(outside.join("version-1/keep.txt")).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn journal_cannot_retarget_another_game() {
        let fixture = UpdateFixture::new("update-forged-journal");
        let journal_path = fixture.journal();
        let mut journal = super::read_journal(&fixture.games, "game-1").unwrap();
        journal.managed_path = fixture.games.join("game-2").to_string_lossy().into_owned();
        super::write_journal_record(&journal_path, &journal).unwrap();
        assert!(GameBodyUpdateService::recover_pending_updates(
            &fixture.games,
            &Default::default()
        )
        .is_err());
        assert!(fixture.staging.join("game.exe").is_file());
        assert!(fixture.managed.join("game.exe").is_file());
    }

    #[test]
    fn source_changes_during_copy_are_rejected_without_touching_original() {
        let fixture = UpdateFixture::new("update-source-change");
        let source = fixture.root.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("game.exe"), b"new").unwrap();
        let game = Game::new_pending("Game", fixture.managed.to_string_lossy(), "game.exe");
        let plan = GameBodyUpdateService::validate_source(&source, &game).unwrap();
        fs::write(source.join("extra.bin"), b"source changed").unwrap();
        assert!(GameBodyUpdateService::copy_to_staging(
            &plan,
            &fixture.games,
            "game-1",
            |_, _| {},
            || false
        )
        .is_err());
        assert!(!fixture.staging.exists());
        assert_eq!(
            fs::read(fixture.managed.join("game.exe")).unwrap(),
            b"old body"
        );
    }
}
