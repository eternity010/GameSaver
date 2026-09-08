use crate::domain::{game::GameLifecycle, AppStore, Game, GameHealth, SaveProfile, SaveScope};
use std::path::{Component, Path};

pub struct GameLibraryService;

impl GameLibraryService {
    pub fn register_pending(store: &mut AppStore, game: Game) -> Result<(), String> {
        if store
            .games
            .iter()
            .any(|item| item.game_uid == game.game_uid || item.managed_path == game.managed_path)
        {
            return Err("受管游戏已登记".to_string());
        }
        store.games.push(game);
        Ok(())
    }

    pub fn list(store: &AppStore) -> Vec<Game> {
        store
            .games
            .iter()
            .filter(|game| {
                matches!(
                    game.lifecycle,
                    GameLifecycle::PendingSetup
                        | GameLifecycle::Active
                        | GameLifecycle::NeedsRepair
                )
            })
            .map(|game| {
                let mut game = game.clone();
                game.health = Self::derive_health(store, &game);
                game
            })
            .collect()
    }

    pub fn is_installed(game: &Game) -> bool {
        let root = Path::new(&game.managed_path);
        root.is_dir() && root.join(&game.launch.executable_relative_path).is_file()
    }

    pub fn find(store: &AppStore, game_uid: &str) -> Option<Game> {
        store
            .games
            .iter()
            .find(|game| game.game_uid == game_uid)
            .cloned()
    }

    fn derive_health(store: &AppStore, game: &Game) -> GameHealth {
        if !Self::is_installed(game) {
            return GameHealth::Broken;
        }
        let profile = store.save_profiles.iter().find(|profile| {
            profile.game_uid == game.game_uid
                && game.save_profile_id.as_deref() == Some(profile.profile_id.as_str())
                && profile.enabled
        });
        if profile.is_some_and(|profile| Self::valid_scope_count(profile) > 0) {
            GameHealth::Ready
        } else {
            GameHealth::NeedsAttention
        }
    }

    pub fn valid_scope_count(profile: &SaveProfile) -> usize {
        profile
            .scopes
            .iter()
            .filter(|scope| Self::scope_is_accessible(scope))
            .count()
    }

    fn scope_is_accessible(scope: &SaveScope) -> bool {
        let root = Path::new(&scope.root_path);
        if !root.is_dir() {
            return false;
        }
        let valid_relative = |value: &str| {
            let path = Path::new(value);
            !path.is_absolute()
                && !path
                    .components()
                    .any(|component| matches!(component, Component::ParentDir))
        };
        scope
            .confirmed_files
            .iter()
            .all(|value| valid_relative(value))
            && scope
                .include_directories
                .iter()
                .all(|value| valid_relative(value) && root.join(value).is_dir())
    }
}

#[cfg(test)]
mod tests {
    use super::GameLibraryService;
    use crate::domain::{game::GameLifecycle, AppStore, Game};
    use std::{fs, path::PathBuf};
    use uuid::Uuid;

    fn test_game(root: PathBuf) -> Game {
        let mut game = Game::new_pending("Test Game", root.to_string_lossy(), "game.exe");
        game.activate("profile-1");
        game
    }

    fn test_root() -> PathBuf {
        std::env::temp_dir().join(format!("gamesaver-library-test-{}", Uuid::new_v4()))
    }

    #[test]
    fn library_keeps_broken_game_when_managed_body_is_missing() {
        let root = test_root();
        let mut store = AppStore::default();
        store.games.push(test_game(root.clone()));

        assert!(!GameLibraryService::is_installed(&store.games[0]));
        let listed = GameLibraryService::list(&store);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].health, crate::domain::GameHealth::Broken);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn library_keeps_broken_game_when_executable_is_missing() {
        let root = test_root();
        fs::create_dir_all(&root).expect("create managed body");
        let mut store = AppStore::default();
        store.games.push(test_game(root.clone()));

        assert!(!GameLibraryService::is_installed(&store.games[0]));
        let listed = GameLibraryService::list(&store);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].health, crate::domain::GameHealth::Broken);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn library_includes_game_when_body_and_executable_exist() {
        let root = test_root();
        fs::create_dir_all(&root).expect("create managed body");
        fs::write(root.join("game.exe"), b"test executable").expect("create executable");
        let mut store = AppStore::default();
        store.games.push(test_game(root.clone()));

        assert!(GameLibraryService::is_installed(&store.games[0]));
        assert_eq!(GameLibraryService::list(&store).len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn library_includes_pending_game_when_body_and_executable_exist() {
        let root = test_root();
        fs::create_dir_all(&root).expect("create managed body");
        fs::write(root.join("game.exe"), b"test executable").expect("create executable");
        let mut store = AppStore::default();
        let game = Game::new_pending("Pending Game", root.to_string_lossy(), "game.exe");
        store.games.push(game);

        let listed = GameLibraryService::list(&store);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].lifecycle, GameLifecycle::PendingSetup);
        assert_eq!(listed[0].health, crate::domain::GameHealth::NeedsAttention);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn scope_stays_valid_when_confirmed_save_file_is_temporarily_missing() {
        let root = test_root();
        fs::create_dir_all(&root).expect("create save scope");
        let mut store = AppStore::default();
        let mut game = test_game(root.clone());
        game.save_profile_id = Some("profile-1".to_string());
        store.games.push(game);
        store.save_profiles.push(crate::domain::SaveProfile {
            profile_id: "profile-1".to_string(),
            game_uid: store.games[0].game_uid.clone(),
            executable_hash: "hash".to_string(),
            scopes: vec![crate::domain::SaveScope {
                root_type: crate::domain::SaveRootType::Custom,
                root_path: root.to_string_lossy().to_string(),
                confirmed_files: vec!["future-save.json".to_string()],
                include_directories: vec![".".to_string()],
                exclude_exact: Vec::new(),
                exclude_patterns: Vec::new(),
                exclude_directories: Vec::new(),
                unknown_file_policy: crate::domain::UnknownFilePolicy::Protect,
                max_file_bytes: Some(10 * 1024 * 1024),
            }],
            detection_evidence: Vec::new(),
            confidence: 0,
            enabled: true,
            keep_versions: 5,
            created_at: "0".to_string(),
            updated_at: "0".to_string(),
        });

        assert_eq!(
            GameLibraryService::valid_scope_count(&store.save_profiles[0]),
            1
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn scope_is_invalid_when_included_directory_is_missing() {
        let root = test_root();
        fs::create_dir_all(&root).expect("create save scope");
        let mut profile = crate::domain::SaveProfile::new(
            "game-1".to_string(),
            "hash".to_string(),
            vec![crate::domain::SaveScope {
                root_type: crate::domain::SaveRootType::Custom,
                root_path: root.to_string_lossy().to_string(),
                confirmed_files: Vec::new(),
                include_directories: vec!["missing".to_string()],
                exclude_exact: Vec::new(),
                exclude_patterns: Vec::new(),
                exclude_directories: Vec::new(),
                unknown_file_policy: crate::domain::UnknownFilePolicy::Protect,
                max_file_bytes: Some(10 * 1024 * 1024),
            }],
            0,
            "0".to_string(),
        );

        assert_eq!(GameLibraryService::valid_scope_count(&profile), 0);
        profile.scopes[0].include_directories = vec![".".to_string()];
        assert_eq!(GameLibraryService::valid_scope_count(&profile), 1);
        let _ = fs::remove_dir_all(root);
    }
}
