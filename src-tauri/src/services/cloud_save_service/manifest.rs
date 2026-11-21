use super::*;

pub(super) fn read_manifest(
    game_key: &str,
    files: Option<Vec<RemoteFile>>,
    download: impl FnOnce(&RemoteFile) -> Result<Vec<u8>, String>,
) -> Result<Option<CloudSaveManifest>, String> {
    let Some(files) = files else {
        return Ok(None);
    };
    let path = CloudSaveService::remote_manifest_path(game_key)?;
    let Some(file) = files.iter().find(|file| file.path == path && !file.is_dir) else {
        if files
            .iter()
            .any(|file| !file.is_dir && file.path.ends_with(".zip"))
        {
            return Err("云端存在存档包但缺少清单，已停止同步以保护已有历史版本".to_string());
        }
        return Ok(None);
    };
    if file.size > MAX_SAVE_PACKAGE_META_BYTES {
        return Err("云端存档清单过大".to_string());
    }
    let raw = download(file)?;
    if raw.len() as u64 > MAX_SAVE_PACKAGE_META_BYTES {
        return Err("云端存档清单过大".to_string());
    }
    let mut manifest: CloudSaveManifest = serde_json::from_slice(&raw)
        .map_err(|error| format!("云端存档清单损坏，已停止同步：{error}"))?;
    validate(&manifest, game_key)?;
    manifest.versions.retain(|version| {
        files
            .iter()
            .any(|file| !file.is_dir && file.path == version.package_path)
    });
    sort_versions(&mut manifest.versions);
    if !manifest
        .versions
        .iter()
        .any(|version| Some(&version.version_id) == manifest.latest_version_id.as_ref())
    {
        manifest.latest_version_id = manifest
            .versions
            .first()
            .map(|version| version.version_id.clone());
    }
    Ok(Some(manifest))
}

pub(super) fn validate(manifest: &CloudSaveManifest, game_key: &str) -> Result<(), String> {
    if manifest.format_version != MANIFEST_VERSION || manifest.game_key != game_key {
        return Err("云端存档清单格式或游戏标识不匹配".to_string());
    }
    let mut ids = HashSet::new();
    let mut paths = HashSet::new();
    for version in &manifest.versions {
        if !is_safe_path_segment(&version.version_id)
            || !ids.insert(&version.version_id)
            || !paths.insert(&version.package_path)
            || version.package_size == 0
            || version
                .package_sha256
                .as_ref()
                .is_some_and(|hash| !valid_hash(hash))
        {
            return Err("云端存档清单包含无效或重复的版本信息".to_string());
        }
        CloudSaveService::verified_package_path(game_key, &version.package_path)?;
    }
    if manifest
        .latest_version_id
        .as_ref()
        .is_some_and(|id| !ids.contains(id))
    {
        return Err("云端存档清单的当前版本不存在".to_string());
    }
    Ok(())
}

pub(super) fn sort_versions(versions: &mut [CloudSaveManifestVersion]) {
    versions.sort_by(|left, right| {
        compare_created_at(&right.created_at, &left.created_at)
            .then_with(|| right.version_id.cmp(&left.version_id))
    });
}

