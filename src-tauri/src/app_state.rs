use crate::domain::{ActiveLearningSession, AppStore, GameRuntime};
use crate::domain::AppTask;
use std::{collections::HashMap, sync::Mutex};

pub struct AppState {
    pub store: Mutex<AppStore>,
    pub tasks: Mutex<HashMap<String, AppTask>>,
    pub learning_sessions: Mutex<HashMap<String, ActiveLearningSession>>,
    pub running_games: Mutex<HashMap<String, GameRuntime>>,
}

impl AppState {
    pub fn new(store: AppStore) -> Self {
        Self {
            store: Mutex::new(store),
            tasks: Mutex::new(HashMap::new()),
            learning_sessions: Mutex::new(HashMap::new()),
            running_games: Mutex::new(HashMap::new()),
        }
    }
}
