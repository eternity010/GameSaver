use crate::{
    domain::{path_utils::normalize_path, AppStore, Game, GameHealth, GameLifecycle, SaveVersion},
    repositories::store_file::{atomic_replace, load_with_recovery},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemovalKind {
    Uninstall,
    Delete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RemovalPhase {
    Prepared,
    Committed,
    Complete,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemovalPlan {
    schema_version: u32,
    pub game_uid: String,
    pub display_name: String,
    pub kind: RemovalKind,
    token: String,
    physical_root: String,
    phase: RemovalPhase,
    body_was_present: bool,
    original_lifecycle: GameLifecycle,
    original_health: GameHealth,
}

#[derive(Default, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemovalOutcome {
    pub removed_paths: usize,
    pub removed_objects: usize,
    pub cleanup_pending: bool,
    pub failed_paths: Vec<String>,
    pub rolled_back: bool,
}

pub enum RemovalRecovery {
    Cleanup(RemovalPlan),
    Finished(RemovalOutcome),
}

pub struct RemovalRecoveryNotice {
    pub game_uid: String,
    pub result: Result<RemovalOutcome, String>,
}

pub struct GameRemovalService;

impl GameRemovalService {
    pub fn validate_game(library_root: &Path, game: &Game) -> Result<(), String> {
        validate_uid(&game.game_uid)?;
        validate_root(library_root)?;
        let expected = library_root.join("games").join(&game.game_uid);
        if normalize_path(Path::new(&game.managed_path)) != normalize_path(&expected) {
            return Err("受管游戏路径不在当前 GameSaver 游戏库中，已停止删除".into());
        }
        validate_target(library_root, &expected)?;
        match fs::metadata(&expected) {
            Ok(metadata) if !metadata.is_dir() => Err("受管游戏路径不是目录，已停止删除".into()),
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("读取受管游戏目录失败：{error}"))
            }
            _ => Ok(()),
        }
    }

    pub fn prepare(
        library_root: &Path,
        game: &Game,
        kind: RemovalKind,
    ) -> Result<RemovalPlan, String> {
        Self::validate_game(library_root, game)?;
        if Self::pending_game_uids(library_root)?.contains(&game.game_uid) {
            return Err("该游戏有未完成的卸载或删除，请先在传输中心重试".into());
        }
        let games_root = library_root.join("games");
        for suffix in ["update.json", "cloud-install.json"] {
            if games_root
                .join(format!(".{}.{suffix}", game.game_uid))
                .exists()
            {
                return Err("该游戏有未完成的更新或安装事务，请先恢复后再删除".into());
            }
        }
        let plan = RemovalPlan {
            schema_version: 1,
            game_uid: game.game_uid.clone(),
            display_name: game.display_name.clone(),
            kind,
            token: Uuid::new_v4().simple().to_string(),
            physical_root: physical_root(library_root)?,
            phase: RemovalPhase::Prepared,
            body_was_present: games_root.join(&game.game_uid).exists(),
            original_lifecycle: game.lifecycle.clone(),
            original_health: game.health.clone(),
        };
        validate_plan(library_root, &plan)?;
        write_plan(library_root, &plan)?;
        Ok(plan)
    }

    pub fn commit(
        library_root: &Path,
        plan: &mut RemovalPlan,
        store: &mut AppStore,
        mut persist: impl FnMut(&AppStore) -> Result<(), String>,
    ) -> Result<Option<String>, String> {
        validate_plan(library_root, plan)?;
        if plan.phase != RemovalPhase::Prepared {
            return Err("删除事务不处于待提交阶段".into());
        }
        let game = store
            .games
            .iter()
            .find(|game| game.game_uid == plan.game_uid)
            .ok_or("游戏记录不存在，已保留删除现场")?;
        Self::validate_game(library_root, game)?;
        let managed = managed_path(library_root, plan);
        let quarantine = quarantine_path(library_root, plan);
        if quarantine.exists() {
            return Err("删除暂存目录已存在，已停止重复提交".into());
        }
        if plan.body_was_present {
            fs::rename(&managed, &quarantine)
                .map_err(|error| format!("移出受管游戏本体失败：{error}"))?;
        } else if managed.exists() {
            return Err("游戏本体在删除准备后发生变化，已停止操作".into());
        }
        let mut candidate = store.clone();
        apply_metadata(&mut candidate, plan)?;
        if let Err(error) = persist(&candidate) {
            let rollback =
                rollback_body(library_root, plan).and_then(|_| complete_plan(library_root, plan));
            return Err(match rollback {
                Ok(()) => format!("保存删除状态失败，已恢复本体：{error}"),
                Err(rollback_error) => {
                    format!("保存删除状态失败：{error}；现场已保留，请重试恢复：{rollback_error}")
                }
            });
        }
        *store = candidate;
        plan.phase = RemovalPhase::Committed;
        Ok(write_plan(library_root, plan).err().map(|error| {
            format!("游戏记录已保存，但删除事务提交未完成，本体已保留，请重试或重启恢复：{error}")
        }))
    }

    pub fn abort(library_root: &Path, plan: &RemovalPlan) -> Result<(), String> {
        validate_plan(library_root, plan)?;
        if plan.phase != RemovalPhase::Prepared {
            return Err("删除事务已提交，不能撤销".into());
        }
        rollback_body(library_root, plan)?;
        complete_plan(library_root, plan)
    }

    pub fn recover_metadata(
        library_root: &Path,
        game_uid: &str,
        store: &mut AppStore,
        mut persist: impl FnMut(&AppStore) -> Result<(), String>,
    ) -> Result<RemovalRecovery, String> {
        validate_uid(game_uid)?;
        validate_root(library_root)?;
        let Some(mut plan) = read_plan(library_root, game_uid)? else {
            return recover_legacy_uninstall(library_root, game_uid, store);
        };
        validate_plan(library_root, &plan)?;
        if plan.game_uid != game_uid {
            return Err("删除事务与游戏标识不一致，已保留现场".into());
        }
        if plan.phase == RemovalPhase::Complete {
            clear_plan(library_root, &plan)?;
            return Ok(RemovalRecovery::Finished(RemovalOutcome::default()));
        }
        let game = store.games.iter().find(|game| game.game_uid == game_uid);
        if plan.phase == RemovalPhase::Prepared {
            if plan.kind == RemovalKind::Delete && game.is_none() {
                plan.phase = RemovalPhase::Committed;
                write_plan(library_root, &plan)?;
            } else {
                let game = game.ok_or("未提交的卸载缺少游戏记录，已保留本体")?;
                Self::validate_game(library_root, game)?;
                rollback_body(library_root, &plan)?;
                let mut candidate = store.clone();
                let game = candidate
                    .games
                    .iter_mut()
                    .find(|game| game.game_uid == game_uid)
                    .unwrap();
                game.lifecycle = plan.original_lifecycle.clone();
                game.health = plan.original_health.clone();
                persist(&candidate)?;
                *store = candidate;
                let failed_paths = complete_plan(library_root, &plan)
                    .err()
                    .into_iter()
                    .collect::<Vec<_>>();
                return Ok(RemovalRecovery::Finished(RemovalOutcome {
                    rolled_back: true,
                    cleanup_pending: !failed_paths.is_empty(),
                    failed_paths,
                    ..Default::default()
                }));
            }
        }
        match (plan.kind, game) {
            (RemovalKind::Delete, Some(_)) => {
                return Err("已提交删除的游戏记录重新出现，已保留现场".into())
            }
            (RemovalKind::Uninstall, Some(game)) => {
                Self::validate_game(library_root, game)?;
                if game.lifecycle != GameLifecycle::NeedsRepair || game.health != GameHealth::Broken
                {
                    return Err("游戏卸载状态与事务不一致，已保留本体".into());
                }
            }
            (RemovalKind::Uninstall, None) => {
                return Err("已提交卸载缺少游戏记录，已保留本体".into())
            }
            _ => {}
        }
        Ok(RemovalRecovery::Cleanup(plan))
    }

    pub fn cleanup(
        library_root: &Path,
        plan: &RemovalPlan,
        mut collect_garbage: impl FnMut() -> Result<usize, String>,
        mut on_progress: impl FnMut(u8, &str),
    ) -> Result<RemovalOutcome, String> {
        validate_plan(library_root, plan)?;
        if plan.phase != RemovalPhase::Committed || managed_path(library_root, plan).exists() {
            return Err("删除事务尚未提交或原本体目录重新出现，已停止清理".into());
        }
        let targets = cleanup_targets(library_root, plan);
        let mut outcome = RemovalOutcome::default();
        for (index, (label, path)) in targets.iter().enumerate() {
            validate_plan(library_root, plan)?;
            validate_target(library_root, path)?;
            on_progress((30 + index * 55 / targets.len().max(1)) as u8, label);
            match fs::remove_dir_all(path) {
                Ok(()) => outcome.removed_paths += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => outcome
                    .failed_paths
                    .push(format!("{label}（{}）：{error}", path.display())),
            }
        }
        if plan.kind == RemovalKind::Delete {
            on_progress(90, "正在回收不再引用的存档对象");
            match collect_garbage() {
                Ok(count) => outcome.removed_objects = count,
                Err(error) => outcome.failed_paths.push(format!("存档对象回收：{error}")),
            }
        }
        if outcome.failed_paths.is_empty() {
            if let Err(error) = complete_plan(library_root, plan) {
                outcome.failed_paths.push(error);
            }
        }
        outcome.cleanup_pending = !outcome.failed_paths.is_empty();
        Ok(outcome)
    }

    pub fn pending_game_uids(library_root: &Path) -> Result<Vec<String>, String> {
        let games_root = library_root.join("games");
        validate_root(library_root)?;
        validate_target(library_root, &games_root)?;
        let entries = match fs::read_dir(&games_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("读取未完成删除事务失败：{error}")),
        };
        let mut pending = BTreeSet::new();
        for entry in entries {
            let name = entry
                .map_err(|error| error.to_string())?
                .file_name()
                .to_string_lossy()
                .into_owned();
            let clean = name.trim_start_matches('.');
            let uid = if let Some((uid, suffix)) = clean.split_once(".removal.json") {
                (suffix.is_empty() || suffix.starts_with(".tmp-") || suffix.starts_with(".bak-"))
                    .then_some(uid)
            } else {
                clean.split_once(".uninstalling-").map(|(uid, _)| uid)
            };
            if let Some(uid) = uid.filter(|uid| validate_uid(uid).is_ok()) {
                pending.insert(uid.to_string());
            }
        }
        Ok(pending.into_iter().collect())
    }

    pub fn recover_pending(
        library_root: &Path,
        store: &mut AppStore,
        mut persist: impl FnMut(&AppStore) -> Result<(), String>,
        mut collect_garbage: impl FnMut(&[SaveVersion]) -> Result<usize, String>,
    ) -> Result<Vec<RemovalRecoveryNotice>, String> {
        let mut notices = Vec::new();
        for game_uid in Self::pending_game_uids(library_root)? {
            let mut candidate = store.clone();
            let result =
                Self::recover_metadata(library_root, &game_uid, &mut candidate, &mut persist)
                    .and_then(|recovered| {
                        *store = candidate;
                        match recovered {
                            RemovalRecovery::Cleanup(plan) => Self::cleanup(
                                library_root,
                                &plan,
                                || collect_garbage(&store.save_versions),
                                |_, _| {},
                            ),
                            RemovalRecovery::Finished(outcome) => Ok(outcome),
                        }
                    });
            notices.push(RemovalRecoveryNotice { game_uid, result });
        }
        Ok(notices)
    }
}

