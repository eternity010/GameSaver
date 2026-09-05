use crate::domain::{ActiveLearningSession, AppStore, AppTask, GameRuntime};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Mutex,
};

pub struct AppState {
    pub store: Mutex<AppStore>,
    pub library_root: Mutex<PathBuf>,
    pub tasks: Mutex<HashMap<String, AppTask>>,
    pub tasks_path: PathBuf,
    pub learning_sessions: Mutex<HashMap<String, ActiveLearningSession>>,
    pub running_games: Mutex<HashMap<String, GameRuntime>>,
    pub save_operations: Mutex<HashSet<String>>,
    pub library_migration: Mutex<bool>,
    pub cloud_account_sync: Mutex<bool>,
}

impl AppState {
    pub fn new(
        store: AppStore,
        library_root: PathBuf,
        tasks: HashMap<String, AppTask>,
        tasks_path: PathBuf,
    ) -> Self {
        Self {
            store: Mutex::new(store),
            library_root: Mutex::new(library_root),
            tasks: Mutex::new(tasks),
            tasks_path,
            learning_sessions: Mutex::new(HashMap::new()),
            running_games: Mutex::new(HashMap::new()),
            save_operations: Mutex::new(HashSet::new()),
            library_migration: Mutex::new(false),
            cloud_account_sync: Mutex::new(false),
        }
    }

    pub fn library_root_path(&self) -> Result<PathBuf, String> {
        self.library_root
            .lock()
            .map(|path| path.clone())
            .map_err(|_| "读取游戏库根目录失败".to_string())
    }

    pub fn games_root(&self) -> Result<PathBuf, String> {
        Ok(self.library_root_path()?.join("games"))
    }

    pub fn body_packages_root(&self) -> Result<PathBuf, String> {
        Ok(self.library_root_path()?.join("body-packages"))
    }

    pub fn saves_root(&self) -> Result<PathBuf, String> {
        Ok(self.library_root_path()?.join("saves"))
    }
}
