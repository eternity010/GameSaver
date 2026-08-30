use crate::domain::{ActiveLearningSession, AppStore, AppTask, GameRuntime};
use std::{collections::{HashMap, HashSet}, path::PathBuf, sync::Mutex};

pub struct AppState {
    pub store: Mutex<AppStore>,
    pub tasks: Mutex<HashMap<String, AppTask>>,
    pub tasks_path: PathBuf,
    pub learning_sessions: Mutex<HashMap<String, ActiveLearningSession>>,
    pub running_games: Mutex<HashMap<String, GameRuntime>>,
    pub save_operations: Mutex<HashSet<String>>,
}

impl AppState {
    pub fn new(store: AppStore, tasks: HashMap<String, AppTask>, tasks_path: PathBuf) -> Self {
        Self {
            store: Mutex::new(store),
            tasks: Mutex::new(tasks),
            tasks_path,
            learning_sessions: Mutex::new(HashMap::new()),
            running_games: Mutex::new(HashMap::new()),
            save_operations: Mutex::new(HashSet::new()),
        }
    }
}