fn validate_uid(uid: &str) -> Result<(), String> {
    if uid.is_empty()
        || uid.starts_with('.')
        || uid.ends_with(['.', ' '])
        || uid
            .chars()
            .any(|character| character.is_control() || "<>:\"/\\|?*".contains(character))
        || Path::new(uid)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("游戏标识不是安全的单层目录名称".into());
    }
    Ok(())
}

fn validate_root(root: &Path) -> Result<(), String> {
    if !root.is_absolute()
        || root
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err("游戏库根目录必须是无上级跳转的绝对路径".into());
    }
    Ok(())
}

fn physical_root(root: &Path) -> Result<String, String> {
    fs::canonicalize(root)
        .map(|path| normalize_path(&path))
        .map_err(|error| format!("解析游戏库真实位置失败：{error}"))
}

pub(crate) fn validate_target(root: &Path, target: &Path) -> Result<(), String> {
    let relative = target
        .strip_prefix(root)
        .map_err(|_| "删除目标超出游戏库")?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err("删除路径包含不安全的目录跳转".into());
        }
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                #[cfg(windows)]
                let linked = {
                    use std::os::windows::fs::MetadataExt;
                    metadata.file_attributes() & 0x400 != 0
                };
                #[cfg(not(windows))]
                let linked = metadata.file_type().is_symlink();
                if linked {
                    return Err(format!(
                        "删除路径包含链接或重解析目录，已停止操作：{}",
                        current.display()
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "检查删除目标失败（{}）：{error}",
                    current.display()
                ))
            }
        }
    }
    Ok(())
}

