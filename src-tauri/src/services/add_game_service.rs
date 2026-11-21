use crate::{
    domain::{path_utils::normalize_path, AppStore},
    repositories::store_file::{atomic_replace, load_with_recovery},
    services::{
        disk_space,
        game_removal_service::{validate_target, GameRemovalService},
    },
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

pub const LARGE_SOURCE_WARNING_PREFIX: &str = "LARGE_SOURCE_REQUIRED:";
pub const LARGE_SOURCE_WARNING_BYTES: u64 = 3 * 1024 * 1024 * 1024;

pub struct AddGameService;

pub struct CopyResult {
    pub managed_path: PathBuf,
    pub executable_relative_path: String,
    pub file_count: usize,
    pub total_bytes: u64,
}

#[derive(Serialize, Deserialize)]
struct AddPlan {
    game_uid: String,
    physical_root: String,
}

impl AddGameService {
    pub fn cleanup_copy_artifacts(games_root: &Path, game_uid: &str) -> Result<(), String> {
        validate_add_paths(games_root, game_uid)?;
        let managed_path = games_root.join(game_uid);
        let staging_path = games_root.join(format!(".{game_uid}.copying"));
        let owned = read_add_plan(games_root, game_uid)?.is_some();
        for path in [staging_path, managed_path] {
            if path == games_root.join(game_uid) && path.exists() && !owned {
                return Err("游戏目录缺少可验证的添加事务，已保留现场".into());
            }
            if path.exists() {
                fs::remove_dir_all(&path)
                    .map_err(|err| format!("清理失败的游戏本体失败：{}：{err}", path.display()))?;
            }
        }
        Self::complete_copy(games_root, game_uid)?;
        Ok(())
    }

    pub fn complete_copy(games_root: &Path, game_uid: &str) -> Result<(), String> {
        validate_add_paths(games_root, game_uid)?;
        if games_root.is_dir() {
            let prefix = format!("..{game_uid}.adding.json.");
            for entry in fs::read_dir(games_root).map_err(|error| error.to_string())? {
                let entry = entry.map_err(|error| error.to_string())?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if name
                    .strip_prefix(&prefix)
                    .is_some_and(|suffix| suffix.starts_with("tmp-") || suffix.starts_with("bak-"))
                {
                    validate_target(games_root, &entry.path())?;
                    fs::remove_file(entry.path()).map_err(|error| error.to_string())?;
                }
            }
        }
        match fs::remove_file(add_plan_path(games_root, game_uid)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("清理添加游戏事务失败：{error}")),
        }
    }

    pub fn recover_interrupted(
        games_root: &Path,
        store: &AppStore,
        interrupted_adds: &[String],
    ) -> Result<Vec<String>, String> {
        if !games_root.exists() {
            return Ok(Vec::new());
        }
        let mut failures = Vec::new();
        let mut recovered = Vec::new();
        let mut pending = std::collections::BTreeSet::new();
        for entry in fs::read_dir(games_root).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with('.') {
                continue;
            }
            let name = name.trim_start_matches('.');
            if let Some((uid, suffix)) = name.split_once(".adding.json") {
                if suffix.is_empty() || suffix.starts_with(".tmp-") || suffix.starts_with(".bak-") {
                    pending.insert(uid.to_string());
                }
            } else if let Some(uid) = name.strip_suffix(".copying") {
                pending.insert(uid.to_string());
            } else if let Some(uid) = name.strip_suffix(".discarding") {
                let result = (|| {
                    validate_add_paths(games_root, uid)?;
                    let quarantine = games_root.join(format!(".{uid}.discarding"));
                    validate_target(games_root, &quarantine)?;
                    if let Some(game) = store.games.iter().find(|game| game.game_uid == uid) {
                        let library_root = games_root.parent().ok_or("游戏库路径无效")?;
                        GameRemovalService::validate_game(library_root, game)?;
                        let managed = games_root.join(uid);
                        if managed.exists() {
                            return Err("原目录与放弃目录同时存在，已保留现场".to_string());
                        }
                        fs::rename(&quarantine, managed).map_err(|error| error.to_string())?;
                    } else {
                        fs::remove_dir_all(quarantine).map_err(|error| error.to_string())?;
                    }
                    Ok::<_, String>(())
                })();
                if let Err(error) = result {
                    failures.push(format!("{uid}：{error}"));
                }
            }
        }
        pending.extend(interrupted_adds.iter().cloned());
        for uid in pending {
            let result = (|| {
                validate_add_paths(games_root, &uid)?;
                let owned = read_add_plan(games_root, &uid)?.is_some();
                let referenced = store.games.iter().any(|game| {
                    game.game_uid == uid
                        || normalize_path(Path::new(&game.managed_path))
                            == normalize_path(&games_root.join(&uid))
                });
                if referenced {
                    let staging = games_root.join(format!(".{uid}.copying"));
                    if staging.exists() {
                        fs::remove_dir_all(staging).map_err(|error| error.to_string())?;
                    }
                    Self::complete_copy(games_root, &uid)?;
                } else {
                    if !owned && interrupted_adds.contains(&uid) {
                        let plan = AddPlan {
                            game_uid: uid.clone(),
                            physical_root: normalize_path(
                                &games_root
                                    .canonicalize()
                                    .map_err(|error| error.to_string())?,
                            ),
                        };
                        atomic_replace(
                            &add_plan_path(games_root, &uid),
                            &serde_json::to_vec(&plan).map_err(|error| error.to_string())?,
                            "添加游戏事务",
                        )?;
                    }
                    Self::cleanup_copy_artifacts(games_root, &uid)?;
                }
                recovered.push(uid.clone());
                Ok::<_, String>(())
            })();
            if let Err(error) = result {
                failures.push(format!("{uid}：{error}"));
            }
        }
        if failures.is_empty() {
            Ok(recovered)
        } else {
            Err(failures.join("；"))
        }
    }

    pub fn validate_source(source: &Path, executable: &Path) -> Result<String, String> {
        if !source.is_dir() {
            return Err("游戏本体目录不存在或不可访问".to_string());
        }
        if !executable.is_file() {
            return Err("启动程序不存在或不可访问".to_string());
        }
        let source = source
            .canonicalize()
            .map_err(|err| format!("解析游戏目录失败：{err}"))?;
        let executable = executable
            .canonicalize()
            .map_err(|err| format!("解析启动程序失败：{err}"))?;
        let relative = executable
            .strip_prefix(&source)
            .map_err(|_| "启动程序必须位于游戏本体目录内".to_string())?;
        if relative.as_os_str().is_empty() {
            return Err("启动程序路径无效".to_string());
        }
        Ok(relative.to_string_lossy().replace('\\', "/"))
    }

    #[cfg(test)]
    pub fn copy_to_managed(
        source: &Path,
        games_root: &Path,
        game_uid: &str,
        executable_relative_path: String,
        allow_large_source: bool,
        on_progress: impl Fn(u8, &str),
        is_cancelled: impl Fn() -> bool,
    ) -> Result<CopyResult, String> {
        Self::copy_to_managed_with_commit(
            source,
            games_root,
            game_uid,
            executable_relative_path,
            allow_large_source,
            on_progress,
            is_cancelled,
            || Ok(()),
        )
    }

    pub fn copy_to_managed_with_commit(
        source: &Path,
        games_root: &Path,
        game_uid: &str,
        executable_relative_path: String,
        allow_large_source: bool,
        on_progress: impl Fn(u8, &str),
        is_cancelled: impl Fn() -> bool,
        begin_commit: impl Fn() -> Result<(), String>,
    ) -> Result<CopyResult, String> {
        let source = source
            .canonicalize()
            .map_err(|err| format!("解析游戏目录失败：{err}"))?;
        fs::create_dir_all(games_root).map_err(|err| format!("创建游戏库目录失败：{err}"))?;
        validate_add_paths(games_root, game_uid)?;
        let source_key = normalize_path(&source);
        let root_key = normalize_path(
            &games_root
                .canonicalize()
                .map_err(|error| error.to_string())?,
        );
        if source_key == root_key
            || source_key.starts_with(&(root_key.clone() + "\\"))
            || root_key.starts_with(&(source_key + "\\"))
        {
            return Err("原始游戏目录不能与受管游戏库重叠".into());
        }
        let managed_path = games_root.join(game_uid);
        let staging_path = games_root.join(format!(".{game_uid}.copying"));
        if managed_path.exists() {
            return Err("受管游戏目录已存在".to_string());
        }
        if staging_path.exists() {
            fs::remove_dir_all(&staging_path)
                .map_err(|err| format!("清理上次未完成复制失败：{err}"))?;
        }

        let mut files = Vec::new();
        let mut total_bytes = 0u64;
        let mut last_progress = std::time::Instant::now();
        for entry in WalkDir::new(&source).follow_links(false) {
            if is_cancelled() {
                return Err("任务已取消".into());
            }
            let entry = entry.map_err(|err| format!("扫描游戏目录失败：{err}"))?;
            if entry.file_type().is_symlink() {
                return Err(format!(
                    "游戏目录包含不支持的符号链接：{}",
                    entry.path().display()
                ));
            }
            if entry.file_type().is_file() {
                let metadata = entry
                    .metadata()
                    .map_err(|err| format!("读取游戏文件信息失败：{err}"))?;
                total_bytes = total_bytes.saturating_add(metadata.len());
                files.push((entry.path().to_path_buf(), metadata.len()));
                if needs_large_source_confirmation(total_bytes, allow_large_source) {
                    return Err(format!("{LARGE_SOURCE_WARNING_PREFIX}已扫描游戏大小至少为 {}，超过 3 GB。请确认游戏大小是否正常；确认后继续扫描和复制。", format_size(total_bytes)));
                }
                if last_progress.elapsed() >= std::time::Duration::from_millis(150) {
                    on_progress(
                        2,
                        &format!(
                            "正在扫描游戏目录：{} 个文件，{}",
                            files.len(),
                            format_size(total_bytes)
                        ),
                    );
                    last_progress = std::time::Instant::now();
                }
            }
        }
        if files.is_empty() {
            return Err("游戏目录中没有可复制的文件".to_string());
        }
        if needs_large_source_confirmation(total_bytes, allow_large_source) {
            return Err(format!(
                "{LARGE_SOURCE_WARNING_PREFIX}游戏本体大小为 {}，超过 3 GB。请确认游戏大小是否正常；确认后将继续复制。",
                format_size(total_bytes)
            ));
        }
        disk_space::ensure_available_space(games_root, total_bytes)?;
        on_progress(5, &format!("已扫描 {} 个文件", files.len()));
        let plan = AddPlan {
            game_uid: game_uid.to_string(),
            physical_root: root_key,
        };
        atomic_replace(
            &add_plan_path(games_root, game_uid),
            &serde_json::to_vec(&plan).map_err(|error| error.to_string())?,
            "添加游戏事务",
        )?;
        fs::create_dir_all(&staging_path).map_err(|err| format!("创建游戏暂存目录失败：{err}"))?;

        let result = (|| -> Result<CopyResult, String> {
            for (index, (source_file, _)) in files.iter().enumerate() {
                if is_cancelled() {
                    return Err("任务已取消".to_string());
                }
                let relative = source_file
                    .strip_prefix(&source)
                    .map_err(|err| format!("计算游戏文件相对路径失败：{err}"))?;
                let target = staging_path.join(relative);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|err| format!("创建游戏文件目录失败：{err}"))?;
                }
                fs::copy(source_file, &target)
                    .map_err(|err| format!("复制游戏文件失败（{}）：{err}", relative.display()))?;
                let progress = 5 + (((index + 1) * 90) / files.len()) as u8;
                on_progress(
                    progress.min(95),
                    &format!("正在复制游戏文件 {}/{}", index + 1, files.len()),
                );
            }
            if is_cancelled() {
                return Err("任务已取消".to_string());
            }
            begin_commit()?;
            fs::rename(&staging_path, &managed_path)
                .map_err(|err| format!("提交受管游戏目录失败：{err}"))?;
            on_progress(97, &format!("已复制 {} 个文件，正在登记游戏", files.len()));
            Ok(CopyResult {
                managed_path,
                executable_relative_path,
                file_count: files.len(),
                total_bytes,
            })
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&staging_path);
        }
        result
    }
}

