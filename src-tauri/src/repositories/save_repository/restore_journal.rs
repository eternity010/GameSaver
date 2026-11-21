use super::{
    is_reparse_point, is_restore_artifact_id, path_is_restore_artifact, restore_rollback_contents,
    safe_join, sha256_file, validate_restore_file, validate_restore_root, RestoreGroup,
    RESTORE_ROLLBACK_PREFIX, RESTORE_STAGING_PREFIX,
};
use crate::{domain::AppStore, repositories::store_file::atomic_replace};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};
use uuid::Uuid;

const JOURNAL_DIRECTORY: &str = "save-restore-journals";

#[derive(Serialize, Deserialize)]
pub(super) struct Journal {
    id: String,
    game_uid: String,
    groups: Vec<JournalGroup>,
}

#[derive(Serialize, Deserialize)]
struct JournalGroup {
    root: PathBuf,
    new_files: std::collections::BTreeMap<String, String>,
}

impl Journal {
    pub(super) fn begin(
        directory: &Path,
        game_uid: &str,
        groups: &[RestoreGroup],
    ) -> Result<Self, String> {
        let mut journal = Self {
            id: Uuid::new_v4().simple().to_string(),
            game_uid: game_uid.to_string(),
            groups: Vec::with_capacity(groups.len()),
        };
        for group in groups {
            let mut new_files = std::collections::BTreeMap::new();
            for (relative, hash) in &group.entries {
                if !validate_restore_file(&safe_join(&group.root, relative)?)? {
                    new_files.insert(relative.clone(), hash.to_ascii_lowercase());
                }
            }
            journal.groups.push(JournalGroup {
                root: group.root.clone(),
                new_files,
            });
        }
        let bytes =
            serde_json::to_vec(&journal).map_err(|err| format!("序列化存档恢复事务失败：{err}"))?;
        atomic_replace(&journal.path(directory), &bytes, "存档恢复事务")?;
        Ok(journal)
    }

    pub(super) fn id(&self) -> &str {
        &self.id
    }

    pub(super) fn was_new(&self, root: &Path, relative: &str) -> bool {
        self.groups
            .iter()
            .find(|group| group.root == root)
            .is_some_and(|group| group.new_files.contains_key(relative))
    }

    fn path(&self, directory: &Path) -> PathBuf {
        directory.join(format!("{}.json", self.id))
    }

    pub(super) fn settle(&self, directory: &Path, committed: bool) -> Result<(), String> {
        for group in &self.groups {
            validate_restore_root(&group.root)?;
        }
        if !committed {
            for group in self.groups.iter().rev() {
                group.rollback(&self.id)?;
            }
        }
        for group in &self.groups {
            let staging = work_path(&group.root, RESTORE_STAGING_PREFIX, &self.id);
            let rollback = work_path(&group.root, RESTORE_ROLLBACK_PREFIX, &self.id);
            remove_work_directory(&staging)?;
            remove_work_directory(&rollback)?;
        }
        fs::remove_file(self.path(directory)).map_err(|err| format!("清理存档恢复事务失败：{err}"))
    }
}

impl JournalGroup {
    fn rollback(&self, id: &str) -> Result<(), String> {
        let staging = work_path(&self.root, RESTORE_STAGING_PREFIX, id);
        let rollback = work_path(&self.root, RESTORE_ROLLBACK_PREFIX, id);
        if work_directory_exists(&staging)? && validate_restore_file(&ready_path(&staging, id))? {
            for (relative, hash) in &self.new_files {
                if path_is_restore_artifact(relative) {
                    return Err("存档恢复事务包含无效的新文件路径".to_string());
                }
                if validate_restore_file(&safe_join(&staging, relative)?)? {
                    continue;
                }
                let destination = safe_join(&self.root, relative)?;
                if validate_restore_file(&destination)? {
                    if sha256_file(&destination)? != *hash {
                        return Err(format!(
                            "中断后存档已发生变化，已保留现场：{}",
                            destination.display()
                        ));
                    }
                    fs::remove_file(&destination).map_err(|err| {
                        format!("撤销恢复写入失败：{}：{err}", destination.display())
                    })?;
                }
            }
        }
        if work_directory_exists(&rollback)? {
            restore_rollback_contents(&self.root, &rollback)?;
        }
        Ok(())
    }
}

pub(super) fn directory(data_dir: &Path) -> PathBuf {
    data_dir.join(JOURNAL_DIRECTORY)
}

pub(super) fn work_path(root: &Path, prefix: &str, id: &str) -> PathBuf {
    root.join(format!("{prefix}{id}"))
}

fn ready_path(staging: &Path, id: &str) -> PathBuf {
    work_path(staging, RESTORE_STAGING_PREFIX, id)
}

pub(super) fn mark_ready(staging: &Path, id: &str) -> Result<(), String> {
    fs::File::options()
        .write(true)
        .create_new(true)
        .open(ready_path(staging, id))
        .and_then(|file| file.sync_all())
        .map_err(|err| format!("写入存档恢复准备标记失败：{err}"))
}

pub(super) fn recover(data_dir: &Path, store: &mut AppStore) -> Result<(), String> {
    let directory = directory(data_dir);
    for journal in list(&directory)? {
        journal.settle(
            &directory,
            store.committed_save_restores.contains(journal.id()),
        )?;
        store.committed_save_restores.remove(journal.id());
    }
    store.committed_save_restores.clear();
    Ok(())
}

pub(super) fn has_pending(data_dir: &Path, game_uid: &str) -> Result<bool, String> {
    Ok(list(&directory(data_dir))?
        .iter()
        .any(|journal| journal.game_uid == game_uid))
}

fn list(directory: &Path) -> Result<Vec<Journal>, String> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("读取存档恢复事务目录失败：{err}")),
    };
    let mut journals = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| format!("读取存档恢复事务条目失败：{err}"))?;
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_suffix(".json"))
            .filter(|id| is_restore_artifact_id(id))
        else {
            continue;
        };
        validate_restore_file(&entry.path())?;
        let bytes = fs::read(entry.path()).map_err(|err| format!("读取存档恢复事务失败：{err}"))?;
        let journal: Journal =
            serde_json::from_slice(&bytes).map_err(|err| format!("解析存档恢复事务失败：{err}"))?;
        if journal.id != id || journal.groups.iter().any(|group| !group.root.is_absolute()) {
            return Err("存档恢复事务记录无效，已保留现场".to_string());
        }
        journals.push(journal);
    }
    Ok(journals)
}

fn work_directory_exists(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !is_reparse_point(&metadata) => Ok(true),
        Ok(_) => Err(format!("存档恢复工作路径不是普通目录：{}", path.display())),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(format!(
            "检查存档恢复工作目录失败：{}：{err}",
            path.display()
        )),
    }
}

fn remove_work_directory(path: &Path) -> Result<(), String> {
    if work_directory_exists(path)? {
        fs::remove_dir_all(path)
            .map_err(|err| format!("清理存档恢复工作目录失败：{}：{err}", path.display()))?;
    }
    Ok(())
}