fn managed_path(root: &Path, plan: &RemovalPlan) -> PathBuf {
    root.join("games").join(&plan.game_uid)
}

fn quarantine_path(root: &Path, plan: &RemovalPlan) -> PathBuf {
    root.join("games")
        .join(format!(".{}.removing-{}", plan.game_uid, plan.token))
}

fn journal_path(root: &Path, uid: &str) -> PathBuf {
    root.join("games").join(format!(".{uid}.removal.json"))
}

fn cleanup_targets(root: &Path, plan: &RemovalPlan) -> Vec<(&'static str, PathBuf)> {
    let mut targets = vec![("正在清理本体文件", quarantine_path(root, plan))];
    if plan.kind == RemovalKind::Delete {
        targets.extend([
            ("正在清理封面", root.join("covers").join(&plan.game_uid)),
            (
                "正在清理本体包",
                root.join("body-packages").join(&plan.game_uid),
            ),
            (
                "正在清理历史本体",
                root.join("games/.versions").join(&plan.game_uid),
            ),
            (
                "正在清理更新暂存",
                root.join("games")
                    .join(format!(".{}.updating", plan.game_uid)),
            ),
        ]);
    }
    targets
}

fn validate_plan(root: &Path, plan: &RemovalPlan) -> Result<(), String> {
    validate_uid(&plan.game_uid)?;
    validate_root(root)?;
    if plan.schema_version != 1
        || Uuid::parse_str(&plan.token).is_err()
        || physical_root(root)? != plan.physical_root
    {
        return Err("删除事务版本、标识或游戏库真实位置不匹配，已保留现场".into());
    }
    validate_target(root, &managed_path(root, plan))?;
    validate_target(root, &journal_path(root, &plan.game_uid))?;
    for (_, path) in cleanup_targets(root, plan) {
        validate_target(root, &path)?;
    }
    Ok(())
}

fn write_plan(root: &Path, plan: &RemovalPlan) -> Result<(), String> {
    validate_plan(root, plan)?;
    let raw = serde_json::to_vec(plan).map_err(|error| error.to_string())?;
    atomic_replace(&journal_path(root, &plan.game_uid), &raw, "游戏删除事务")
}

fn read_plan(root: &Path, uid: &str) -> Result<Option<RemovalPlan>, String> {
    let path = journal_path(root, uid);
    validate_target(root, &path)?;
    let raw = load_with_recovery(&path, "游戏删除事务", |raw| {
        serde_json::from_slice::<RemovalPlan>(raw).is_ok()
    })?;
    raw.map(|raw| {
        serde_json::from_slice(&raw).map_err(|error| format!("解析删除事务失败：{error}"))
    })
    .transpose()
}

fn apply_metadata(store: &mut AppStore, plan: &RemovalPlan) -> Result<(), String> {
    if plan.kind == RemovalKind::Delete {
        store.games.retain(|game| game.game_uid != plan.game_uid);
        store
            .save_profiles
            .retain(|profile| profile.game_uid != plan.game_uid);
        store
            .save_versions
            .retain(|version| version.game_uid != plan.game_uid);
        store
            .body_versions
            .retain(|version| version.game_uid != plan.game_uid);
    } else {
        let game = store
            .games
            .iter_mut()
            .find(|game| game.game_uid == plan.game_uid)
            .ok_or("游戏记录不存在")?;
        game.lifecycle = GameLifecycle::NeedsRepair;
        game.health = GameHealth::Broken;
    }
    Ok(())
}

