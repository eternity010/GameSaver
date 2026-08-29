use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GameLifecycle {
    PendingSetup,
    Active,
    NeedsRepair,
    Removing,
}

impl Default for GameLifecycle {
    fn default() -> Self {
        Self::PendingSetup
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GameHealth {
    NeedsSetup,
    Ready,
    NeedsAttention,
    Broken,
}

impl Default for GameHealth {
    fn default() -> Self {
        Self::NeedsSetup
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CloudStatus {
    Disabled,
    LocalOnly,
    Syncing,
    Synced,
    Failed,
}

impl Default for CloudStatus {
    fn default() -> Self {
        Self::Disabled
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LaunchConfig {
    pub executable_relative_path: String,
    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default)]
    pub working_directory_relative_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Game {
    pub game_uid: String,
    pub display_name: String,
    pub managed_path: String,
    pub lifecycle: GameLifecycle,
    pub health: GameHealth,
    pub cloud_status: CloudStatus,
    pub launch: LaunchConfig,
    #[serde(default)]
    pub save_profile_id: Option<String>,
    #[serde(default)]
    pub last_played_at: Option<String>,
    #[serde(default)]
    pub latest_save_version_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameRuntime {
    pub game_uid: String,
    pub status: GameRuntimeStatus,
    pub pid: Option<u32>,
    pub started_at: Option<String>,
    pub task_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GameRuntimeStatus {
    Launching,
    Running,
    Saving,
}

impl Game {
    pub fn new_pending(
        display_name: impl Into<String>,
        managed_path: impl Into<String>,
        executable_relative_path: impl Into<String>,
    ) -> Self {
        Self {
            game_uid: Uuid::new_v4().to_string(),
            display_name: display_name.into(),
            managed_path: managed_path.into(),
            lifecycle: GameLifecycle::PendingSetup,
            health: GameHealth::NeedsSetup,
            cloud_status: CloudStatus::Disabled,
            launch: LaunchConfig {
                executable_relative_path: executable_relative_path.into(),
                ..LaunchConfig::default()
            },
            save_profile_id: None,
            last_played_at: None,
            latest_save_version_id: None,
        }
    }

    #[allow(dead_code)]
    pub fn activate(&mut self, save_profile_id: impl Into<String>) {
        self.save_profile_id = Some(save_profile_id.into());
        self.lifecycle = GameLifecycle::Active;
        self.health = GameHealth::Ready;
    }
}

#[cfg(test)]
mod tests {
    use super::{Game, GameHealth, GameLifecycle};

    #[test]
    fn new_game_requires_setup_before_activation() {
        let mut game = Game::new_pending("Test Game", "games/test", "game.exe");
        assert_eq!(game.lifecycle, GameLifecycle::PendingSetup);
        assert_eq!(game.health, GameHealth::NeedsSetup);

        game.activate("profile-1");
        assert_eq!(game.lifecycle, GameLifecycle::Active);
        assert_eq!(game.health, GameHealth::Ready);
        assert_eq!(game.save_profile_id.as_deref(), Some("profile-1"));
    }
}
