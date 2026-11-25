use super::*;
use crate::{
    domain::{AppStore, GameBodyVersion, GameLifecycle, SaveRootType, SaveScope},
    repositories::store_file::atomic_replace,
    services::{
        cloud_account_service::{merge_profile, CloudAccountProfile, CloudAccountService},
        cloud_manifest_service::{
            CloudBodyManifestVersion, CloudGameCatalog, CloudManifestService,
        },
        BodyPackageService, GameBodyInstallService, LaunchService,
    },
    test_support::TempWorkspace,
};
use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const GAME_KEY: &str = "new-machine-fixture";
const BODY_VERSION: &str = "body-version-1";

#[derive(Serialize, Deserialize)]
struct ExportedFixture {
    account: CloudAccountProfile,
    catalog: CloudGameCatalog,
    body: CloudBodyManifestVersion,
    save: CloudSaveManifestVersion,
}

fn object_path(root: &Path, hash: &str) -> PathBuf {
    root.join(&hash[..2]).join(hash)
}

fn store_object(root: &Path, bytes: &[u8]) -> String {
    let hash = sha256_hex(bytes);
    let path = object_path(root, &hash);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
    hash
}

fn export_old_machine(root: &Path) {
    let source = root.join("source");
    let cloud = root.join("cloud");
    let managed = source.join("games").join(Uuid::new_v4().to_string());
    fs::create_dir_all(managed.join("SaveData")).unwrap();
    fs::create_dir_all(managed.join("Assets")).unwrap();
    fs::create_dir_all(&cloud).unwrap();
    fs::write(managed.join("game.exe"), b"fixture-game-executable").unwrap();
    fs::write(managed.join("Assets/data.bin"), b"fixture-game-assets").unwrap();
    fs::write(managed.join("SaveData/slot.dat"), b"old-machine-progress-0").unwrap();
    let mut game = Game::new_pending("迁移测试游戏", managed.to_string_lossy(), "game.exe");
    game.game_key = GAME_KEY.into();
    game.launch.arguments = vec!["--windowed".into()];
    let scopes = [
        (
            SaveRootType::ManagedGame,
            managed.join("SaveData").to_string_lossy().into_owned(),
        ),
        (
            SaveRootType::LocalLow,
            r"C:\Users\OldUser\AppData\LocalLow\Studio\Game".into(),
        ),
        (
            SaveRootType::AppData,
            r"C:\Users\OldUser\AppData\Roaming\Studio\Game".into(),
        ),
        (
            SaveRootType::LocalAppData,
            r"C:\Users\OldUser\AppData\Local\Studio\Game".into(),
        ),
        (
            SaveRootType::SavedGames,
            r"C:\Users\OldUser\Saved Games\Studio\Game".into(),
        ),
        (
            SaveRootType::ProgramData,
            r"C:\ProgramData\Studio\Game".into(),
        ),
    ]
    .into_iter()
    .map(|(root_type, root_path)| {
        let mut scope = SaveScope::new_manual(root_path);
        scope.root_type = root_type;
        scope.confirmed_files = vec!["slot.dat".into()];
        scope
    })
    .collect::<Vec<_>>();
    let profile = SaveProfile::new(
        game.game_uid.clone(),
        sha256_hex(b"exe"),
        scopes,
        100,
        "1".into(),
    );
    game.activate(&profile.profile_id);
    let objects = source.join("objects");
    let files = profile
        .scopes
        .iter()
        .enumerate()
        .map(|(index, scope)| {
            let bytes = format!("old-machine-progress-{index}");
            SaveFileEntry {
                root_type: scope.root_type,
                root_path: Some(scope.root_path.clone()),
                relative_path: "slot.dat".into(),
                object_hash: Some(store_object(&objects, bytes.as_bytes())),
                size: bytes.len() as u64,
                deleted: false,
                mtime_ms: Some(1),
            }
        })
        .collect::<Vec<_>>();
    let version = SaveVersion {
        version_id: "save-version-1".into(),
        game_uid: game.game_uid.clone(),
        created_at: "100".into(),
        total_bytes: files.iter().map(|entry| entry.size).sum(),
        files,
    };
    let meta = CloudSavePackageMeta {
        format_version: 2,
        version: version.clone(),
        game_key: game.game_key.clone(),
        display_name: game.display_name.clone(),
        created_at: "100".into(),
        device_name: Some("Old PC".into()),
    };
    let (_, save_hash, save_size) = write_save_package(
        &cloud.join("save.zip"),
        &meta,
        |entry| {
            fs::read(object_path(&objects, entry.object_hash.as_deref().unwrap()))
                .map_err(|error| error.to_string())
        },
        &|_, _| true,
    )
    .unwrap();
    let body = BodyPackageService::create_package_with_exclusions(
        &managed,
        &source.join("packages"),
        &game.game_uid,
        BODY_VERSION,
        "game.exe",
        &["SaveData".into()],
        |_, _| {},
        || false,
    )
    .unwrap();
    fs::copy(&body.package_path, cloud.join("body.zip")).unwrap();
    let account = CloudAccountService::build(
        &AppStore {
            games: vec![game.clone()],
            save_profiles: vec![profile],
            ..AppStore::default()
        },
        false,
    );
    let fixture = ExportedFixture {
        account,
        catalog: CloudManifestService::catalog_from_game(&game),
        body: CloudBodyManifestVersion {
            version_id: BODY_VERSION.into(),
            created_at: "100".into(),
            package_path: format!("/apps/GameSaver/games/{GAME_KEY}/body/{BODY_VERSION}.zip"),
            package_fs_id: 1,
            package_size: fs::metadata(cloud.join("body.zip")).unwrap().len(),
            package_sha256: Some(body.sha256),
            file_count: body.manifest.file_count,
            total_bytes: body.manifest.total_bytes,
        },
        save: CloudSaveManifestVersion {
            version_id: version.version_id,
            created_at: version.created_at,
            package_path: format!("/apps/GameSaver/saves/{GAME_KEY}/save-version-1.zip"),
            package_fs_id: 2,
            package_size: save_size,
            package_sha256: Some(save_hash),
            file_count: version.files.len(),
            total_bytes: version.total_bytes,
            device_name: Some("Old PC".into()),
        },
    };
    fs::write(
        cloud.join("fixture.json"),
        serde_json::to_vec(&fixture).unwrap(),
    )
    .unwrap();
    fs::remove_dir_all(&source).unwrap();
}