fn rollback_body(root: &Path, plan: &RemovalPlan) -> Result<(), String> {
    validate_plan(root, plan)?;
    let managed = managed_path(root, plan);
    let quarantine = quarantine_path(root, plan);
    match (managed.exists(), quarantine.exists(), plan.body_was_present) {
        (false, true, true) => fs::rename(&quarantine, &managed)
            .map_err(|error| format!("恢复原游戏本体失败：{error}")),
        (true, false, true) | (false, false, false) => Ok(()),
        _ => Err("游戏本体与暂存目录存在冲突或缺失，已保留现场".into()),
    }
}

fn complete_plan(root: &Path, plan: &RemovalPlan) -> Result<(), String> {
    let mut completed = plan.clone();
    completed.phase = RemovalPhase::Complete;
    write_plan(root, &completed)?;
    clear_plan(root, &completed)
}

fn clear_plan(root: &Path, plan: &RemovalPlan) -> Result<(), String> {
    let path = journal_path(root, &plan.game_uid);
    validate_target(root, &path)?;
    let name = path.file_name().unwrap().to_string_lossy();
    for entry in fs::read_dir(root.join("games")).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let candidate = entry.file_name().to_string_lossy().into_owned();
        if candidate.starts_with(&format!(".{name}.tmp-"))
            || candidate.starts_with(&format!(".{name}.bak-"))
        {
            validate_target(root, &entry.path())?;
            fs::remove_file(entry.path())
                .map_err(|error| format!("清理删除事务残留失败：{error}"))?;
        }
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("清理删除事务记录失败：{error}")),
    }
}

