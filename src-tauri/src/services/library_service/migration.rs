use super::{is_descendant, same_path, LibraryService};
use crate::repositories::{
    store_file::{atomic_replace, load_with_recovery},
    LibraryConfig, LibraryConfigRepository,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use uuid::Uuid;

const JOURNAL: &str = "library-migration.json";
const OWNER: &str = ".gamesaver-migration-owner";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Copying,
    Committing,
    Cleanup,
    Complete,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryMigration {
    format_version: u32,
    id: String,
    source: PathBuf,
    target: PathBuf,
    staging: PathBuf,
    phase: Phase,
}

#[derive(Debug)]
pub struct MigrationOutcome {
    pub library_root: PathBuf,
    pub warning: Option<String>,
    pub pending: bool,
}

impl LibraryMigration {
    pub fn pending(data_dir: &Path) -> Result<Option<Self>, String> {
        let Some(raw) =
            load_with_recovery(&data_dir.join(JOURNAL), "游戏库迁移日志", |bytes| {
                serde_json::from_slice::<Self>(bytes).is_ok()
            })?
        else {
            return Ok(None);
        };
        let record: Self = serde_json::from_slice(&raw)
            .map_err(|error| format!("解析游戏库迁移日志失败：{error}"))?;
        if record.format_version != 1 || Uuid::parse_str(&record.id).is_err() {
            return Err("游戏库迁移日志格式无效，已保留现场".to_string());
        }
        if record.phase == Phase::Complete {
            return Ok(None);
        }
        record.validate_paths()?;
        Ok(Some(record))
    }

    pub fn begin(data_dir: &Path, source: &Path, target: &Path) -> Result<Self, String> {
        if Self::pending(data_dir)?.is_some() {
            return Err("请先继续或清理上次未完成的游戏库迁移".to_string());
        }
        LibraryService::validate_target(source, target)?;
        let source = LibraryService::resolve_path(source)?;
        let target = LibraryService::resolve_path(target)?;
        let id = Uuid::new_v4().to_string();
        let staging = target
            .parent()
            .ok_or("新游戏库路径缺少父目录")?
            .join(format!(".gamesaver-library-migration-{id}"));
        let record = Self {
            format_version: 1,
            id,
            source,
            target,
            staging,
            phase: Phase::Copying,
        };
        record.validate_paths()?;
        record.save(data_dir)?;
        Ok(record)
    }

    pub fn source(&self) -> &Path {
        &self.source
    }

    pub fn cleanup_only(&self) -> bool {
        self.phase == Phase::Cleanup
    }

    pub fn copying(&self) -> bool {
        self.phase == Phase::Copying
    }

    pub fn copy(
        &self,
        mut on_progress: impl FnMut(u8, &str) -> Result<(), String>,
    ) -> Result<(), String> {
        self.validate_paths()?;
        if self.phase != Phase::Copying {
            return Err("游戏库迁移不在复制阶段".to_string());
        }
        on_progress(1, "正在复制游戏库文件")?;
        fs::create_dir_all(self.staging.parent().ok_or("迁移暂存目录缺少父目录")?)
            .map_err(|error| format!("创建迁移父目录失败：{error}"))?;
        fs::create_dir(&self.staging).map_err(|error| format!("创建迁移暂存目录失败：{error}"))?;
        let mut owner = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(self.staging.join(OWNER))
            .map_err(|error| format!("创建迁移归属标识失败：{error}"))?;
        owner
            .write_all(self.id.as_bytes())
            .and_then(|_| owner.sync_all())
            .map_err(|error| format!("写入迁移归属标识失败：{error}"))?;
        LibraryService::copy_contents(&self.source, &self.staging, on_progress)
    }

    pub fn mark_ready(&mut self, data_dir: &Path) -> Result<(), String> {
        if self.phase != Phase::Copying || !self.owns(&self.staging) {
            return Err("迁移暂存目录归属或状态无效".to_string());
        }
        let mut candidate = self.clone();
        candidate.phase = Phase::Committing;
        candidate.save(data_dir)?;
        *self = candidate;
        Ok(())
    }

    pub fn commit(
        &mut self,
        data_dir: &Path,
        mut persist_paths: impl FnMut(&Path, &Path) -> Result<(), String>,
    ) -> Result<(), String> {
        self.validate_paths()?;
        if self.phase == Phase::Cleanup {
            return Ok(());
        }
        if self.phase != Phase::Committing {
            return Err("迁移尚未进入提交阶段".to_string());
        }
        if !self.owns(&self.target) {
            if !self.owns(&self.staging) {
                return Err("未找到归属于本次迁移的新库，已保留原库及迁移记录".to_string());
            }
            if self.target.exists() {
                fs::remove_dir(&self.target)
                    .map_err(|error| format!("新库目录已被占用，停止提交：{error}"))?;
            }
            fs::rename(&self.staging, &self.target)
                .map_err(|error| format!("提交新游戏库文件失败：{error}"))?;
        }
        persist_paths(&self.source, &self.target)?;
        LibraryConfigRepository::save(
            data_dir,
            &LibraryConfig {
                library_root: Some(self.target.to_string_lossy().to_string()),
            },
        )?;
        let mut candidate = self.clone();
        candidate.phase = Phase::Cleanup;
        candidate.save(data_dir)?;
        *self = candidate;
        Ok(())
    }

    pub fn cleanup(&mut self, data_dir: &Path) -> MigrationOutcome {
        let result = (|| {
            self.validate_paths()?;
            if self.phase != Phase::Cleanup
                || !same_path(
                    &LibraryConfigRepository::resolve_root(data_dir)?,
                    &self.target,
                )
            {
                return Err("游戏库引用尚未确认切换，已保留旧目录".to_string());
            }
            LibraryService::cleanup_source(&self.source)?;
            if self.target.join(OWNER).exists() {
                fs::remove_file(self.target.join(OWNER))
                    .map_err(|error| format!("清理迁移标识失败：{error}"))?;
            }
            let mut candidate = self.clone();
            candidate.phase = Phase::Complete;
            candidate.save(data_dir)?;
            *self = candidate;
            Ok::<(), String>(())
        })();
        MigrationOutcome {
            library_root: self.target.clone(),
            pending: result.is_err(),
            warning: result.err().map(|error| {
                format!(
                    "游戏库已切换；旧目录或迁移收尾尚未完成：{}。{error}。可在平台设置中重试。",
                    self.source.display()
                )
            }),
        }
    }

    pub fn abort_copy(&mut self, data_dir: &Path) -> Result<MigrationOutcome, String> {
        self.validate_paths()?;
        if self.phase != Phase::Copying {
            return Err("迁移已开始提交，必须继续恢复，不能删除新库".to_string());
        }
        if self.staging.exists() {
            if self.owns(&self.staging) {
                fs::remove_dir_all(&self.staging)
                    .map_err(|error| format!("清理迁移暂存目录失败：{error}"))?;
            } else {
                fs::remove_dir(&self.staging)
                    .map_err(|error| format!("暂存目录归属无法确认，已保留：{error}"))?;
            }
        }
        let mut candidate = self.clone();
        candidate.phase = Phase::Complete;
        candidate.save(data_dir)?;
        *self = candidate;
        Ok(MigrationOutcome {
            library_root: self.source.clone(),
            warning: None,
            pending: false,
        })
    }

    pub fn recover(
        data_dir: &Path,
        persist_paths: impl FnMut(&Path, &Path) -> Result<(), String>,
    ) -> Result<Option<MigrationOutcome>, String> {
        let Some(mut record) = Self::pending(data_dir)? else {
            return Ok(None);
        };
        if record.phase == Phase::Copying {
            return record.abort_copy(data_dir).map(Some);
        }
        record.commit(data_dir, persist_paths)?;
        Ok(Some(record.cleanup(data_dir)))
    }

    fn save(&self, data_dir: &Path) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| format!("序列化迁移日志失败：{error}"))?;
        atomic_replace(&data_dir.join(JOURNAL), &bytes, "游戏库迁移日志")
    }

    fn owns(&self, directory: &Path) -> bool {
        fs::read_to_string(directory.join(OWNER))
            .map(|value| value == self.id)
            .unwrap_or(false)
    }

    fn validate_paths(&self) -> Result<(), String> {
        if self.format_version != 1 || Uuid::parse_str(&self.id).is_err() {
            return Err("迁移日志标识无效".to_string());
        }
        let expected = self
            .target
            .parent()
            .ok_or("迁移目标缺少父目录")?
            .join(format!(".gamesaver-library-migration-{}", self.id));
        if self.staging != expected
            || same_path(&self.source, &self.target)
            || is_descendant(&self.source, &self.target)
            || is_descendant(&self.target, &self.source)
        {
            return Err("迁移目录归属或重叠检查失败，已保留现场".to_string());
        }
        for path in [&self.source, &self.target, &self.staging] {
            if !same_path(path, &LibraryService::resolve_path(path)?) {
                return Err(format!(
                    "迁移目录真实位置发生变化，已停止操作：{}",
                    path.display()
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{AppStore, Game},
        test_support::TempWorkspace,
    };

    struct Fixture {
        root: TempWorkspace,
        data: PathBuf,
        source: PathBuf,
        target: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = TempWorkspace::new("library-migration");
            let data = root.join("data");
            let source = root.join("source");
            let target = root.join("target");
            fs::create_dir_all(&data).unwrap();
            for directory in super::super::MANAGED_DIRECTORIES {
                fs::create_dir_all(source.join(directory)).unwrap();
                fs::write(
                    source.join(directory).join("fixture.bin"),
                    directory.as_bytes(),
                )
                .unwrap();
            }
            fs::write(source.join("keep.txt"), b"not managed").unwrap();
            LibraryConfigRepository::save(
                &data,
                &LibraryConfig {
                    library_root: Some(source.to_string_lossy().into_owned()),
                },
            )
            .unwrap();
            let mut store = AppStore::default();
            store.games.push(Game::new_pending(
                "A",
                source.join("games").to_str().unwrap(),
                "fixture.bin",
            ));
            atomic_replace(
                &data.join("store.json"),
                &serde_json::to_vec(&store).unwrap(),
                "test store",
            )
            .unwrap();
            Self {
                root,
                data,
                source,
                target,
            }
        }

        fn ready(&self) -> LibraryMigration {
            let mut record =
                LibraryMigration::begin(&self.data, &self.source, &self.target).unwrap();
            record.copy(|_, _| Ok(())).unwrap();
            record.mark_ready(&self.data).unwrap();
            record
        }

        fn persist_paths(&self, source: &Path, target: &Path) -> Result<(), String> {
            let raw = fs::read(self.data.join("store.json")).map_err(|error| error.to_string())?;
            let mut store: AppStore =
                serde_json::from_slice(&raw).map_err(|error| error.to_string())?;
            LibraryService::rewrite_paths_in_place(&mut store, source, target)?;
            atomic_replace(
                &self.data.join("store.json"),
                &serde_json::to_vec(&store).unwrap(),
                "test store",
            )
        }

        fn recover(&self) -> MigrationOutcome {
            LibraryMigration::recover(&self.data, |source, target| {
                self.persist_paths(source, target)
            })
            .unwrap()
            .unwrap()
        }
    }

    #[test]
    fn migration_moves_all_managed_directories_and_preserves_unrelated_files() {
        let fixture = Fixture::new();
        let _record = fixture.ready();
        let outcome = fixture.recover();
        assert_eq!(outcome.library_root, fixture.target);
        assert!(!outcome.pending);
        assert!(outcome.warning.is_none());
        for directory in super::super::MANAGED_DIRECTORIES {
            assert_eq!(
                fs::read(fixture.target.join(directory).join("fixture.bin")).unwrap(),
                directory.as_bytes()
            );
            assert!(!fixture.source.join(directory).exists());
        }
        assert!(fixture.source.join("keep.txt").exists());
        let store: AppStore =
            serde_json::from_slice(&fs::read(fixture.data.join("store.json")).unwrap()).unwrap();
        assert_eq!(
            PathBuf::from(&store.games[0].managed_path),
            fixture.target.join("games")
        );
        assert_eq!(
            LibraryConfigRepository::resolve_root(&fixture.data).unwrap(),
            fixture.target
        );
        assert!(LibraryMigration::pending(&fixture.data).unwrap().is_none());
    }

    #[test]
    fn interrupted_copy_removes_only_owned_staging_and_keeps_source() {
        let fixture = Fixture::new();
        let record =
            LibraryMigration::begin(&fixture.data, &fixture.source, &fixture.target).unwrap();
        assert!(record
            .copy(|progress, _| if progress >= 10 {
                Err("cancelled".into())
            } else {
                Ok(())
            })
            .is_err());
        assert!(record.staging.exists());
        let outcome =
            LibraryMigration::recover(&fixture.data, |_, _| panic!("copy was never committed"))
                .unwrap()
                .unwrap();
        assert_eq!(outcome.library_root, fixture.source);
        assert!(!record.staging.exists());
        assert!(!fixture.target.exists());
        assert!(fixture.source.join("games/fixture.bin").exists());
        assert!(LibraryMigration::pending(&fixture.data).unwrap().is_none());
    }

    #[test]
    fn store_commit_failure_preserves_both_copies_and_can_resume() {
        let fixture = Fixture::new();
        let mut record = fixture.ready();
        assert!(record
            .commit(&fixture.data, |_, _| Err("injected store failure".into()))
            .is_err());
        assert!(fixture.source.join("games/fixture.bin").exists());
        assert!(fixture.target.join("games/fixture.bin").exists());
        assert_eq!(
            LibraryConfigRepository::resolve_root(&fixture.data).unwrap(),
            fixture.source
        );
        assert!(LibraryMigration::pending(&fixture.data).unwrap().is_some());
        assert!(!fixture.recover().pending);
        assert!(!fixture.source.join("games").exists());
    }

    #[test]
    fn recovery_is_idempotent_after_store_was_already_written() {
        let fixture = Fixture::new();
        let mut record = fixture.ready();
        assert!(record
            .commit(&fixture.data, |source, target| {
                fixture.persist_paths(source, target)?;
                Err("interrupted after store write".into())
            })
            .is_err());
        let mut store: AppStore =
            serde_json::from_slice(&fs::read(fixture.data.join("store.json")).unwrap()).unwrap();
        store.games[0].display_name = "Renamed".into();
        fs::write(
            fixture.data.join("store.json"),
            serde_json::to_vec(&store).unwrap(),
        )
        .unwrap();
        assert!(!fixture.recover().pending);
        let final_store: AppStore =
            serde_json::from_slice(&fs::read(fixture.data.join("store.json")).unwrap()).unwrap();
        assert_eq!(final_store.games[0].display_name, "Renamed");
        assert_eq!(
            PathBuf::from(&final_store.games[0].managed_path),
            fixture.target.join("games")
        );
    }

    #[test]
    fn foreign_target_is_never_removed_or_merged() {
        let fixture = Fixture::new();
        let mut record = fixture.ready();
        fs::create_dir(&fixture.target).unwrap();
        fs::write(fixture.target.join("foreign.txt"), b"keep").unwrap();
        assert!(record
            .commit(&fixture.data, |_, _| panic!("target must be rejected"))
            .is_err());
        assert_eq!(
            fs::read(fixture.target.join("foreign.txt")).unwrap(),
            b"keep"
        );
        assert!(record.staging.join("covers/fixture.bin").exists());
        assert!(fixture.source.join("games/fixture.bin").exists());
    }

    #[test]
    fn ready_migration_cannot_be_aborted_or_replaced() {
        let fixture = Fixture::new();
        let mut record = fixture.ready();
        assert!(record.abort_copy(&fixture.data).is_err());
        assert!(LibraryMigration::begin(
            &fixture.data,
            &fixture.source,
            &fixture.root.join("other")
        )
        .is_err());
        assert!(record.staging.exists());
    }

    #[cfg(windows)]
    fn lock_file(path: &Path) -> fs::File {
        use std::os::windows::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(path)
            .unwrap()
    }

    #[cfg(windows)]
    #[test]
    fn configuration_commit_failure_keeps_target_and_retries_forward() {
        let fixture = Fixture::new();
        let mut record = fixture.ready();
        let handle = lock_file(&fixture.data.join("library-config.json"));
        assert!(record
            .commit(&fixture.data, |source, target| fixture
                .persist_paths(source, target))
            .is_err());
        assert!(fixture.source.join("games/fixture.bin").exists());
        assert!(fixture.target.join("games/fixture.bin").exists());
        drop(handle);
        assert!(!fixture.recover().pending);
        assert_eq!(
            LibraryConfigRepository::resolve_root(&fixture.data).unwrap(),
            fixture.target
        );
    }

    #[cfg(windows)]
    #[test]
    fn cleanup_failure_is_visible_and_retry_does_not_rewrite_store() {
        let fixture = Fixture::new();
        let mut record = fixture.ready();
        record
            .commit(&fixture.data, |source, target| {
                fixture.persist_paths(source, target)
            })
            .unwrap();
        let handle = lock_file(&fixture.source.join("games/fixture.bin"));
        let outcome = record.cleanup(&fixture.data);
        assert!(outcome.pending);
        assert!(outcome.warning.unwrap().contains("重试"));
        assert_eq!(
            LibraryConfigRepository::resolve_root(&fixture.data).unwrap(),
            fixture.target
        );
        assert!(fixture.source.join("games/fixture.bin").exists());
        drop(handle);
        let outcome = LibraryMigration::recover(&fixture.data, |_, _| {
            panic!("cleanup must not rewrite store")
        })
        .unwrap()
        .unwrap();
        assert!(!outcome.pending);
        assert!(!fixture.source.join("games").exists());
    }

    #[cfg(windows)]
    #[test]
    fn ready_journal_write_failure_can_abort_without_deleting_source() {
        let fixture = Fixture::new();
        let mut record =
            LibraryMigration::begin(&fixture.data, &fixture.source, &fixture.target).unwrap();
        record.copy(|_, _| Ok(())).unwrap();
        let handle = lock_file(&fixture.data.join(JOURNAL));
        assert!(record.mark_ready(&fixture.data).is_err());
        assert!(record.copying());
        drop(handle);
        let outcome = fixture.recover();
        assert_eq!(outcome.library_root, fixture.source);
        assert!(fixture.source.join("covers/fixture.bin").exists());
        assert!(!record.staging.exists());
    }

    #[cfg(windows)]
    #[test]
    fn final_journal_write_failure_leaves_a_safe_retryable_cleanup() {
        let fixture = Fixture::new();
        let mut record = fixture.ready();
        record
            .commit(&fixture.data, |source, target| {
                fixture.persist_paths(source, target)
            })
            .unwrap();
        let handle = lock_file(&fixture.data.join(JOURNAL));
        let outcome = record.cleanup(&fixture.data);
        assert!(outcome.pending);
        assert!(fixture.target.join("covers/fixture.bin").exists());
        drop(handle);
        assert!(!fixture.recover().pending);
    }
}