fn run_new_machine_case(scenario: &str) {
    let workspace = TempWorkspace::new("new-machine");
    export_old_machine(&workspace);
    assert!(!workspace.join("source").exists());
    let user = workspace.join("target/Users/新用户");
    let output = fs::File::create(workspace.join("worker.log")).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "services::cloud_save_service::new_machine_tests::new_machine_worker",
            "--nocapture",
        ])
        .env("GAMESAVER_NEW_MACHINE_ROOT", workspace.as_ref())
        .env("GAMESAVER_NEW_MACHINE_SCENARIO", scenario)
        .env("USERPROFILE", &user)
        .env("APPDATA", user.join("AppData/Roaming"))
        .env("LOCALAPPDATA", user.join("AppData/Local"))
        .env("PROGRAMDATA", workspace.join("target/ProgramData"))
        .stdin(Stdio::null())
        .stdout(output.try_clone().unwrap())
        .stderr(output)
        .spawn()
        .unwrap();
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "{scenario}: {}",
                fs::read_to_string(workspace.join("worker.log")).unwrap()
            );
            break;
        }
        if started.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("new machine worker timed out: {scenario}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn new_machine_restores_game_and_all_save_roots_without_old_machine_files() {
    run_new_machine_case("roundtrip");
}

#[test]
fn new_machine_failed_install_never_becomes_launchable() {
    run_new_machine_case("install-failure");
}

#[test]
fn new_machine_missing_object_preserves_every_existing_save() {
    run_new_machine_case("missing-object");
}

#[test]
fn new_machine_corrupt_object_preserves_every_existing_save() {
    run_new_machine_case("corrupt-object");
}

#[test]
fn new_machine_cancelled_import_never_overwrites_saves() {
    run_new_machine_case("cancel-import");
}

#[test]
fn new_machine_uncommitted_restore_rolls_back_all_save_roots() {
    run_new_machine_case("rollback");
}

#[test]
fn new_machine_restart_rolls_back_an_uncommitted_restore() {
    run_new_machine_case("restart-uncommitted");
}

#[test]
fn new_machine_restart_keeps_a_committed_restore() {
    run_new_machine_case("restart-committed");
}