fn recover_legacy_uninstall(
    root: &Path,
    uid: &str,
    store: &AppStore,
) -> Result<RemovalRecovery, String> {
    let prefix = format!(".{uid}.uninstalling-");
    let mut paths = Vec::new();
    for entry in fs::read_dir(root.join("games")).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            paths.push(entry.path());
        }
    }
    if paths.is_empty() {
        return Ok(RemovalRecovery::Finished(RemovalOutcome::default()));
    }
    let game = store
        .games
        .iter()
        .find(|game| game.game_uid == uid)
        .ok_or_else(|| format!("卸载残留缺少游戏记录，已保留：{}", paths[0].display()))?;
    GameRemovalService::validate_game(root, game)?;
    if paths.len() != 1 || Path::new(&game.managed_path).exists() {
        return Err(format!(
            "卸载残留与当前本体冲突，已保留：{}",
            paths[0].display()
        ));
    }
    validate_target(root, &paths[0])?;
    fs::rename(&paths[0], &game.managed_path)
        .map_err(|error| format!("恢复旧卸载现场失败：{error}"))?;
    Ok(RemovalRecovery::Finished(RemovalOutcome {
        rolled_back: true,
        ..Default::default()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{SaveFileEntry, SaveRootType, SaveVersion},
        repositories::SaveRepository,
        test_support::TempWorkspace,
    };
    use sha2::Digest;

    struct Fixture {
        _workspace: TempWorkspace,
        root: PathBuf,
        store: AppStore,
    }

    impl Fixture {
        fn new() -> Self {
            let workspace = TempWorkspace::new("game-removal-fix");
            let root = workspace.join("library");
            fs::create_dir_all(root.join("games/game-a")).unwrap();
            fs::write(root.join("games/game-a/game.exe"), b"body").unwrap();
            let mut game = Game::new_pending(
                "Game A",
                root.join("games/game-a").to_str().unwrap(),
                "game.exe",
            );
            game.game_uid = "game-a".into();
            game.lifecycle = GameLifecycle::Active;
            game.health = GameHealth::Ready;
            let mut store = AppStore::default();
            store.games.push(game);
            Self {
                _workspace: workspace,
                root,
                store,
            }
        }

        fn plan(&self, kind: RemovalKind) -> RemovalPlan {
            GameRemovalService::prepare(&self.root, &self.store.games[0], kind).unwrap()
        }

        fn commit(&mut self, plan: &mut RemovalPlan) {
            let path = self.root.join("store-fixture.json");
            let warning = GameRemovalService::commit(&self.root, plan, &mut self.store, |store| {
                fs::write(&path, serde_json::to_vec(store).unwrap())
                    .map_err(|error| error.to_string())
            })
            .unwrap();
            assert!(warning.is_none());
        }

        fn recover(&mut self) -> RemovalRecovery {
            GameRemovalService::recover_metadata(&self.root, "game-a", &mut self.store, |_| Ok(()))
                .unwrap()
        }

        fn write(&self, relative: &str) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"fixture").unwrap();
            path
        }
    }

    #[test]
    fn external_paths_and_unsafe_ids_are_rejected_before_mutation() {
        let fixture = Fixture::new();
        let external = fixture._workspace.join("external/game-a");
        fs::create_dir_all(&external).unwrap();
        fs::write(external.join("keep.dat"), b"keep").unwrap();
        let mut game = fixture.store.games[0].clone();
        game.managed_path = external.to_string_lossy().into_owned();
        assert!(GameRemovalService::prepare(&fixture.root, &game, RemovalKind::Delete).is_err());
        assert!(external.join("keep.dat").is_file());
        for uid in [
            "",
            "..",
            "../outside",
            "game/a",
            "game\\a",
            "a:b",
            ".hidden",
            "a ",
            "a.",
        ] {
            game.game_uid = uid.into();
            assert!(
                GameRemovalService::prepare(&fixture.root, &game, RemovalKind::Delete).is_err()
            );
        }
        assert!(GameRemovalService::pending_game_uids(&fixture.root)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn verbatim_path_is_accepted_and_file_as_body_is_rejected() {
        let fixture = Fixture::new();
        let mut game = fixture.store.games[0].clone();
        game.managed_path = fs::canonicalize(&game.managed_path)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        GameRemovalService::validate_game(&fixture.root, &game).unwrap();
        fs::remove_dir_all(&game.managed_path).unwrap();
        fs::write(&game.managed_path, b"not a directory").unwrap();
        assert!(GameRemovalService::validate_game(&fixture.root, &game).is_err());
    }

    #[test]
    fn uninstall_preserves_metadata_assets_and_external_saves() {
        let mut fixture = Fixture::new();
        let cover = fixture.write("covers/game-a/cover/display.jpg");
        let package = fixture.write("body-packages/game-a/body.zip");
        let external = fixture._workspace.join("save.dat");
        fs::write(&external, b"save").unwrap();
        fixture
            .store
            .save_versions
            .push(SaveVersion::new("game-a".into(), "1".into(), Vec::new()));
        let mut plan = fixture.plan(RemovalKind::Uninstall);
        fixture.commit(&mut plan);
        let outcome = GameRemovalService::cleanup(
            &fixture.root,
            &plan,
            || panic!("uninstall must not collect CAS"),
            |_, _| {},
        )
        .unwrap();
        assert!(!outcome.cleanup_pending);
        assert_eq!(fixture.store.games[0].lifecycle, GameLifecycle::NeedsRepair);
        assert_eq!(fixture.store.save_versions.len(), 1);
        assert!(cover.is_file() && package.is_file() && external.is_file());
        assert!(!managed_path(&fixture.root, &plan).exists());
        assert!(!quarantine_path(&fixture.root, &plan).exists());
        assert!(GameRemovalService::pending_game_uids(&fixture.root)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn delete_removes_current_assets_and_preserves_cross_game_objects() {
        let mut fixture = Fixture::new();
        let cover = fixture.write("covers/game-a/cover/display.jpg");
        let package = fixture.write("body-packages/game-a/body.zip");
        let old_body = fixture.write("games/.versions/game-a/old/game.exe");
        let other_package = fixture.write("body-packages/game-b/body.zip");
        let shared_hash = format!(
            "{:x}",
            sha2::Sha256::digest(format!("shared:{}", fixture.root.display()).as_bytes())
        );
        let orphan_hash = format!(
            "{:x}",
            sha2::Sha256::digest(format!("orphan:{}", fixture.root.display()).as_bytes())
        );
        let shared = fixture.write(&format!(
            "saves/.gamesaver-repository/objects/sha256/{}/{shared_hash}",
            &shared_hash[..2]
        ));
        let orphan = fixture.write(&format!(
            "saves/.gamesaver-repository/objects/sha256/{}/{orphan_hash}",
            &orphan_hash[..2]
        ));
        let entry = |hash: String| SaveFileEntry {
            root_type: SaveRootType::ManagedGame,
            root_path: None,
            relative_path: "save.dat".into(),
            object_hash: Some(hash),
            size: 7,
            deleted: false,
            mtime_ms: None,
        };
        fixture.store.save_versions.extend([
            SaveVersion::new(
                "game-a".into(),
                "1".into(),
                vec![entry(orphan_hash), entry(shared_hash.clone())],
            ),
            SaveVersion::new("game-b".into(), "1".into(), vec![entry(shared_hash)]),
        ]);
        let mut plan = fixture.plan(RemovalKind::Delete);
        fixture.commit(&mut plan);
        let outcome = GameRemovalService::cleanup(
            &fixture.root,
            &plan,
            || {
                SaveRepository::collect_garbage_in_library(
                    &fixture.root,
                    &fixture.store.save_versions,
                )
            },
            |_, _| {},
        )
        .unwrap();
        assert!(!outcome.cleanup_pending);
        assert_eq!(outcome.removed_objects, 1);
        assert!(!cover.exists() && !package.exists() && !old_body.exists() && !orphan.exists());
        assert!(shared.exists() && other_package.exists());
        assert!(fixture.store.games.is_empty());
        assert_eq!(fixture.store.save_versions.len(), 1);
    }

    #[test]
    fn metadata_failure_restores_body_and_keeps_records() {
        let mut fixture = Fixture::new();
        let mut plan = fixture.plan(RemovalKind::Delete);
        let result =
            GameRemovalService::commit(&fixture.root, &mut plan, &mut fixture.store, |_| {
                Err("injected store failure".into())
            });
        assert!(result.unwrap_err().contains("已恢复本体"));
        assert!(fixture.root.join("games/game-a/game.exe").exists());
        assert_eq!(fixture.store.games.len(), 1);
        assert!(GameRemovalService::pending_game_uids(&fixture.root)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn abort_before_commit_does_not_move_body() {
        let fixture = Fixture::new();
        let plan = fixture.plan(RemovalKind::Uninstall);
        GameRemovalService::abort(&fixture.root, &plan).unwrap();
        assert!(fixture.root.join("games/game-a/game.exe").exists());
        assert!(!journal_path(&fixture.root, "game-a").exists());
    }

    #[test]
    fn prepared_uninstall_after_rename_is_rolled_back() {
        let mut fixture = Fixture::new();
        let plan = fixture.plan(RemovalKind::Uninstall);
        fs::rename(
            managed_path(&fixture.root, &plan),
            quarantine_path(&fixture.root, &plan),
        )
        .unwrap();
        assert!(matches!(
            fixture.recover(),
            RemovalRecovery::Finished(RemovalOutcome {
                rolled_back: true,
                ..
            })
        ));
        assert!(fixture.root.join("games/game-a/game.exe").exists());
        assert_eq!(fixture.store.games[0].lifecycle, GameLifecycle::Active);
    }

    #[test]
    fn metadata_saved_before_uninstall_marker_still_rolls_back() {
        let mut fixture = Fixture::new();
        let plan = fixture.plan(RemovalKind::Uninstall);
        fs::rename(
            managed_path(&fixture.root, &plan),
            quarantine_path(&fixture.root, &plan),
        )
        .unwrap();
        apply_metadata(&mut fixture.store, &plan).unwrap();
        fixture.recover();
        assert_eq!(fixture.store.games[0].lifecycle, GameLifecycle::Active);
        assert_eq!(fixture.store.games[0].health, GameHealth::Ready);
        assert!(fixture.root.join("games/game-a/game.exe").exists());
    }

    #[test]
    fn prepared_delete_with_records_keeps_original_game() {
        let mut fixture = Fixture::new();
        let plan = fixture.plan(RemovalKind::Delete);
        fs::rename(
            managed_path(&fixture.root, &plan),
            quarantine_path(&fixture.root, &plan),
        )
        .unwrap();
        fixture.recover();
        assert_eq!(fixture.store.games.len(), 1);
        assert!(fixture.root.join("games/game-a/game.exe").exists());
    }

    #[test]
    fn prepared_delete_after_metadata_commit_recovers_forward() {
        let mut fixture = Fixture::new();
        let plan = fixture.plan(RemovalKind::Delete);
        let cover = fixture.write("covers/game-a/display.jpg");
        fs::rename(
            managed_path(&fixture.root, &plan),
            quarantine_path(&fixture.root, &plan),
        )
        .unwrap();
        apply_metadata(&mut fixture.store, &plan).unwrap();
        let RemovalRecovery::Cleanup(plan) = fixture.recover() else {
            panic!("must finish committed delete")
        };
        let outcome =
            GameRemovalService::cleanup(&fixture.root, &plan, || Ok(0), |_, _| {}).unwrap();
        assert!(!outcome.cleanup_pending && !cover.exists());
        assert!(fixture.store.games.is_empty());
    }

    #[test]
    fn interrupted_committed_uninstall_resumes_cleanup() {
        let mut fixture = Fixture::new();
        let mut plan = fixture.plan(RemovalKind::Uninstall);
        fixture.commit(&mut plan);
        let RemovalRecovery::Cleanup(plan) = fixture.recover() else {
            panic!("must clean committed uninstall")
        };
        GameRemovalService::cleanup(&fixture.root, &plan, || Ok(0), |_, _| {}).unwrap();
        assert!(!quarantine_path(&fixture.root, &plan).exists());
        assert_eq!(fixture.store.games.len(), 1);
    }

    #[test]
    fn rollback_persist_failure_preserves_recoverable_intent() {
        let mut fixture = Fixture::new();
        let plan = fixture.plan(RemovalKind::Uninstall);
        fs::rename(
            managed_path(&fixture.root, &plan),
            quarantine_path(&fixture.root, &plan),
        )
        .unwrap();
        apply_metadata(&mut fixture.store, &plan).unwrap();
        assert!(GameRemovalService::recover_metadata(
            &fixture.root,
            "game-a",
            &mut fixture.store,
            |_| Err("injected failure".into())
        )
        .is_err());
        assert!(journal_path(&fixture.root, "game-a").exists());
        assert!(fixture.root.join("games/game-a/game.exe").exists());
        fixture.recover();
        assert_eq!(fixture.store.games[0].lifecycle, GameLifecycle::Active);
    }

    #[test]
    fn gc_failure_keeps_journal_and_is_retryable() {
        let mut fixture = Fixture::new();
        let mut plan = fixture.plan(RemovalKind::Delete);
        fixture.commit(&mut plan);
        let outcome = GameRemovalService::cleanup(
            &fixture.root,
            &plan,
            || Err("injected GC failure".into()),
            |_, _| {},
        )
        .unwrap();
        assert!(outcome.cleanup_pending && !outcome.failed_paths.is_empty());
        assert!(journal_path(&fixture.root, "game-a").exists());
        let RemovalRecovery::Cleanup(plan) = fixture.recover() else {
            panic!("must retry cleanup")
        };
        assert!(
            !GameRemovalService::cleanup(&fixture.root, &plan, || Ok(2), |_, _| {})
                .unwrap()
                .cleanup_pending
        );
        assert!(GameRemovalService::pending_game_uids(&fixture.root)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn startup_recovery_finishes_delete_and_reports_corrupt_unrelated_journal() {
        let mut fixture = Fixture::new();
        let mut plan = fixture.plan(RemovalKind::Delete);
        fixture.commit(&mut plan);
        fs::write(journal_path(&fixture.root, "broken-game"), b"invalid").unwrap();
        let notices = GameRemovalService::recover_pending(
            &fixture.root,
            &mut fixture.store,
            |_| Ok(()),
            |_| Ok(0),
        )
        .unwrap();
        assert_eq!(notices.len(), 2);
        assert!(notices
            .iter()
            .find(|notice| notice.game_uid == "game-a")
            .unwrap()
            .result
            .is_ok());
        assert!(notices
            .iter()
            .find(|notice| notice.game_uid == "broken-game")
            .unwrap()
            .result
            .is_err());
        assert!(!quarantine_path(&fixture.root, &plan).exists());
        assert_eq!(
            GameRemovalService::pending_game_uids(&fixture.root).unwrap(),
            vec!["broken-game"]
        );
    }

    #[test]
    fn task_cancel_is_effective_only_before_commit() {
        use crate::{app_state::AppState, domain::TaskCategory, services::TaskService};
        use std::collections::HashMap;
        let mut fixture = Fixture::new();
        let state = AppState::new(
            fixture.store.clone(),
            fixture.root.clone(),
            HashMap::new(),
            fixture.root.join("tasks.json"),
        );
        let task = TaskService::create(
            &state,
            "uninstall_game_body",
            TaskCategory::BodyTransfer,
            Some("game-a".into()),
            "prepare",
        )
        .unwrap();
        let plan = fixture.plan(RemovalKind::Uninstall);
        TaskService::cancel(&state, &task).unwrap();
        assert_eq!(
            TaskService::begin_commit(&state, &task).unwrap_err(),
            "任务已取消"
        );
        GameRemovalService::abort(&fixture.root, &plan).unwrap();
        assert!(fixture.root.join("games/game-a/game.exe").exists());
        let task = TaskService::create(
            &state,
            "uninstall_game_body",
            TaskCategory::BodyTransfer,
            Some("game-a".into()),
            "prepare",
        )
        .unwrap();
        let mut plan = fixture.plan(RemovalKind::Uninstall);
        TaskService::begin_commit(&state, &task).unwrap();
        assert!(TaskService::cancel(&state, &task).is_err());
        assert!(!TaskService::is_cancelled(&state, &task));
        assert_eq!(
            TaskService::get(&state, &task).unwrap().cancellable,
            Some(false)
        );
        fixture.commit(&mut plan);
        assert!(
            !GameRemovalService::cleanup(&fixture.root, &plan, || Ok(0), |_, _| {})
                .unwrap()
                .cleanup_pending
        );
    }

    #[cfg(windows)]
    #[test]
    fn commit_marker_failure_preserves_body_and_committed_store_for_recovery() {
        use std::os::windows::fs::OpenOptionsExt;
        for kind in [RemovalKind::Uninstall, RemovalKind::Delete] {
            let mut fixture = Fixture::new();
            let mut plan = fixture.plan(kind);
            let mut lock = None;
            let warning =
                GameRemovalService::commit(&fixture.root, &mut plan, &mut fixture.store, |_| {
                    lock = Some(
                        fs::OpenOptions::new()
                            .read(true)
                            .share_mode(3)
                            .open(journal_path(&fixture.root, "game-a"))
                            .unwrap(),
                    );
                    Ok(())
                })
                .unwrap();
            assert!(warning.is_some());
            assert!(quarantine_path(&fixture.root, &plan)
                .join("game.exe")
                .exists());
            assert_eq!(fixture.store.games.is_empty(), kind == RemovalKind::Delete);
            drop(lock);
            match fixture.recover() {
                RemovalRecovery::Finished(outcome) => {
                    assert_eq!(kind, RemovalKind::Uninstall);
                    assert!(
                        outcome.rolled_back && fixture.root.join("games/game-a/game.exe").is_file()
                    );
                }
                RemovalRecovery::Cleanup(plan) => {
                    assert_eq!(kind, RemovalKind::Delete);
                    assert!(
                        !GameRemovalService::cleanup(&fixture.root, &plan, || Ok(0), |_, _| {})
                            .unwrap()
                            .cleanup_pending
                    );
                }
            }
        }
    }

    #[test]
    fn persist_and_rollback_conflict_keep_both_directories_for_manual_resolution() {
        let mut fixture = Fixture::new();
        let mut plan = fixture.plan(RemovalKind::Delete);
        let managed = managed_path(&fixture.root, &plan);
        let result =
            GameRemovalService::commit(&fixture.root, &mut plan, &mut fixture.store, |_| {
                fs::create_dir_all(&managed).unwrap();
                fs::write(managed.join("new.exe"), b"new").unwrap();
                Err("injected persistence failure".into())
            });
        assert!(result.is_err());
        assert!(quarantine_path(&fixture.root, &plan)
            .join("game.exe")
            .exists());
        assert!(managed.join("new.exe").exists());
        assert!(journal_path(&fixture.root, "game-a").exists());
        assert_eq!(fixture.store.games.len(), 1);
    }

    #[test]
    fn missing_body_uninstalls_without_touching_cover_or_package() {
        let mut fixture = Fixture::new();
        let cover = fixture.write("covers/game-a/cover.jpg");
        fs::remove_dir_all(fixture.root.join("games/game-a")).unwrap();
        let mut plan = fixture.plan(RemovalKind::Uninstall);
        fixture.commit(&mut plan);
        let outcome = GameRemovalService::cleanup(
            &fixture.root,
            &plan,
            || panic!("unexpected GC"),
            |_, _| {},
        )
        .unwrap();
        assert!(!outcome.cleanup_pending && cover.exists());
        assert_eq!(fixture.store.games[0].lifecycle, GameLifecycle::NeedsRepair);
    }

    #[cfg(windows)]
    #[test]
    fn cas_junction_is_not_traversed_during_delete_collection() {
        use std::os::windows::process::CommandExt;
        let mut fixture = Fixture::new();
        let outside = fixture._workspace.join("external-objects");
        fs::create_dir_all(outside.join("aa")).unwrap();
        fs::write(outside.join("aa/keep"), b"keep").unwrap();
        let junction = fixture
            .root
            .join("saves")
            .join(".gamesaver-repository")
            .join("objects")
            .join("sha256");
        fs::create_dir_all(junction.parent().unwrap()).unwrap();
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "junction creation failed: {status:?}"
        );
        let mut plan = fixture.plan(RemovalKind::Delete);
        fixture.commit(&mut plan);
        let outcome = GameRemovalService::cleanup(
            &fixture.root,
            &plan,
            || {
                SaveRepository::collect_garbage_in_library(
                    &fixture.root,
                    &fixture.store.save_versions,
                )
            },
            |_, _| {},
        )
        .unwrap();
        assert!(outcome.cleanup_pending);
        assert!(outside.join("aa/keep").is_file());
        fs::remove_dir(junction).unwrap();
    }

    #[test]
    fn corrupt_or_mismatched_journal_never_deletes_body() {
        let mut fixture = Fixture::new();
        let mut plan = fixture.plan(RemovalKind::Delete);
        plan.game_uid = "other-game".into();
        fs::write(
            journal_path(&fixture.root, "game-a"),
            serde_json::to_vec(&plan).unwrap(),
        )
        .unwrap();
        assert!(GameRemovalService::recover_metadata(
            &fixture.root,
            "game-a",
            &mut fixture.store,
            |_| Ok(())
        )
        .is_err());
        fs::write(journal_path(&fixture.root, "game-a"), b"broken").unwrap();
        assert!(GameRemovalService::recover_metadata(
            &fixture.root,
            "game-a",
            &mut fixture.store,
            |_| Ok(())
        )
        .is_err());
        assert!(fixture.root.join("games/game-a/game.exe").exists());
        assert_eq!(
            GameRemovalService::pending_game_uids(&fixture.root).unwrap(),
            vec!["game-a"]
        );
    }

    #[test]
    fn completed_tombstone_does_not_resurrect_backup() {
        let mut fixture = Fixture::new();
        let plan = fixture.plan(RemovalKind::Uninstall);
        let residue = fixture.root.join("games/..game-a.removal.json.bak-audit");
        fs::write(&residue, serde_json::to_vec(&plan).unwrap()).unwrap();
        let mut complete = plan.clone();
        complete.phase = RemovalPhase::Complete;
        write_plan(&fixture.root, &complete).unwrap();
        assert!(matches!(fixture.recover(), RemovalRecovery::Finished(_)));
        assert!(!residue.exists());
        assert!(GameRemovalService::pending_game_uids(&fixture.root)
            .unwrap()
            .is_empty());
        assert!(fixture.root.join("games/game-a/game.exe").exists());
    }

    #[test]
    fn missing_primary_journal_recovers_atomic_write_residue() {
        let mut fixture = Fixture::new();
        let plan = fixture.plan(RemovalKind::Uninstall);
        let residue = fixture.root.join("games/..game-a.removal.json.bak-audit");
        fs::rename(journal_path(&fixture.root, "game-a"), residue).unwrap();
        fs::rename(
            managed_path(&fixture.root, &plan),
            quarantine_path(&fixture.root, &plan),
        )
        .unwrap();
        assert_eq!(
            GameRemovalService::pending_game_uids(&fixture.root).unwrap(),
            vec!["game-a"]
        );
        fixture.recover();
        assert!(fixture.root.join("games/game-a/game.exe").exists());
    }

    #[test]
    fn legacy_uninstall_is_restored_instead_of_deleted() {
        let mut fixture = Fixture::new();
        let managed = fixture.root.join("games/game-a");
        let legacy = fixture.root.join("games/.game-a.uninstalling-old");
        fs::rename(&managed, &legacy).unwrap();
        let RemovalRecovery::Finished(outcome) = fixture.recover() else {
            panic!("legacy should roll back")
        };
        assert!(outcome.rolled_back);
        assert!(managed.join("game.exe").is_file());
        assert!(!legacy.exists());
    }

    #[test]
    fn recovery_never_overwrites_new_body() {
        let mut fixture = Fixture::new();
        let plan = fixture.plan(RemovalKind::Uninstall);
        fs::rename(
            managed_path(&fixture.root, &plan),
            quarantine_path(&fixture.root, &plan),
        )
        .unwrap();
        fixture.write("games/game-a/new.exe");
        assert!(GameRemovalService::recover_metadata(
            &fixture.root,
            "game-a",
            &mut fixture.store,
            |_| Ok(())
        )
        .is_err());
        assert!(quarantine_path(&fixture.root, &plan)
            .join("game.exe")
            .exists());
        assert!(fixture.root.join("games/game-a/new.exe").exists());
    }

    #[cfg(windows)]
    #[test]
    fn locked_body_cleanup_keeps_journal_until_retry() {
        use std::os::windows::fs::OpenOptionsExt;
        let mut fixture = Fixture::new();
        let mut plan = fixture.plan(RemovalKind::Uninstall);
        fixture.commit(&mut plan);
        let file = fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(quarantine_path(&fixture.root, &plan).join("game.exe"))
            .unwrap();
        let outcome =
            GameRemovalService::cleanup(&fixture.root, &plan, || Ok(0), |_, _| {}).unwrap();
        assert!(outcome.cleanup_pending);
        assert!(!outcome.failed_paths.is_empty());
        assert!(quarantine_path(&fixture.root, &plan)
            .join("game.exe")
            .is_file());
        drop(file);
        let RemovalRecovery::Cleanup(plan) = fixture.recover() else {
            panic!("must retry cleanup")
        };
        assert!(
            !GameRemovalService::cleanup(&fixture.root, &plan, || Ok(0), |_, _| {})
                .unwrap()
                .cleanup_pending
        );
    }

    #[cfg(windows)]
    #[test]
    fn junction_asset_parent_is_rejected_without_touching_external_files() {
        use std::os::windows::process::CommandExt;
        let fixture = Fixture::new();
        let external = fixture._workspace.join("external-covers");
        fs::create_dir_all(external.join("game-a")).unwrap();
        fs::write(external.join("game-a/keep.jpg"), b"keep").unwrap();
        let junction = fixture.root.join("covers");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&external)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(status.status.success());
        assert!(GameRemovalService::prepare(
            &fixture.root,
            &fixture.store.games[0],
            RemovalKind::Delete
        )
        .is_err());
        assert!(external.join("game-a/keep.jpg").is_file());
        fs::remove_dir(junction).unwrap();
    }
}
