use crate::domain::{game::GameLifecycle, AppStore, Game, GameHealth, SaveProfile, SaveScope};
use std::path::{Component, Path};

pub struct GameLibraryService;

impl GameLibraryService {
    pub fn register_pending(store: &mut AppStore, game: Game) -> Result<(), String> {
        if store.games.iter().any(|item| item.game_uid == game.game_uid || item.managed_path == game.managed_path) {
            return Err("受管游戏已登记".to_string());
        }
        store.games.push(game);
        Ok(())
    }

    pub fn list(store: &AppStore) -> Vec<Game> {
        store
            .games
            .iter()
            .filter(|game| matches!(game.lifecycle, GameLifecycle::Active))
            .map(|game| {
                let mut game = game.clone();
                game.health = Self::derive_health(store, &game);
                game
            })
            .collect()
    }

    pub fn find(store: &AppStore, game_uid: &str) -> Option<Game> {
        store.games.iter().find(|game| game.game_uid == game_uid).cloned()
    }

    fn derive_health(store: &AppStore, game: &Game) -> GameHealth {
        let root = Path::new(&game.managed_path);
        let executable = root.join(&game.launch.executable_relative_path);
        if !root.is_dir() || !executable.is_file() {
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
        profile.scopes.iter().filter(|scope| Self::scope_is_accessible(scope)).count()
    }

    fn scope_is_accessible(scope: &SaveScope) -> bool {
        let root = Path::new(&scope.root_path);
        if !root.is_dir() {
            return false;
        }
        let valid_relative = |value: &str| {
            let path = Path::new(value);
            !path.is_absolute() && !path.components().any(|component| matches!(component, Component::ParentDir))
        };
        scope.confirmed_files.iter().all(|value| valid_relative(value) && root.join(value).is_file())
            && scope.include_directories.iter().all(|value| valid_relative(value) && root.join(value).is_dir())
    }
}