#[test]
fn new_machine_missing_body_and_custom_roots_are_not_created_implicitly() {
    let workspace = TempWorkspace::new("new-machine-root-guards");
    let managed = workspace.join("games/not-installed");
    let game = Game::new_pending("Not installed", managed.to_string_lossy(), "game.exe");
    for root_type in [SaveRootType::ManagedGame, SaveRootType::Custom] {
        let destination = if root_type == SaveRootType::ManagedGame {
            managed.join("SaveData")
        } else {
            workspace.join("missing-custom-disk/Game")
        };
        let mut scope = SaveScope::new_manual(destination.to_string_lossy().into_owned());
        scope.root_type = root_type;
        let version = SaveVersion {
            version_id: "remote-save".into(),
            game_uid: game.game_uid.clone(),
            created_at: "1".into(),
            files: vec![SaveFileEntry {
                root_type,
                root_path: Some(scope.root_path.clone()),
                relative_path: "slot.dat".into(),
                object_hash: Some(sha256_hex(b"progress")),
                size: 8,
                deleted: false,
                mtime_ms: None,
            }],
            total_bytes: 8,
        };
        let profile = SaveProfile::new(
            game.game_uid.clone(),
            "hash".into(),
            vec![scope],
            100,
            "1".into(),
        );
        let result = SaveRepository::restore_new_machine_fixture(
            &workspace.join("objects"),
            &workspace.join("journals"),
            &game,
            &profile,
            &version,
            None,
        );
        let error = result.err().expect("missing root must be rejected");
        assert!(
            if root_type == SaveRootType::ManagedGame {
                error.contains("请先安装游戏")
            } else {
                error.contains("存档范围不存在")
            },
            "{error}"
        );
        assert!(!destination.exists());
        assert!(!workspace.join("journals").exists());
    }
}

