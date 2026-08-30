use super::{Game, GameBodyVersion, SaveProfile, SaveVersion};
use serde::{Deserialize, Serialize};

pub const CURRENT_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppStore {
    pub schema_version: u32,
    #[serde(default)]
    pub games: Vec<Game>,
    #[serde(default)]
    pub save_profiles: Vec<SaveProfile>,
    #[serde(default)]
    pub save_versions: Vec<SaveVersion>,
    #[serde(default)]
    pub body_versions: Vec<GameBodyVersion>,
}

impl Default for AppStore {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            games: Vec::new(),
            save_profiles: Vec::new(),
            save_versions: Vec::new(),
            body_versions: Vec::new(),
        }
    }
}

impl AppStore {
    pub fn normalize(&mut self) {
        self.schema_version = CURRENT_SCHEMA_VERSION;
        for game in &mut self.games {
            game.game_key = if game.game_key.trim().is_empty() {
                Game::derive_game_key(&game.display_name)
            } else {
                game.game_key
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_lowercase()
            };
        }
        self.games.retain(|game| {
            !game.game_uid.trim().is_empty()
                && !game.game_key.trim().is_empty()
                && !game.display_name.trim().is_empty()
                && !game.managed_path.trim().is_empty()
                && !game.launch.executable_relative_path.trim().is_empty()
        });
        self.games.sort_by(|left, right| {
            left.display_name
                .to_lowercase()
                .cmp(&right.display_name.to_lowercase())
        });
        self.save_profiles.retain(|profile| {
            !profile.profile_id.trim().is_empty()
                && !profile.game_uid.trim().is_empty()
                && !profile.scopes.is_empty()
        });
        self.save_versions.retain(|version| {
            !version.version_id.trim().is_empty()
                && !version.game_uid.trim().is_empty()
                && !version.files.is_empty()
                && self
                    .games
                    .iter()
                    .any(|game| game.game_uid == version.game_uid)
        });
        self.body_versions.retain(|version| {
            !version.version_id.trim().is_empty()
                && !version.game_uid.trim().is_empty()
                && (!version.archive_path.trim().is_empty()
                    || version
                        .package_path
                        .as_deref()
                        .is_some_and(|path| !path.trim().is_empty()))
                && version.file_count > 0
                && self
                    .games
                    .iter()
                    .any(|game| game.game_uid == version.game_uid)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::AppStore;

    #[test]
    fn normalizing_schema_one_data_derives_game_key_before_filtering() {
        let mut store: AppStore = serde_json::from_value(serde_json::json!({
            "schemaVersion": 1,
            "games": [{
                "gameUid": "local-1",
                "displayName": "  Monster   Black Market ",
                "managedPath": "E:/GameSaverGames/games/local-1",
                "lifecycle": "active",
                "health": "ready",
                "cloudStatus": "local_only",
                "launch": { "executableRelativePath": "game.exe" }
            }]
        }))
        .expect("schema one data should deserialize");

        store.normalize();

        assert_eq!(store.schema_version, 2);
        assert_eq!(store.games.len(), 1);
        assert_eq!(store.games[0].game_key, "monster black market");
    }
}