fn add_plan_path(games_root: &Path, game_uid: &str) -> PathBuf {
    games_root.join(format!(".{game_uid}.adding.json"))
}

fn read_add_plan(games_root: &Path, game_uid: &str) -> Result<Option<AddPlan>, String> {
    let raw = load_with_recovery(
        &add_plan_path(games_root, game_uid),
        "添加游戏事务",
        |raw| serde_json::from_slice::<AddPlan>(raw).is_ok(),
    )?;
    raw.map(|raw| {
        let plan: AddPlan = serde_json::from_slice(&raw).map_err(|error| error.to_string())?;
        if plan.game_uid != game_uid
            || plan.physical_root
                != normalize_path(
                    &games_root
                        .canonicalize()
                        .map_err(|error| error.to_string())?,
                )
        {
            return Err("添加事务与当前游戏库不匹配，已保留现场".into());
        }
        Ok(plan)
    })
    .transpose()
}

fn validate_add_paths(games_root: &Path, game_uid: &str) -> Result<(), String> {
    if !games_root.is_absolute()
        || games_root
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
        || !crate::domain::is_safe_path_segment(game_uid)
        || game_uid.starts_with('.')
        || game_uid.ends_with(['.', ' '])
        || game_uid.chars().any(|value| "<>:\"|?*".contains(value))
    {
        return Err("添加游戏路径或标识无效，已停止操作".into());
    }
    for path in [
        games_root.join(game_uid),
        games_root.join(format!(".{game_uid}.copying")),
        add_plan_path(games_root, game_uid),
    ] {
        validate_target(games_root, &path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{add_plan_path, needs_large_source_confirmation, AddGameService, AddPlan};
    use crate::domain::{path_utils::normalize_path, AppStore, Game};
    use crate::test_support::TempWorkspace;
    use std::fs;

    /// 建一棵带 `Drop` 清理的测试目录树，理由见 `crate::test_support::TempWorkspace`。
    ///
    /// 这里三个用例都必须有真实目录（复制/清理都要落盘）。此前清理靠末尾
    /// `remove_dir_all(...).expect(...)`，`assert!` 一失败就执行不到。
    fn temp_root() -> TempWorkspace {
        TempWorkspace::new("next-test")
    }

    #[test]
    fn copies_game_and_preserves_executable_relative_path() {
        let root = temp_root();
        let source = root.join("source");
        let games_root = root.join("games");
        fs::create_dir_all(source.join("bin")).expect("create source");
        fs::write(source.join("bin/game.exe"), b"exe").expect("write exe");
        fs::write(source.join("save.dat"), b"save").expect("write save");

        let relative = AddGameService::validate_source(&source, &source.join("bin/game.exe"))
            .expect("validate source");
        let result = AddGameService::copy_to_managed(
            &source,
            &games_root,
            "game-1",
            relative,
            false,
            |_, _| {},
            || false,
        )
        .expect("copy game");
        assert_eq!(result.executable_relative_path, "bin/game.exe");
        assert_eq!(
            fs::read(result.managed_path.join("save.dat")).expect("read copied file"),
            b"save"
        );
        fs::remove_dir_all(root).expect("cleanup test directory");
    }

    #[test]
    fn cancellation_removes_staging_directory() {
        let root = temp_root();
        let source = root.join("source");
        let games_root = root.join("games");
        fs::create_dir_all(&source).expect("create source");
        fs::write(source.join("game.exe"), b"exe").expect("write exe");
        let result = AddGameService::copy_to_managed(
            &source,
            &games_root,
            "game-2",
            "game.exe".to_string(),
            false,
            |_, _| {},
            || true,
        );
        assert!(result.is_err());
        assert!(!games_root.join("game-2").exists());
        assert!(!games_root.join(".game-2.copying").exists());
        fs::remove_dir_all(root).expect("cleanup test directory");
    }

    #[test]
    fn cleanup_removes_managed_and_staging_directories() {
        let root = temp_root();
        let games_root = root.join("games");
        fs::create_dir_all(games_root.join("game-3/bin")).expect("create managed directory");
        fs::create_dir_all(games_root.join(".game-3.copying/bin"))
            .expect("create staging directory");
        fs::write(games_root.join("game-3/bin/game.exe"), b"exe").expect("write managed file");
        fs::write(games_root.join(".game-3.copying/bin/game.exe"), b"exe")
            .expect("write staging file");
        let plan = AddPlan {
            game_uid: "game-3".into(),
            physical_root: normalize_path(&games_root.canonicalize().unwrap()),
        };
        fs::write(
            add_plan_path(&games_root, "game-3"),
            serde_json::to_vec(&plan).unwrap(),
        )
        .unwrap();

        AddGameService::cleanup_copy_artifacts(&games_root, "game-3").expect("cleanup artifacts");

        assert!(!games_root.join("game-3").exists());
        assert!(!games_root.join(".game-3.copying").exists());
        fs::remove_dir_all(root).expect("cleanup test directory");
    }

    #[test]
    fn large_source_warning_uses_binary_three_gigabyte_boundary() {
        let boundary = 3 * 1024 * 1024 * 1024;
        assert!(!needs_large_source_confirmation(boundary, false));
        assert!(needs_large_source_confirmation(boundary + 1, false));
        assert!(!needs_large_source_confirmation(boundary + 1, true));
    }

    #[test]
    fn cancellation_during_scan_stops_before_copy_and_commit() {
        let root = temp_root();
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        for index in 0..80 {
            fs::write(source.join(format!("{index}.dat")), b"data").unwrap();
        }
        let checks = std::cell::Cell::new(0);
        let result = AddGameService::copy_to_managed_with_commit(
            &source,
            &root.join("games"),
            "scan-cancel",
            "1.dat".into(),
            false,
            |progress, _| assert!(progress < 5),
            || {
                checks.set(checks.get() + 1);
                checks.get() >= 4
            },
            || panic!("cancelled scan must never commit"),
        );
        assert_eq!(result.err().as_deref(), Some("任务已取消"));
        assert_eq!(checks.get(), 4);
        assert!(!root.join("games/scan-cancel").exists());
    }

    #[test]
    fn cancelled_commit_leaves_no_visible_game() {
        let root = temp_root();
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("game.exe"), b"exe").unwrap();
        let games = root.join("games");
        let result = AddGameService::copy_to_managed_with_commit(
            &source,
            &games,
            "cancel-commit",
            "game.exe".into(),
            false,
            |_, _| {},
            || false,
            || Err("任务已取消".into()),
        );
        assert_eq!(result.err().as_deref(), Some("任务已取消"));
        assert!(!games.join("cancel-commit").exists());
        assert!(!games.join(".cancel-commit.copying").exists());
        AddGameService::recover_interrupted(&games, &AppStore::default(), &[]).unwrap();
        assert!(!add_plan_path(&games, "cancel-commit").exists());
    }

    #[test]
    fn startup_removes_only_unregistered_interrupted_additions() {
        let root = temp_root();
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("game.exe"), b"exe").unwrap();
        let games = root.join("games");
        for uid in ["orphan", "registered"] {
            AddGameService::copy_to_managed(
                &source,
                &games,
                uid,
                "game.exe".into(),
                false,
                |_, _| {},
                || false,
            )
            .unwrap();
        }
        fs::create_dir_all(games.join(".staged.copying")).unwrap();
        fs::write(games.join(".staged.copying/data"), b"partial").unwrap();
        let mut store = AppStore::default();
        let mut game = Game::new_pending(
            "Registered",
            games.join("registered").to_string_lossy(),
            "game.exe",
        );
        game.game_uid = "registered".into();
        store.games.push(game);
        AddGameService::recover_interrupted(&games, &store, &[]).unwrap();
        assert!(games.join("registered/game.exe").is_file());
        assert!(!games.join("orphan").exists());
        assert!(!games.join(".staged.copying").exists());
        assert!(!add_plan_path(&games, "registered").exists());
        assert!(source.join("game.exe").is_file());
    }

    #[test]
    fn startup_recovers_add_journal_residue_before_removing_unregistered_body() {
        let root = temp_root();
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("game.exe"), b"exe").unwrap();
        let games = root.join("games");
        AddGameService::copy_to_managed(
            &source,
            &games,
            "residue",
            "game.exe".into(),
            false,
            |_, _| {},
            || false,
        )
        .unwrap();
        let backup = games.join("..residue.adding.json.bak-test");
        fs::rename(add_plan_path(&games, "residue"), &backup).unwrap();
        AddGameService::recover_interrupted(&games, &AppStore::default(), &[]).unwrap();
        assert!(!games.join("residue").exists());
        assert!(!backup.exists());
        assert!(!add_plan_path(&games, "residue").exists());
    }

    #[test]
    fn unsafe_cleanup_ids_and_unowned_body_are_preserved() {
        let root = temp_root();
        let games = root.join("games");
        fs::create_dir_all(games.join("unowned")).unwrap();
        fs::write(games.join("unowned/game.exe"), b"keep").unwrap();
        for uid in ["..", ".", "../outside", "a/b", "a:b", "a.", ""] {
            assert!(AddGameService::cleanup_copy_artifacts(&games, uid).is_err());
        }
        assert!(AddGameService::cleanup_copy_artifacts(&games, "unowned").is_err());
        assert!(games.join("unowned/game.exe").is_file());
    }

    #[test]
    fn corrupt_add_journal_preserves_body() {
        let root = temp_root();
        let games = root.join("games");
        fs::create_dir_all(games.join("protected")).unwrap();
        fs::write(games.join("protected/game.exe"), b"keep").unwrap();
        fs::write(add_plan_path(&games, "protected"), b"invalid json").unwrap();
        assert!(AddGameService::recover_interrupted(&games, &AppStore::default(), &[]).is_err());
        assert!(games.join("protected/game.exe").is_file());
    }

    #[test]
    fn legacy_discard_with_referenced_game_restores_instead_of_deleting() {
        let root = temp_root();
        let games = root.join("games");
        fs::create_dir_all(games.join(".pending.discarding")).unwrap();
        fs::write(games.join(".pending.discarding/game.exe"), b"keep").unwrap();
        let mut game = Game::new_pending(
            "Pending",
            games.join("pending").to_string_lossy(),
            "game.exe",
        );
        game.game_uid = "pending".into();
        let mut store = AppStore::default();
        store.games.push(game);
        AddGameService::recover_interrupted(&games, &store, &[]).unwrap();
        assert!(games.join("pending/game.exe").is_file());
        assert!(!games.join(".pending.discarding").exists());
    }

    #[test]
    fn legacy_discard_rollback_conflict_preserves_both_paths() {
        let root = temp_root();
        let games = root.join("games");
        fs::create_dir_all(games.join(".pending.discarding")).unwrap();
        fs::write(games.join(".pending.discarding/game.exe"), b"original").unwrap();
        fs::create_dir_all(games.join("pending")).unwrap();
        fs::write(games.join("pending/keep.dat"), b"new").unwrap();
        let mut game = Game::new_pending(
            "Pending",
            games.join("pending").to_string_lossy(),
            "game.exe",
        );
        game.game_uid = "pending".into();
        let mut store = AppStore::default();
        store.games.push(game);
        assert!(AddGameService::recover_interrupted(&games, &store, &[]).is_err());
        assert!(games.join(".pending.discarding/game.exe").is_file());
        assert!(games.join("pending/keep.dat").is_file());
    }
}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn needs_large_source_confirmation(total_bytes: u64, allow_large_source: bool) -> bool {
    total_bytes > LARGE_SOURCE_WARNING_BYTES && !allow_large_source
}
