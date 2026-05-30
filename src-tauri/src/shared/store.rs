use serde::{Deserialize, Serialize};

use super::{config::ExecutionConfig, learning::LearningSession, rules::GameSaveRule};

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PersistedStore {
    pub(crate) sessions: Vec<LearningSession>,
    pub(crate) rules: Vec<GameSaveRule>,
    #[serde(default)]
    pub(crate) launcher_sessions: Vec<super::launcher::LauncherSession>,
    #[serde(default)]
    pub(crate) execution_config: ExecutionConfig,
}
