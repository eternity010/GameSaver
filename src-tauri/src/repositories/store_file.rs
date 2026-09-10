//! 单文件 JSON 存储的原子写入与崩溃恢复。
//!
//! GameSaver 有四份「单文件 JSON」形态的持久化数据：游戏库 `store.json`、
//! 任务记录 `tasks.json`、游戏库配置 `library-config.json`、百度网盘配置
//! `baidu-netdisk-config.json`。它们原先各自复制了一份相同的落盘逻辑，
//! 于是也复制了同一个缺陷，这里统一收口。
//!
//! 落盘流程是「写临时文件 → 原文件改名为备份 → 临时文件改名到位 → 删备份」。
//! 在第二步与第三步之间进程若被终止（断电、任务管理器结束进程、崩溃），
//! 目标文件会停留在「不存在」状态，而 `.bak-<uuid>` 就躺在它旁边。此时上层
//! 若只看 `path.exists()`，会把这种情况误判成「首次运行」并返回空数据，下一次
//! 写入再用空数据覆盖现场——用户的整个游戏库就这样静默消失。
//!
//! [`load_with_recovery`] 负责在这种情形下把数据救回来：扫描同目录下的
//! `.tmp-*` / `.bak-*` 残留，按修改时间取最新且能通过上层校验的候选写回主文件，
//! 然后继续正常启动流程。

use std::{
    fs,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use uuid::Uuid;

/// 残留文件被视为「陈旧」的时长阈值。
///
/// 只有存在超过该时长的残留才会在恢复后被清理，避免误删其它线程正在写入的
/// 临时文件（写临时文件与改名之间的窗口虽然极窄，但并非不可能被撞上）。
const RESIDUE_MIN_AGE: Duration = Duration::from_secs(60);

/// 原子写入：先写临时文件并落盘，再把原文件改名为备份，最后把临时文件改名到位。
///
/// 调用前无需自行创建目录，本函数会补齐；`label` 只用于拼装错误信息，例如
/// 「游戏库数据」会得到「提交游戏库数据失败：…」。
pub fn atomic_replace(target: &Path, bytes: &[u8], label: &str) -> Result<(), String> {
    if let Some(parent) = target
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|error| format!("创建{label}目录失败：{error}"))?;
    }
    let temporary = residue_path(target, "tmp");
    let backup = residue_path(target, "bak");
    let result = (|| -> Result<(), String> {
        let mut file = fs::File::create(&temporary)
            .map_err(|error| format!("创建{label}临时文件失败：{error}"))?;
        file.write_all(bytes)
            .map_err(|error| format!("写入{label}临时文件失败：{error}"))?;
        file.sync_all()
            .map_err(|error| format!("刷新{label}临时文件失败：{error}"))?;
        drop(file);
        let had_target = target.exists();
        if had_target {
            fs::rename(target, &backup).map_err(|error| format!("暂存{label}失败：{error}"))?;
        }
        if let Err(error) = fs::rename(&temporary, target) {
            if had_target {
                let _ = fs::rename(&backup, target);
            }
            return Err(format!("提交{label}失败：{error}"));
        }
        if had_target {
            let _ = fs::remove_file(&backup);
        }
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result
}

/// 读取单文件存储的完整内容；主文件缺失时尝试从崩溃残留中恢复。
///
/// - `Ok(Some(bytes))`：正常读到内容，或已从残留中恢复出内容。
/// - `Ok(None)`：目标确实不存在，也没有任何残留——真正的首次运行。
/// - `Err(_)`：读取失败，或存在残留但无一能通过 `is_valid`，此时刻意不返回
///   空数据，以免把「有数据但读不出来」伪装成「没有数据」。
///
/// `is_valid` 由调用方提供，用于判定候选内容能否被上层解析。恢复成功后会把内容
/// 原子写回主文件；写回失败则保留残留，下次启动仍会重试。
pub fn load_with_recovery<F>(
    target: &Path,
    label: &str,
    is_valid: F,
) -> Result<Option<Vec<u8>>, String>
where
    F: Fn(&[u8]) -> bool,
{
    match fs::read(target) {
        Ok(bytes) => return Ok(Some(bytes)),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(format!("读取{label}失败：{error}")),
    }

    let residues = collect_residue(target);
    if residues.is_empty() {
        return Ok(None);
    }

    let mut recovered = None;
    for residue in &residues {
        let Ok(bytes) = fs::read(&residue.path) else {
            continue;
        };
        if is_valid(&bytes) {
            recovered = Some((bytes, residue.path.clone()));
            break;
        }
    }

    let (bytes, source) = match recovered {
        Some(found) => found,
        None => {
            let names = residues
                .iter()
                .map(|residue| residue.path.display().to_string())
                .collect::<Vec<_>>()
                .join("、");
            return Err(format!(
                "{label}主文件缺失，且残留文件均无法解析：{names}。为避免覆盖现场已保留原样，请人工确认后处理"
            ));
        }
    };

    crate::logging::warn(format!(
        "{label}主文件缺失，已从残留文件恢复：{}（来源 {}）",
        target.display(),
        source.display()
    ));

    match atomic_replace(target, &bytes, label) {
        // 写回成功后才清理残留，避免写回失败时把唯一的恢复来源删掉。
        Ok(()) => prune_residue(&residues, SystemTime::now()),
        Err(error) => crate::logging::warn(format!(
            "{label}恢复结果写回失败，残留文件已保留，下次启动会再次尝试：{error}"
        )),
    }

    Ok(Some(bytes))
}

struct Residue {
    path: PathBuf,
    modified: SystemTime,
    /// 临时文件承载的是「尚未提交的新数据」，同等修改时间下优先于备份。
    is_temporary: bool,
}

fn residue_name(target: &Path) -> String {
    target
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn residue_path(target: &Path, kind: &str) -> PathBuf {
    target.with_file_name(format!(
        ".{}.{kind}-{}",
        residue_name(target),
        Uuid::new_v4().simple()
    ))
}

fn residue_prefix(target: &Path, kind: &str) -> String {
    format!(".{}.{kind}-", residue_name(target))
}

/// 收集同目录下属于 `target` 的 `.tmp-*` / `.bak-*` 残留，按可信度降序排列。
///
/// 排序依据：修改时间倒序（越新越可能是崩溃前刚要提交的数据），同一时刻优先
/// 临时文件。备份文件的修改时间是它还是主文件时的旧时间，因此正常情况下临时
/// 文件总是排在前面。
fn collect_residue(target: &Path) -> Vec<Residue> {
    let Some(directory) = target.parent() else {
        return Vec::new();
    };
    let temporary_prefix = residue_prefix(target, "tmp");
    let backup_prefix = residue_prefix(target, "bak");
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };

    let mut residues = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_temporary = name.starts_with(&temporary_prefix);
        if !is_temporary && !name.starts_with(&backup_prefix) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        residues.push(Residue {
            path: entry.path(),
            modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            is_temporary,
        });
    }

    residues.sort_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then(right.is_temporary.cmp(&left.is_temporary))
    });
    residues
}

