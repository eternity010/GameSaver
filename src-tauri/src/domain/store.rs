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
        for (idx, game) in self.games.iter_mut().enumerate() {
            game.game_key = if game.game_key.trim().is_empty() {
                Game::derive_game_key(&game.display_name)
            } else {
                game.game_key
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_lowercase()
            };
            if game.added_at.is_none() {
                game.added_at = Some((1700000000u64 + (idx as u64) * 3600).to_string());
            }
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
        // 上面的 retain 可能正好裁掉 `latest_save_version_id` 指向的那一版，于是留下一个
        // 悬垂指针。它的危害不在当下而在之后：`commit` 会因此失去「与哪一版比对」的基线，
        // 此后每次游戏退出都无条件新建版本（存档管理审查 V5）。载入时就修掉，别把坏状态
        // 带进运行期。先取出 uid 列表，避免在遍历 `self.games` 的同时可变借用它。
        let game_uids: Vec<String> = self
            .games
            .iter()
            .map(|game| game.game_uid.clone())
            .collect();
        for game_uid in game_uids {
            self.repair_latest_save_version_id(&game_uid);
        }
    }

    /// 某游戏最新一版存档的 `version_id`（`created_at` 最大者；`created_at` 相同时按
    /// `version_id` 兜底比较，与各处剪枝/排序用的口径保持一致）。
    fn newest_save_version_id(&self, game_uid: &str) -> Option<String> {
        self.save_versions
            .iter()
            .filter(|version| version.game_uid == game_uid)
            .max_by(|left, right| {
                left.created_at
                    .cmp(&right.created_at)
                    .then(left.version_id.cmp(&right.version_id))
            })
            .map(|version| version.version_id.clone())
    }

    /// 修复 `latest_save_version_id`：指针仍指向本游戏现存的版本就**原样保留**，只有为空、
    /// 或指向的版本已不存在（悬垂）时才回退到最新一版。
    ///
    /// 为什么不无脑改写成最新一版：这个指针是 `commit` 判断「存档有没有变化」的比对基线
    /// （`SaveRepository::commit` 拿它做逐文件哈希复用 + 缺失文件的墓碑判定），而**恢复旧版本
    /// 后它会有意指向那个旧版本** —— 那时本地内容就等于旧版本，下一次游戏退出理当比对出
    /// 「无变化」。一旦改写成最新一版，这次比对必然发现差异，白多出一个版本。
    ///
    /// 真正要修的是悬垂场景（存档管理审查 V5）：剪枝/删除把指针指向的版本删掉之后，指针解析
    /// 出 `None`，`commit` 丢掉基线，于是**此后每次游戏退出都无条件新建版本**，版本库持续
    /// 无意义膨胀，也违反设计文档「没有变化时不创建新版本」。
    pub fn repair_latest_save_version_id(&mut self, game_uid: &str) {
        let Some(game) = self.games.iter().find(|game| game.game_uid == game_uid) else {
            return;
        };
        let pointer_still_resolves = game.latest_save_version_id.as_deref().is_some_and(|id| {
            self.save_versions
                .iter()
                .any(|version| version.game_uid == game_uid && version.version_id == id)
        });
        if pointer_still_resolves {
            return;
        }
        let newest = self.newest_save_version_id(game_uid);
        if let Some(game) = self.games.iter_mut().find(|game| game.game_uid == game_uid) {
            game.latest_save_version_id = newest;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AppStore, Game, SaveVersion};
    use crate::domain::{SaveFileEntry, SaveRootType};

    fn game(uid: &str, latest_save_version_id: Option<&str>) -> Game {
        let mut game = Game::new_pending("A", "C:/games/a", "a.exe");
        game.game_uid = uid.to_string();
        game.latest_save_version_id = latest_save_version_id.map(str::to_string);
        game
    }

    fn version(game_uid: &str, version_id: &str, created_at: &str) -> SaveVersion {
        SaveVersion {
            version_id: version_id.to_string(),
            game_uid: game_uid.to_string(),
            created_at: created_at.to_string(),
            files: Vec::new(),
            total_bytes: 0,
        }
    }

    fn version_with_file(game_uid: &str, version_id: &str, created_at: &str) -> SaveVersion {
        SaveVersion {
            version_id: version_id.to_string(),
            game_uid: game_uid.to_string(),
            created_at: created_at.to_string(),
            files: vec![SaveFileEntry {
                root_type: SaveRootType::Custom,
                root_path: None,
                relative_path: "save.dat".to_string(),
                object_hash: Some("h".to_string()),
                size: 1,
                deleted: false,
                mtime_ms: None,
            }],
            total_bytes: 1,
        }
    }

    /// 恢复旧版本后指针会**有意**停在那个旧版本上（本地内容就等于它），此时绝不能
    /// 把它改写成最新一版：那会让下一次游戏退出比对出「有变化」，白多一个版本。
    #[test]
    fn repair_keeps_a_pointer_that_still_resolves_even_when_it_is_not_the_newest() {
        let mut store = AppStore {
            games: vec![game("game-a", Some("restored"))],
            save_versions: vec![
                version("game-a", "restored", "1700000001"),
                version("game-a", "newer", "1700000002"),
            ],
            ..AppStore::default()
        };

        store.repair_latest_save_version_id("game-a");

        assert_eq!(
            store.games[0].latest_save_version_id.as_deref(),
            Some("restored"),
            "仍能解析到的指针必须原样保留，哪怕它不是最新一版"
        );
    }

    /// V5 主体：指针悬垂时回退到最新一版，`commit` 才有比对基线。
    #[test]
    fn repair_falls_back_to_the_newest_when_the_pointer_dangles() {
        let mut store = AppStore {
            games: vec![game("game-a", Some("gone"))],
            // 故意乱序：取的是 created_at 最大者，而不是列表里的最后一个。
            save_versions: vec![
                version("game-a", "a1", "1700000001"),
                version("game-a", "a3", "1700000003"),
                version("game-a", "a2", "1700000002"),
            ],
            ..AppStore::default()
        };

        store.repair_latest_save_version_id("game-a");

        assert_eq!(
            store.games[0].latest_save_version_id.as_deref(),
            Some("a3"),
            "悬垂指针应回退到 created_at 最新的一版"
        );
    }

    /// 指针悬垂、且该游戏一版不剩时，清空指针（而不是留着一个永远解析不到的 id）。
    #[test]
    fn repair_clears_a_dangling_pointer_when_no_version_survives() {
        let mut store = AppStore {
            games: vec![game("game-a", Some("gone"))],
            save_versions: vec![version("game-b", "b1", "1700000001")],
            ..AppStore::default()
        };

        store.repair_latest_save_version_id("game-a");

        assert_eq!(store.games[0].latest_save_version_id, None);
    }

    /// 修复是逐游戏的：不能因为修 A 就顺手改了 B。
    #[test]
    fn repair_only_touches_the_named_game() {
        let mut store = AppStore {
            games: vec![game("game-a", Some("gone")), game("game-b", Some("gone"))],
            save_versions: vec![version("game-a", "a1", "1700000001")],
            ..AppStore::default()
        };

        store.repair_latest_save_version_id("game-a");

        assert_eq!(store.games[0].latest_save_version_id.as_deref(), Some("a1"));
        assert_eq!(
            store.games[1].latest_save_version_id.as_deref(),
            Some("gone"),
            "未被点名的游戏不得被改动"
        );
    }

    /// 载入时的 sanitize 也会裁掉版本记录，那同样可能让指针悬垂 —— 修在 normalize 里，
    /// 坏状态就不会被带进运行期。
    #[test]
    fn normalize_repairs_a_pointer_left_dangling_by_dropped_versions() {
        let mut store = AppStore {
            games: vec![game("game-a", Some("broken"))],
            save_versions: vec![
                // files 为空 → normalize 判定为无效记录并丢弃，指针随之悬垂。
                version("game-a", "broken", "1700000003"),
                version_with_file("game-a", "good", "1700000001"),
            ],
            ..AppStore::default()
        };

        store.normalize();

        let ids: Vec<&str> = store
            .save_versions
            .iter()
            .map(|version| version.version_id.as_str())
            .collect();
        assert_eq!(ids, vec!["good"], "无效版本记录应被裁掉");
        assert_eq!(
            store.games[0].latest_save_version_id.as_deref(),
            Some("good"),
            "指针指向的版本被裁掉后必须修复"
        );
    }

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