pub(super) fn commit_and_prune(
    manifest: &mut CloudSaveManifest,
    keep_limit: usize,
    save: impl Fn(&CloudSaveManifest) -> Result<(), String>,
    delete: impl Fn(&str) -> Result<(), String>,
) -> Result<(), String> {
    validate(manifest, &manifest.game_key)?;
    save(manifest)?;
    if keep_limit == 0 || manifest.versions.len() <= keep_limit {
        return Ok(());
    }
    let has_current = manifest.latest_version_id.is_some();
    let candidates = manifest
        .versions
        .iter()
        .filter(|version| Some(&version.version_id) != manifest.latest_version_id.as_ref())
        .skip(keep_limit.saturating_sub(usize::from(has_current)))
        .cloned()
        .collect::<Vec<_>>();
    let mut removed = HashSet::new();
    for version in candidates {
        if manifest.latest_version_id.as_ref() == Some(&version.version_id) {
            continue;
        }
        match delete(&version.package_path) {
            Ok(()) => {
                removed.insert(version.version_id);
            }
            Err(error) => {
                crate::logging::warn(format!("云端旧存档回收失败，保留条目以便重试：{error}"))
            }
        }
    }
    if !removed.is_empty() {
        manifest
            .versions
            .retain(|version| !removed.contains(&version.version_id));
        if let Err(error) = save(manifest) {
            crate::logging::warn(format!(
                "新存档已提交，回收后的清单更新失败，下次读取时核对：{error}"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn version(id: &str, created: &str) -> CloudSaveManifestVersion {
        CloudSaveManifestVersion {
            version_id: id.to_string(),
            created_at: created.to_string(),
            package_path: format!("/apps/GameSaver/saves/test/{id}.zip"),
            package_fs_id: 1,
            package_size: 10,
            package_sha256: Some("a".repeat(64)),
            file_count: 1,
            total_bytes: 1,
            device_name: None,
        }
    }

    fn sample() -> CloudSaveManifest {
        CloudSaveManifest {
            format_version: MANIFEST_VERSION,
            game_key: "test".into(),
            game_uid: "other-device".into(),
            updated_at: "100".into(),
            latest_version_id: Some("new".into()),
            versions: vec![version("new", "100"), version("old", "99")],
        }
    }

    fn files(manifest: &CloudSaveManifest) -> Vec<RemoteFile> {
        let mut files = manifest
            .versions
            .iter()
            .map(|version| RemoteFile {
                path: version.package_path.clone(),
                fs_id: 1,
                size: 10,
                md5: None,
                is_dir: false,
                server_mtime: None,
            })
            .collect::<Vec<_>>();
        files.push(RemoteFile {
            path: CloudSaveService::remote_manifest_path("test").unwrap(),
            fs_id: 2,
            size: 100,
            md5: None,
            is_dir: false,
            server_mtime: None,
        });
        files
    }

    #[test]
    fn only_absent_directory_or_empty_directory_is_first_sync() {
        for listing in [None, Some(Vec::new())] {
            assert!(
                read_manifest("test", listing, |_| panic!("no download expected"))
                    .unwrap()
                    .is_none()
            );
        }
        let mut listing = files(&sample());
        listing.pop();
        assert!(read_manifest("test", Some(listing), |_| panic!("missing manifest")).is_err());
    }

    #[test]
    fn manifest_download_and_decode_errors_are_not_absence() {
        assert!(read_manifest("test", Some(files(&sample())), |_| Err(
            "network interrupted".into()
        ))
        .unwrap_err()
        .contains("network"));
        assert!(read_manifest("test", Some(files(&sample())), |_| Ok(b"broken".to_vec())).is_err());
    }

    #[test]
    fn validates_identity_paths_duplicates_and_latest_reference() {
        let original = sample();
        assert!(validate(&original, "test").is_ok());
        for mutation in 0..6 {
            let mut manifest = original.clone();
            match mutation {
                0 => manifest.game_key = "other".into(),
                1 => manifest.format_version = 99,
                2 => {
                    manifest.versions[0].package_path = "/apps/GameSaver/saves/other/new.zip".into()
                }
                3 => manifest.versions.push(manifest.versions[0].clone()),
                4 => manifest.latest_version_id = Some("missing".into()),
                _ => manifest.versions[0].package_sha256 = Some("broken".into()),
            }
            assert!(validate(&manifest, "test").is_err(), "mutation {mutation}");
        }
    }

    #[test]
    fn failed_first_commit_does_not_delete_old_packages() {
        let deletes = Cell::new(0);
        let result = commit_and_prune(
            &mut sample(),
            1,
            |_| Err("commit failed".into()),
            |_| {
                deletes.set(deletes.get() + 1);
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(deletes.get(), 0);
    }

    #[test]
    fn invalid_manifest_never_reaches_commit_or_delete() {
        let mut manifest = sample();
        manifest.versions[1].package_path = "/apps/GameSaver/saves/another/game.zip".into();
        assert!(commit_and_prune(
            &mut manifest,
            1,
            |_| panic!("unsafe commit"),
            |_| panic!("unsafe delete")
        )
        .is_err());
    }

    #[test]
    fn retention_keeps_the_selected_current_version_even_if_it_is_older() {
        let mut manifest = sample();
        manifest.latest_version_id = Some("old".into());
        let deleted = RefCell::new(Vec::new());
        commit_and_prune(
            &mut manifest,
            1,
            |_| Ok(()),
            |path| {
                deleted.borrow_mut().push(path.to_string());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(manifest.versions.len(), 1);
        assert_eq!(manifest.versions[0].version_id, "old");
        assert_eq!(deleted.borrow()[0], "/apps/GameSaver/saves/test/new.zip");
    }

    #[test]
    fn new_manifest_is_committed_before_pruning_and_delete_failure_stays_visible() {
        let saves = RefCell::new(Vec::new());
        let mut manifest = sample();
        commit_and_prune(
            &mut manifest,
            1,
            |value| {
                saves.borrow_mut().push(value.versions.len());
                Ok(())
            },
            |_| {
                assert_eq!(*saves.borrow(), vec![2]);
                Err("delete failed".into())
            },
        )
        .unwrap();
        assert_eq!(manifest.versions.len(), 2);
    }

    #[test]
    fn failed_cleanup_commit_is_reconciled_on_next_read() {
        let persisted = RefCell::new(sample());
        let saves = Cell::new(0);
        let mut listing = files(&sample());
        listing.retain(|file| !file.path.ends_with("/old.zip"));
        commit_and_prune(
            &mut sample(),
            1,
            |value| {
                saves.set(saves.get() + 1);
                if saves.get() == 2 {
                    return Err("cleanup commit failed".into());
                }
                *persisted.borrow_mut() = value.clone();
                Ok(())
            },
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(persisted.borrow().versions.len(), 2);
        let raw = serde_json::to_vec(&*persisted.borrow()).unwrap();
        let reloaded = read_manifest("test", Some(listing), |_| Ok(raw))
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.versions.len(), 1);
        assert_eq!(reloaded.latest_version_id.as_deref(), Some("new"));
    }

    #[test]
    fn read_sorts_numeric_timestamps_and_preserves_current_pointer() {
        let mut manifest = sample();
        manifest.versions.reverse();
        manifest.latest_version_id = Some("old".into());
        let raw = serde_json::to_vec(&manifest).unwrap();
        let read = read_manifest("test", Some(files(&manifest)), |_| Ok(raw))
            .unwrap()
            .unwrap();
        assert_eq!(read.versions[0].version_id, "new");
        assert_eq!(read.latest_version_id.as_deref(), Some("old"));
    }
}