/// 删除早于 `threshold - RESIDUE_MIN_AGE` 的残留文件。
fn prune_residue(residues: &[Residue], threshold: SystemTime) {
    let Some(oldest_kept) = threshold.checked_sub(RESIDUE_MIN_AGE) else {
        return;
    };
    for residue in residues {
        if residue.modified >= oldest_kept {
            continue;
        }
        let _ = fs::remove_file(&residue.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox() -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("gamesaver-store-file-{}", Uuid::new_v4().simple()));
        fs::create_dir_all(&directory).expect("create sandbox");
        directory
    }

    fn accepts(bytes: &[u8]) -> bool {
        bytes.starts_with(b"valid")
    }

    /// 模拟原子替换在第 2、3 步之间崩溃：主文件已被改名为备份，新文件尚未到位。
    fn stage_crash(directory: &Path, target: &Path, residue: &str, bytes: &[u8]) -> PathBuf {
        let path = directory.join(format!(".{}.{residue}", residue_name(target)));
        fs::write(&path, bytes).expect("write residue");
        path
    }

    #[test]
    fn atomic_replace_creates_and_overwrites() {
        let directory = sandbox();
        let file = directory.join("test_store.json");

        atomic_replace(&file, b"{\"test\": 1}", "测试数据").expect("initial create");
        assert_eq!(fs::read_to_string(&file).expect("read 1"), "{\"test\": 1}");

        atomic_replace(&file, b"{\"test\": 2}", "测试数据").expect("overwrite");
        assert_eq!(fs::read_to_string(&file).expect("read 2"), "{\"test\": 2}");

        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn atomic_replace_creates_missing_parent_directory() {
        let directory = sandbox();
        let file = directory
            .join("nested")
            .join("deep")
            .join("test_store.json");

        atomic_replace(&file, b"{}", "测试数据").expect("create with parent");

        assert_eq!(fs::read_to_string(&file).expect("read"), "{}");
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn recovery_reports_absent_data_when_nothing_left_behind() {
        let directory = sandbox();
        let file = directory.join("store.json");

        let loaded = load_with_recovery(&file, "测试数据", accepts).expect("load");

        assert!(loaded.is_none());
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn recovery_restores_from_backup_left_by_crash() {
        let directory = sandbox();
        let file = directory.join("store.json");
        stage_crash(&directory, &file, "bak-0000", b"valid-old");

        let loaded = load_with_recovery(&file, "测试数据", accepts).expect("load");

        assert_eq!(loaded.as_deref(), Some(b"valid-old".as_slice()));
        // 恢复结果要落回主文件，后续 persist 才不会把现场覆盖成空库。
        assert_eq!(
            fs::read_to_string(&file).expect("recovered file"),
            "valid-old"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn recovery_prefers_newest_and_unparseable_candidates_are_skipped() {
        let directory = sandbox();
        let file = directory.join("store.json");
        stage_crash(&directory, &file, "bak-0000", b"valid-old");
        // 后写入 => 修改时间更新；内容无法解析 => 必须回退到更旧但可用的备份。
        stage_crash(&directory, &file, "tmp-0001", b"garbage");

        let loaded = load_with_recovery(&file, "测试数据", accepts).expect("load");

        assert_eq!(loaded.as_deref(), Some(b"valid-old".as_slice()));
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn recovery_prefers_temporary_over_backup() {
        let directory = sandbox();
        let file = directory.join("store.json");
        // 崩溃发生在「主文件已改名、临时文件尚未改名到位」之间，临时文件才是新数据。
        stage_crash(&directory, &file, "bak-0000", b"valid-old");
        stage_crash(&directory, &file, "tmp-0001", b"valid-new");

        let loaded = load_with_recovery(&file, "测试数据", accepts).expect("load");

        assert_eq!(loaded.as_deref(), Some(b"valid-new".as_slice()));
        assert_eq!(
            fs::read_to_string(&file).expect("recovered file"),
            "valid-new"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn recovery_refuses_to_pretend_data_is_missing_when_residue_is_unusable() {
        let directory = sandbox();
        let file = directory.join("store.json");
        stage_crash(&directory, &file, "tmp-0000", b"garbage");

        let result = load_with_recovery(&file, "测试数据", accepts);

        let error = result.expect_err("must surface the failure");
        assert!(error.contains("均无法解析"), "unexpected error: {error}");
        // 现场必须保留，供人工抢救，且绝不能被当成「首次运行」。
        assert!(!file.exists());
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn existing_target_is_used_without_touching_residue() {
        let directory = sandbox();
        let file = directory.join("store.json");
        fs::write(&file, b"valid-current").expect("write target");
        let residue = stage_crash(&directory, &file, "bak-0000", b"valid-old");

        let loaded = load_with_recovery(&file, "测试数据", accepts).expect("load");

        assert_eq!(loaded.as_deref(), Some(b"valid-current".as_slice()));
        assert!(residue.exists(), "residue must be left alone");
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn residue_of_a_different_file_is_ignored() {
        let directory = sandbox();
        let file = directory.join("store.json");
        stage_crash(&directory, &file, "bak-0000", b"valid-old");
        // 同目录下另一个文件的残留不得被误当成自己的备份。
        fs::write(directory.join(".tasks.json.bak-0000"), b"valid-other").expect("write other");

        let loaded = load_with_recovery(&file, "测试数据", accepts).expect("load");

        assert_eq!(loaded.as_deref(), Some(b"valid-old".as_slice()));
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn stale_residue_is_pruned_only_past_the_age_threshold() {
        let directory = sandbox();
        let file = directory.join("store.json");
        let residue = stage_crash(&directory, &file, "bak-0000", b"valid-old");

        let residues = collect_residue(&file);
        assert_eq!(residues.len(), 1);
        // 阈值取「现在」，残留还太新 => 必须保留。
        prune_residue(&residues, SystemTime::now());
        assert!(residue.exists(), "fresh residue must be kept");

        // 阈值推到一小时之后 => 允许清理。
        prune_residue(&residues, SystemTime::now() + Duration::from_secs(3600));
        assert!(!residue.exists(), "stale residue must be pruned");

        let _ = fs::remove_dir_all(&directory);
    }
}
