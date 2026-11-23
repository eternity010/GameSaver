use super::{validate, CloudAccountProfile, CloudSaveScope};
use crate::domain::{
    known_folders::{documents_root, public_documents_root},
    game::CloudStatus, AppStore, Game, GameHealth, GameLifecycle, SaveProfile, SaveRootType,
    SaveScope,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub(crate) fn merge_profile(
    store: &mut AppStore,
    games_root: &Path,
    profile: CloudAccountProfile,
) -> Result<usize, String> {
    validate(&profile)?;
    let mut candidate = store.clone();
    let mut imported_games = 0;
    let mut cloud_uid_to_local_uid = HashMap::new();
    for cloud_game in &profile.games {
        let game_key = Game::derive_game_key(&cloud_game.game_key);
        let matching = candidate
            .games
            .iter()
            .enumerate()
            .filter(|(_, game)| Game::derive_game_key(&game.game_key) == game_key)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if matching.len() > 1 {
            return Err(format!("本地存在重复的游戏标识，无法恢复：{game_key}"));
        }
        let local_uid = if let Some(index) = matching.first().copied() {
            let local = &mut candidate.games[index];
            local.display_name = cloud_game.display_name.clone();
            local.launch = cloud_game.launch.clone();
            local.last_played_at = cloud_game.last_played_at.clone();
            if cloud_game.added_at.is_some() && local.added_at.is_none() {
                local.added_at = cloud_game.added_at.clone();
            }
            local.cloud_status = CloudStatus::Synced;
            local.game_uid.clone()
        } else {
            let local_uid = Uuid::new_v4().to_string();
            let mut game = Game::new_pending(
                &cloud_game.display_name,
                games_root.join(&local_uid).to_string_lossy(),
                &cloud_game.launch.executable_relative_path,
            );
            game.game_uid = local_uid.clone();
            game.game_key = game_key;
            game.launch = cloud_game.launch.clone();
            game.last_played_at = cloud_game.last_played_at.clone();
            if cloud_game.added_at.is_some() {
                game.added_at = cloud_game.added_at.clone();
            }
            game.lifecycle = GameLifecycle::NeedsRepair;
            game.health = GameHealth::Broken;
            game.cloud_status = CloudStatus::Synced;
            candidate.games.push(game);
            imported_games += 1;
            local_uid
        };
        cloud_uid_to_local_uid.insert(cloud_game.game_uid.clone(), local_uid);
    }
    for cloud_profile in &profile.save_profiles {
        let local_uid = cloud_uid_to_local_uid
            .get(&cloud_profile.game_uid)
            .ok_or_else(|| "云端存档配置对应的游戏不存在".to_string())?;
        let game = candidate
            .games
            .iter()
            .find(|game| &game.game_uid == local_uid)
            .ok_or_else(|| "云端存档配置对应的本地游戏不存在".to_string())?;
        let existing = candidate
            .save_profiles
            .iter()
            .filter(|item| &item.game_uid == local_uid)
            .find(|item| game.save_profile_id.as_deref() == Some(item.profile_id.as_str()))
            .or_else(|| {
                candidate
                    .save_profiles
                    .iter()
                    .find(|item| &item.game_uid == local_uid)
            })
            .cloned();
        let profile_id = existing
            .as_ref()
            .map(|item| item.profile_id.clone())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        if candidate
            .save_profiles
            .iter()
            .any(|item| item.profile_id == profile_id && &item.game_uid != local_uid)
        {
            return Err("本地存档配置标识归属冲突，请先修复本地记录".to_string());
        }
        let scopes = cloud_profile
            .scopes
            .iter()
            .enumerate()
            .map(|(index, scope)| -> Result<SaveScope, String> {
                let fallback = existing
                    .as_ref()
                    .and_then(|item| item.scopes.get(index))
                    .filter(|item| item.root_type == scope.root_type)
                    .map(|item| item.root_path.as_str());
                let mut resolved = resolve_scope(game, scope, fallback)?;
                if let Some(existing) = existing.as_ref() {
                    use crate::domain::path_utils::normalize_path;
                    let resolved_key = normalize_path(Path::new(&resolved.root_path));
                    if let Some(binding) = existing.root_bindings.iter().find(|binding| {
                        binding.root_type == scope.root_type
                            && normalize_path(Path::new(&binding.previous_path)) == resolved_key
                    }) {
                        let current_key = normalize_path(Path::new(&binding.current_path));
                        if let Some(current) = existing.scopes.iter().find(|current| {
                            current.root_type == scope.root_type
                                && normalize_path(Path::new(&current.root_path)) == current_key
                        }) {
                            resolved.root_path.clone_from(&current.root_path);
                        }
                    }
                }
                Ok(resolved)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let save_profile = SaveProfile {
            profile_id: profile_id.clone(),
            game_uid: local_uid.clone(),
            executable_hash: cloud_profile.executable_hash.clone(),
            scopes,
            root_bindings: existing
                .as_ref()
                .map(|item| item.root_bindings.clone())
                .unwrap_or_default(),
            detection_evidence: cloud_profile.detection_evidence.clone(),
            confidence: cloud_profile.confidence,
            enabled: true,
            keep_versions: existing
                .as_ref()
                .map(|item| item.keep_versions)
                .unwrap_or(5),
            created_at: existing
                .as_ref()
                .map(|item| item.created_at.clone())
                .unwrap_or_else(|| cloud_profile.updated_at.clone()),
            updated_at: cloud_profile.updated_at.clone(),
        };
        candidate
            .save_profiles
            .retain(|item| &item.game_uid != local_uid);
        candidate.save_profiles.push(save_profile);
        if let Some(local) = candidate
            .games
            .iter_mut()
            .find(|item| &item.game_uid == local_uid)
        {
            local.save_profile_id = Some(profile_id);
            local.save_protection_skipped = false;
        }
    }
    candidate.normalize();
    *store = candidate;
    Ok(imported_games)
}

pub(crate) fn resolve_scope(
    game: &Game,
    scope: &CloudSaveScope,
    fallback: Option<&str>,
) -> Result<SaveScope, String> {
    resolve_scope_with_documents(game, scope, fallback, |public| {
        if public { public_documents_root() } else { documents_root() }
    })
}

fn resolve_scope_with_documents(
    game: &Game,
    scope: &CloudSaveScope,
    fallback: Option<&str>,
    documents: impl FnOnce(bool) -> Option<PathBuf>,
) -> Result<SaveScope, String> {
    let base_path = match scope.root_type {
        SaveRootType::ManagedGame => Some(PathBuf::from(&game.managed_path)),
        SaveRootType::AppData => std::env::var_os("APPDATA").map(PathBuf::from),
        SaveRootType::LocalAppData => std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
        SaveRootType::LocalLow => std::env::var_os("USERPROFILE")
            .map(|path| PathBuf::from(path).join("AppData").join("LocalLow")),
        SaveRootType::Documents => documents(scope.public_documents),
        SaveRootType::SavedGames => {
            std::env::var_os("USERPROFILE").map(|path| PathBuf::from(path).join("Saved Games"))
        }
        SaveRootType::UserProfile => std::env::var_os("USERPROFILE").map(PathBuf::from),
        SaveRootType::ProgramData => std::env::var_os("PROGRAMDATA").map(PathBuf::from),
        SaveRootType::Custom => scope.custom_root_path.as_ref().map(PathBuf::from),
    };
    let root_path = if let Some(base) = base_path {
        match scope
            .sub_path
            .as_deref()
            .filter(|sub| !sub.trim().is_empty())
        {
            Some(sub) => base
                .join(sub.replace('/', "\\"))
                .to_string_lossy()
                .into_owned(),
            None => base.to_string_lossy().into_owned(),
        }
    } else if let Some(path) = fallback {
        path.to_string()
    } else {
        return Err(format!("无法解析云端存档范围：{:?}", scope.root_type));
    };
    Ok(SaveScope {
        root_type: scope.root_type,
        root_path,
        confirmed_files: scope.confirmed_files.clone(),
        include_directories: scope.include_directories.clone(),
        exclude_exact: scope.exclude_exact.clone(),
        exclude_patterns: scope.exclude_patterns.clone(),
        exclude_directories: scope.exclude_directories.clone(),
        unknown_file_policy: scope.unknown_file_policy,
        max_file_bytes: scope.max_file_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Game, SaveRootType, UnknownFilePolicy};
    use crate::services::cloud_account_service::CloudSaveScope;

    fn game_with_profile(name: &str, profile_id: &str) -> (Game, SaveProfile) {
        let mut game = Game::new_pending(name, format!(r"E:\Games\{name}"), "game.exe");
        game.activate(profile_id);
        let mut profile = SaveProfile::new(
            game.game_uid.clone(),
            "hash".into(),
            vec![SaveScope::new_manual(r"C:\Review\Save".into())],
            100,
            "1".into(),
        );
        profile.profile_id = profile_id.into();
        (game, profile)
    }

    fn cloud_profile(game: Game, profile: SaveProfile) -> CloudAccountProfile {
        let store = AppStore {
            games: vec![game],
            save_profiles: vec![profile],
            ..AppStore::default()
        };
        super::super::CloudAccountService::build(&store, false)
    }

    #[test]
    fn profile_collision_preserves_unrelated_game_and_local_profile_id() {
        let (target, target_profile) = game_with_profile("target", "local-profile");
        let (unrelated, unrelated_profile) = game_with_profile("unrelated", "remote-profile");
        let (remote, remote_profile) = game_with_profile("target", "remote-profile");
        let mut store = AppStore {
            games: vec![target.clone(), unrelated.clone()],
            save_profiles: vec![unrelated_profile, target_profile],
            ..AppStore::default()
        };
        merge_profile(
            &mut store,
            Path::new(r"E:\Games"),
            cloud_profile(remote, remote_profile),
        )
        .unwrap();
        assert!(store
            .save_profiles
            .iter()
            .any(|profile| profile.game_uid == unrelated.game_uid
                && profile.profile_id == "remote-profile"));
        assert!(store
            .save_profiles
            .iter()
            .any(|profile| profile.game_uid == target.game_uid
                && profile.profile_id == "local-profile"));
        assert_eq!(
            store
                .games
                .iter()
                .find(|game| game.game_uid == target.game_uid)
                .unwrap()
                .save_profile_id
                .as_deref(),
            Some("local-profile")
        );
    }

    #[test]
    fn new_profile_gets_a_local_id_and_repeated_import_is_idempotent() {
        let (remote, remote_profile) = game_with_profile("target", "remote-profile");
        let profile = cloud_profile(remote, remote_profile);
        let mut store = AppStore::default();
        assert_eq!(
            merge_profile(&mut store, Path::new(r"E:\Games"), profile.clone()).unwrap(),
            1
        );
        let id = store.save_profiles[0].profile_id.clone();
        assert_ne!(id, "remote-profile");
        assert_eq!(store.games[0].save_profile_id.as_deref(), Some(id.as_str()));
        assert_eq!(
            merge_profile(&mut store, Path::new(r"E:\Games"), profile).unwrap(),
            0
        );
        assert_eq!(store.save_profiles.len(), 1);
        assert_eq!(store.save_profiles[0].profile_id, id);
    }

    #[test]
    fn game_key_wins_over_source_machine_uid_without_changing_unrelated_game() {
        let (wrong, wrong_profile) = game_with_profile("unrelated", "wrong-profile");
        let (right, right_profile) = game_with_profile("target", "right-profile");
        let (mut remote, mut remote_profile) = game_with_profile("target", "remote-profile");
        remote.game_uid = wrong.game_uid.clone();
        remote_profile.game_uid = remote.game_uid.clone();
        remote.display_name = "Cloud Target".into();
        let mut store = AppStore {
            games: vec![wrong.clone(), right.clone()],
            save_profiles: vec![wrong_profile, right_profile],
            ..AppStore::default()
        };
        merge_profile(
            &mut store,
            Path::new(r"E:\Games"),
            cloud_profile(remote, remote_profile),
        )
        .unwrap();
        assert_eq!(
            store
                .games
                .iter()
                .find(|game| game.game_uid == wrong.game_uid)
                .unwrap(),
            &wrong
        );
        assert_eq!(
            store
                .games
                .iter()
                .find(|game| game.game_uid == right.game_uid)
                .unwrap()
                .display_name,
            "Cloud Target"
        );
    }

    #[test]
    fn duplicate_normalized_keys_fail_without_partial_changes() {
        let (remote, remote_profile) = game_with_profile("target", "remote-profile");
        let mut profile = cloud_profile(remote, remote_profile);
        let mut duplicate = profile.games[0].clone();
        duplicate.game_uid = "second-source".into();
        duplicate.game_key = " TARGET ".into();
        profile.games.push(duplicate);
        let mut store = AppStore::default();
        assert!(merge_profile(&mut store, Path::new(r"E:\Games"), profile).is_err());
        assert!(store.games.is_empty());
        assert!(store.save_profiles.is_empty());
    }

    #[test]
    fn public_documents_round_trip_keeps_public_location() {
        let root = public_documents_root().expect("Windows public documents");
        let (game, mut profile) = game_with_profile("target", "profile");
        profile.scopes[0].root_type = SaveRootType::Documents;
        profile.scopes[0].root_path = root
            .join("Target")
            .join("Save")
            .to_string_lossy()
            .into_owned();
        let cloud = cloud_profile(game.clone(), profile);
        let scope = &cloud.save_profiles[0].scopes[0];
        assert!(scope.public_documents);
        let resolved = resolve_scope(&game, scope, None).unwrap();
        assert_eq!(
            PathBuf::from(resolved.root_path),
            root.join("Target").join("Save")
        );
    }

    #[test]
    fn public_documents_flag_is_rejected_for_other_root_types() {
        let (game, profile) = game_with_profile("target", "profile");
        let mut cloud = cloud_profile(game, profile);
        cloud.save_profiles[0].scopes[0].public_documents = true;
        assert!(validate(&cloud).is_err());
    }

    #[test]
    fn resolve_scope_uses_redirected_documents_on_the_target_machine() {
        let (game, _) = game_with_profile("Test", "profile");
        let scope = CloudSaveScope {
            root_type: SaveRootType::Documents,
            public_documents: false,
            custom_root_path: None,
            sub_path: Some("Studio/Game".to_string()),
            confirmed_files: vec!["slot.sav".to_string()],
            include_directories: Vec::new(),
            exclude_exact: Vec::new(),
            exclude_patterns: Vec::new(),
            exclude_directories: Vec::new(),
            unknown_file_policy: UnknownFilePolicy::Protect,
            max_file_bytes: None,
        };
        let documents = PathBuf::from(r"F:\Sync\文档");
        let resolved = resolve_scope_with_documents(&game, &scope, None, |public| {
            assert!(!public);
            Some(documents.clone())
        }).unwrap();
        assert_eq!(PathBuf::from(resolved.root_path), documents.join("Studio").join("Game"));
    }

    #[test]
    fn resolve_scope_joins_sub_path_for_locallow() {
        let game = Game::new_pending("Game1", r"E:\Games\Game1", "game.exe");
        let cloud_scope = CloudSaveScope {
            root_type: SaveRootType::LocalLow,
            public_documents: false,
            custom_root_path: None,
            sub_path: Some("miHoYo/GenshinImpact".to_string()),
            confirmed_files: vec!["save.dat".to_string()],
            include_directories: Vec::new(),
            exclude_exact: Vec::new(),
            exclude_patterns: Vec::new(),
            exclude_directories: Vec::new(),
            unknown_file_policy: UnknownFilePolicy::Protect,
            max_file_bytes: None,
        };
        let resolved = resolve_scope(&game, &cloud_scope, None).expect("should resolve");
        assert!(resolved
            .root_path
            .ends_with(r"AppData\LocalLow\miHoYo\GenshinImpact"));
    }

    #[test]
    fn resolve_scope_joins_sub_path_for_managed_game() {
        let game = Game::new_pending("Game1", r"E:\Games\Game1", "game.exe");
        let cloud_scope = CloudSaveScope {
            root_type: SaveRootType::ManagedGame,
            public_documents: false,
            custom_root_path: None,
            sub_path: Some("SaveData/Slot1".to_string()),
            confirmed_files: vec!["save.dat".to_string()],
            include_directories: Vec::new(),
            exclude_exact: Vec::new(),
            exclude_patterns: Vec::new(),
            exclude_directories: Vec::new(),
            unknown_file_policy: UnknownFilePolicy::Protect,
            max_file_bytes: None,
        };
        let resolved = resolve_scope(&game, &cloud_scope, None).expect("should resolve");
        assert_eq!(resolved.root_path, r"E:\Games\Game1\SaveData\Slot1");
    }

    #[test]
    fn resolve_scope_uses_current_standard_root_when_sub_path_is_none() {
        let game = Game::new_pending("Game1", r"E:\Games\Game1", "game.exe");
        let cloud_scope = CloudSaveScope {
            root_type: SaveRootType::LocalLow,
            public_documents: false,
            custom_root_path: None,
            sub_path: None,
            confirmed_files: vec!["save.dat".to_string()],
            include_directories: Vec::new(),
            exclude_exact: Vec::new(),
            exclude_patterns: Vec::new(),
            exclude_directories: Vec::new(),
            unknown_file_policy: UnknownFilePolicy::Protect,
            max_file_bytes: None,
        };
        let fallback = r"C:\Users\Bob\AppData\LocalLow\CustomFolder";
        let resolved = resolve_scope(&game, &cloud_scope, Some(fallback)).expect("should resolve");
        let expected = std::env::var_os("USERPROFILE")
            .map(|path| PathBuf::from(path).join("AppData").join("LocalLow"))
            .expect("USERPROFILE should be available during test");
        assert_eq!(PathBuf::from(&resolved.root_path), expected);
        assert_ne!(resolved.root_path, fallback);
    }
}