#[test]
#[ignore = "spawned with isolated environment by the new_machine tests"]
fn new_machine_worker() {
    let Some(root) = std::env::var_os("GAMESAVER_NEW_MACHINE_ROOT").map(PathBuf::from) else {
        return;
    };
    let scenario = std::env::var("GAMESAVER_NEW_MACHINE_SCENARIO").unwrap();
    let fixture: ExportedFixture =
        serde_json::from_slice(&fs::read(root.join("cloud/fixture.json")).unwrap()).unwrap();
    let target_root = root.join("target");
    let games = target_root.join("新游戏库/games");
    let packages = target_root.join("新游戏库/body-packages");
    let objects = target_root.join("新游戏库/save-objects");
    let data = target_root.join("platform-data");
    for directory in [&games, &packages, &objects, &data] {
        fs::create_dir_all(directory).unwrap();
    }
    let mut store = AppStore::default();
    assert_eq!(
        merge_profile(&mut store, &games, fixture.account.clone()).unwrap(),
        1
    );
    let local_uid = store.games[0].game_uid.clone();
    assert_ne!(local_uid, fixture.catalog.game_uid);
    assert_eq!(
        merge_profile(&mut store, &games, fixture.account).unwrap(),
        0
    );
    assert_eq!(store.games.len(), 1);
    assert_eq!(store.save_profiles.len(), 1);
    assert!(
        !LaunchService::precheck(&store, &local_uid)
            .unwrap()
            .can_launch
    );
    let profile = store.save_profiles[0].clone();
    for scope in &profile.scopes {
        assert!(
            Path::new(&scope.root_path).starts_with(&target_root),
            "escaped sandbox: {}",
            scope.root_path
        );
        assert!(!Path::new(&scope.root_path).exists());
    }
    let target = GameBodyInstallService::resolve_target(
        &store,
        &games,
        &fixture.catalog.game_uid,
        GAME_KEY,
        true,
    )
    .unwrap();
    let staging = games.join(format!(".{local_uid}.cloud-installing"));
    let transaction = Uuid::new_v4().to_string();
    let package =
        GameBodyInstallService::install_package_path(&packages, &local_uid, &transaction).unwrap();
    fs::create_dir_all(package.parent().unwrap()).unwrap();
    fs::copy(root.join("cloud/body.zip"), &package).unwrap();
    let actual_hash = sha256_file_cancellable(&package, || true).unwrap();
    let body = BodyPackageService::extract_cloud_package(
        &package,
        &staging,
        BODY_VERSION,
        "game.exe",
        fixture.body.package_sha256.as_deref().unwrap(),
        &actual_hash,
        |_, _| {},
        || false,
    )
    .unwrap();
    assert_eq!(body.file_count, fixture.body.file_count);
    assert_eq!(body.total_bytes, fixture.body.total_bytes);
    GameBodyInstallService::preserve_managed_saves(&target, &store.save_profiles, &staging, || {
        false
    })
    .unwrap();
    let game = GameBodyInstallService::installed_game(&target, &fixture.catalog);
    let mut candidate = store.clone();
    candidate.games[0] = game.clone();
    candidate.body_versions.push(GameBodyVersion {
        version_id: BODY_VERSION.into(),
        game_uid: local_uid.clone(),
        created_at: "100".into(),
        archive_path: String::new(),
        file_count: body.file_count,
        total_bytes: body.total_bytes,
        package_path: Some(package.to_string_lossy().into_owned()),
        sha256: Some(actual_hash),
        excluded_items: body.excluded_items,
        upload_status: Some("synced".into()),
        remote_path: Some(fixture.body.package_path),
        remote_fs_id: Some(1),
        remote_size: Some(fixture.body.package_size),
    });
    let store_path = data.join("store.json");
    let install = GameBodyInstallService::commit_install(
        &games,
        &packages,
        &target,
        &transaction,
        BODY_VERSION,
        || {
            if scenario == "install-failure" {
                return Err("injected persistence failure".into());
            }
            atomic_replace(
                &store_path,
                &serde_json::to_vec(&candidate).unwrap(),
                "test store",
            )
        },
    );
    if scenario == "install-failure" {
        assert!(install
            .unwrap_err()
            .contains("injected persistence failure"));
        assert!(!target.managed_path.exists());
        assert!(!store_path.exists());
        assert!(
            !LaunchService::precheck(&store, &local_uid)
                .unwrap()
                .can_launch
        );
        assert!(GameBodyInstallService::pending_install_game_uids(&games)
            .unwrap()
            .is_empty());
        return;
    }
    install.unwrap();
    store = serde_json::from_slice(&fs::read(&store_path).unwrap()).unwrap();
    assert_eq!(store.games[0].lifecycle, GameLifecycle::Active);
    assert_eq!(store.games[0].launch.arguments, ["--windowed"]);
    assert!(
        LaunchService::precheck(&store, &local_uid)
            .unwrap()
            .can_launch
    );
    assert!(!target.managed_path.join("SaveData/slot.dat").exists());
    assert_eq!(
        fs::read(target.managed_path.join("game.exe")).unwrap(),
        b"fixture-game-executable"
    );
    let mut backup = SaveVersion {
        version_id: "new-machine-backup".into(),
        game_uid: local_uid.clone(),
        created_at: "200".into(),
        files: Vec::new(),
        total_bytes: 0,
    };
    if scenario != "roundtrip" {
        for scope in &profile.scopes {
            let bytes = b"existing-new-machine-progress";
            fs::create_dir_all(&scope.root_path).unwrap();
            fs::write(Path::new(&scope.root_path).join("slot.dat"), bytes).unwrap();
            backup.files.push(SaveFileEntry {
                root_type: scope.root_type,
                root_path: Some(scope.root_path.clone()),
                relative_path: "slot.dat".into(),
                object_hash: Some(store_object(&objects, bytes)),
                size: bytes.len() as u64,
                deleted: false,
                mtime_ms: None,
            });
            backup.total_bytes += bytes.len() as u64;
        }
    }
    let save_zip = root.join("cloud/save.zip");
    assert_eq!(
        sha256_file_cancellable(&save_zip, || true).unwrap(),
        fixture.save.package_sha256.clone().unwrap()
    );
    let mut archive = ZipArchive::new(fs::File::open(save_zip).unwrap()).unwrap();
    let meta: CloudSavePackageMeta =
        serde_json::from_reader(archive.by_name("meta.json").unwrap()).unwrap();
    let expected = validate_downloaded_save_meta(&meta, &game, &fixture.save).unwrap();
    let imported_objects = import_archive_objects(
        &mut archive,
        &expected,
        |hash, bytes| {
            assert_eq!(store_object(&objects, bytes), hash);
            Ok(())
        },
        &|_, _| scenario != "cancel-import",
    );
    if scenario == "cancel-import" {
        assert_eq!(imported_objects.unwrap_err(), "任务已取消");
        for scope in &profile.scopes {
            assert_eq!(
                fs::read(Path::new(&scope.root_path).join("slot.dat")).unwrap(),
                b"existing-new-machine-progress"
            );
        }
        return;
    }
    imported_objects.unwrap();
    let mut imported = meta.version;
    imported.game_uid = local_uid.clone();
    let damaged_path = object_path(
        &objects,
        imported
            .files
            .last()
            .unwrap()
            .object_hash
            .as_deref()
            .unwrap(),
    );
    if scenario == "missing-object" {
        fs::remove_file(&damaged_path).unwrap();
    }
    if scenario == "corrupt-object" {
        fs::write(&damaged_path, b"damaged-object").unwrap();
    }
    let journals = data.join("save-restore-journals");
    let restored = SaveRepository::restore_new_machine_fixture(
        &objects,
        &journals,
        &game,
        &profile,
        &imported,
        (!backup.files.is_empty()).then_some(&backup),
    );
    if matches!(scenario.as_str(), "missing-object" | "corrupt-object") {
        assert!(restored.is_err());
    } else {
        let receipt = restored.unwrap_or_else(|error| panic!("restore: {error}"));
        if scenario == "rollback" {
            SaveRepository::rollback_restore(receipt).unwrap();
        } else if scenario == "restart-uncommitted" {
            drop(receipt);
            store = serde_json::from_slice(&fs::read(&store_path).unwrap()).unwrap();
            SaveRepository::recover_restore_journals(&data, &mut store).unwrap();
        } else {
            store.save_versions.push(imported.clone());
            store.games[0].latest_save_version_id = Some(imported.version_id.clone());
            receipt.record_commit(&mut store);
            atomic_replace(
                &store_path,
                &serde_json::to_vec(&store).unwrap(),
                "restored store",
            )
            .unwrap();
            if scenario == "restart-committed" {
                drop(receipt);
                store = serde_json::from_slice(&fs::read(&store_path).unwrap()).unwrap();
                SaveRepository::recover_restore_journals(&data, &mut store).unwrap();
            } else {
                SaveRepository::try_finalize_restore(receipt, &mut store).unwrap();
            }
        }
    }
    for (index, scope) in profile.scopes.iter().enumerate() {
        let bytes = fs::read(Path::new(&scope.root_path).join("slot.dat")).unwrap();
        let expected = if matches!(scenario.as_str(), "roundtrip" | "restart-committed") {
            format!("old-machine-progress-{index}").into_bytes()
        } else {
            b"existing-new-machine-progress".to_vec()
        };
        assert_eq!(bytes, expected, "wrong progress in {:?}", scope.root_type);
    }
    assert!(GameBodyInstallService::pending_install_game_uids(&games)
        .unwrap()
        .is_empty());
    if scenario == "roundtrip" {
        let receipt = SaveRepository::restore_new_machine_fixture(
            &objects,
            &journals,
            &game,
            &profile,
            &imported,
            Some(&imported),
        )
        .unwrap_or_else(|error| panic!("repeat restore: {error}"));
        receipt.record_commit(&mut store);
        SaveRepository::try_finalize_restore(receipt, &mut store).unwrap();
        assert_eq!(store.save_versions.len(), 1);
        for (index, scope) in profile.scopes.iter().enumerate() {
            assert_eq!(
                fs::read(Path::new(&scope.root_path).join("slot.dat")).unwrap(),
                format!("old-machine-progress-{index}").as_bytes()
            );
        }
        let persisted: AppStore = serde_json::from_slice(&fs::read(&store_path).unwrap()).unwrap();
        assert_eq!(
            persisted.games[0].latest_save_version_id.as_deref(),
            Some("save-version-1")
        );
        let status = build_sync_status(
            true,
            &persisted.save_versions,
            &[fixture.save],
            Some("save-version-1"),
            Some("save-version-1"),
        );
        assert_eq!(status.sync_state, "synced");
        assert_eq!(
            GameBodyInstallService::recover_pending_installs(&games, &packages, &persisted)
                .unwrap(),
            0
        );
    }
    assert!(store.committed_save_restores.is_empty());
    assert!(fs::read_dir(&journals).unwrap().next().is_none());
}
