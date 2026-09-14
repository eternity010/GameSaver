use crate::{
    app_state::{AppState, CloudOperationClaim},
    domain::{Game, SaveProfile, SaveVersion, TaskCategory, TaskRetry, TaskStatus},
    repositories::BaiduConfigRepository,
    services::{
        BaiduNetdiskClient, CloudSaveManifestVersion, CloudSaveOverview, CloudSaveService,
        CloudSaveSyncStatusView, TaskService,
    },
};
use tauri::{AppHandle, Manager, State};

#[tauri::command]
pub fn get_cloud_save_status(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
) -> Result<CloudSaveSyncStatusView, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "锁定本地存储失败".to_string())?;
    let game = store
        .games
        .iter()
        .find(|g| g.game_uid == game_uid)
        .ok_or_else(|| "未找到指定游戏".to_string())?
        .clone();
    drop(store);

    CloudSaveService::get_sync_status(&app, &game)
}

#[tauri::command]
pub fn get_cloud_save_overview(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
) -> Result<CloudSaveOverview, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "锁定本地存储失败".to_string())?;
    let game = store
        .games
        .iter()
        .find(|g| g.game_uid == game_uid)
        .ok_or_else(|| "未找到指定游戏".to_string())?
        .clone();
    drop(store);

    CloudSaveService::get_overview(&app, &game)
}

#[tauri::command]
pub fn list_cloud_save_versions(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
) -> Result<Vec<CloudSaveManifestVersion>, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "锁定本地存储失败".to_string())?;
    let game = store
        .games
        .iter()
        .find(|g| g.game_uid == game_uid)
        .ok_or_else(|| "未找到指定游戏".to_string())?
        .clone();
    drop(store);

    let client = load_baidu_client(&app)?;
    let manifest = CloudSaveService::fetch_manifest(&client, &game.game_key, &game.game_uid)?;
    Ok(manifest.map(|m| m.versions).unwrap_or_default())
}

#[tauri::command]
pub fn start_upload_save_version_task(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
    version_id: String,
) -> Result<String, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "锁定本地存储失败".to_string())?;
    let game = store
        .games
        .iter()
        .find(|g| g.game_uid == game_uid)
        .ok_or_else(|| "未找到指定游戏".to_string())?
        .clone();
    let profile = game
        .save_profile_id
        .as_ref()
        .and_then(|pid| store.save_profiles.iter().find(|p| &p.profile_id == pid))
        .ok_or_else(|| "未找到该游戏的存档保护规则".to_string())?
        .clone();
    let version = store
        .save_versions
        .iter()
        .find(|v| v.game_uid == game_uid && v.version_id == version_id)
        .ok_or_else(|| "未找到指定的本地存档版本".to_string())?
        .clone();
    drop(store);

    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|err| format!("解析应用数据目录失败：{err}"))?;
    let keep_limit = BaiduConfigRepository::load(&app_data_dir)?
        .map(|c| c.cloud_save_keep_limit)
        .unwrap_or(10);

    // 云端清单是「读-改-写」：一次上传会 fetch 清单、插入自己这条、再整体写回。
    // 两次并发的云端操作会各自读到同一份旧清单，后写的那次把先写的条目**整条丢掉**，
    // 而丢掉的那份包已经上传到网盘、又不在清单里，于是永远无法在界面上删除。
    // 退出游戏时的自动同步与用户手动同步正是可以并发的两条路径，所以这里必须互斥。
    // 认领在 `drop(store)` 之后、发起任务之前：既避开持着 store 锁去抢操作锁，也让
    // 「已被占用」能在真正干活之前就返回。
    //
    // 认领走 `CloudOperationClaim` 守卫而不是裸 key：下面任何一步失败都会自动释放。
    // 收窄前这里是手写配对，`?` 早退会漏掉释放，而泄漏的 key 是纯内存、只有重启才清。
    let claim = CloudOperationClaim::claim(&state, &game_uid)?;

    let task_id = begin_sync(
        &state,
        &format!("上传【{}】游戏存档至百度网盘", game.display_name),
        &game.game_uid,
    )?;
    let task_id_for_thread = task_id.clone();
    let app_for_thread = app.clone();
    // 交接给工作线程：`disarm` 之后守卫不再释放，由线程在结束时释放这把 key。
    let claim_key = claim.disarm();

    std::thread::spawn(move || {
        let result = upload_save_worker(
            &app_for_thread,
            &task_id_for_thread,
            &game,
            &profile,
            &version,
            keep_limit,
        );
        // 释放必须早于 `finish_sync`：任务状态一旦推送成功，前端就可能立刻发起下一次
        // 同步，此刻若 key 还没释放，用户会莫名收到「已有同步任务正在进行」。
        let state = app_for_thread.state::<AppState>();
        state.release_operation(&claim_key);
        finish_sync(
            &state,
            &task_id_for_thread,
            result,
            &format!("【{}】游戏存档已成功同步至百度网盘", game.display_name),
            &format!("【{}】游戏存档云端同步失败", game.display_name),
            TaskRetry {
                operation: "sync_cloud_save".to_string(),
                game_uid: game.game_uid.clone(),
                game_key: None,
                version_id: Some(version.version_id.clone()),
                remote_path: None,
                remote_fs_id: None,
            },
        );
    });

    Ok(task_id)
}

#[tauri::command]
pub fn start_restore_cloud_save_task(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
    version_id: String,
) -> Result<String, String> {
    let game_uid = game_uid.trim().to_string();
    let store = state
        .store
        .lock()
        .map_err(|_| "锁定本地存储失败".to_string())?;
    let game = store
        .games
        .iter()
        .find(|g| g.game_uid == game_uid)
        .ok_or_else(|| "未找到指定游戏".to_string())?
        .clone();
    let profile = game
        .save_profile_id
        .as_ref()
        .and_then(|pid| store.save_profiles.iter().find(|p| &p.profile_id == pid))
        .ok_or_else(|| "未找到该游戏的存档保护规则".to_string())?
        .clone();
    drop(store);

    // 认领必须在**读云端清单之前**：上传路径会把超出 keep_limit 的历史版本从清单里
    // 裁掉并删除对应远程包。若先挑好版本再认领，中间那次上传可能刚好把我们要还原的
    // 那份删掉，于是拿着一个已不存在的 remote_version 去下载。
    //
    // 认领走 `CloudOperationClaim` 守卫：下面三次 `?` 早退（凭据读取失败、清单取不到、
    // 指定版本不存在）都会经 `Drop` 释放。收窄前这里是手写配对，这三处**全都会漏**，
    // 而泄漏的 key 是纯内存、只有重启才清 —— 用户重试永远撞「已有同步任务正在进行」。
    let claim = CloudOperationClaim::claim(&state, &game_uid)?;

    let client = load_baidu_client(&app)?;
    let manifest = CloudSaveService::fetch_manifest(&client, &game.game_key, &game.game_uid)?
        .ok_or_else(|| "未找到云端存档清单".to_string())?;
    let remote_version = manifest
        .versions
        .iter()
        .find(|v| v.version_id == version_id)
        .ok_or_else(|| "未找到指定的云端存档版本".to_string())?
        .clone();

    let task_id = begin_sync(
        &state,
        &format!("从云端还原【{}】游戏存档", game.display_name),
        &game_uid,
    )?;
    let task_id_for_thread = task_id.clone();
    let app_for_thread = app.clone();
    // 交接给工作线程：`disarm` 之后守卫不再释放，由线程在结束时释放这把 key。
    let claim_key = claim.disarm();

    std::thread::spawn(move || {
        let result = restore_save_worker(
            &app_for_thread,
            &task_id_for_thread,
            &game,
            &profile,
            &remote_version,
        );
        // 同上传：先放掉 key 再推送任务状态，别让紧随其后的操作被自己的 key 挡住。
        let state = app_for_thread.state::<AppState>();
        state.release_operation(&claim_key);
        finish_sync(
            &state,
            &task_id_for_thread,
            result,
            &format!("【{}】云端存档已成功还原至本地", game.display_name),
            &format!("【{}】云端存档还原失败", game.display_name),
            TaskRetry {
                operation: "restore_cloud_save".to_string(),
                game_uid: game.game_uid.clone(),
                game_key: None,
                version_id: Some(remote_version.version_id.clone()),
                remote_path: None,
                remote_fs_id: None,
            },
        );
    });

    Ok(task_id)
}

#[tauri::command]
pub fn delete_cloud_save_version(
    app: AppHandle,
    state: State<AppState>,
    game_uid: String,
    version_id: String,
) -> Result<Vec<CloudSaveManifestVersion>, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "锁定本地存储失败".to_string())?;
    let game = store
        .games
        .iter()
        .find(|g| g.game_uid == game_uid)
        .ok_or_else(|| "未找到指定游戏".to_string())?
        .clone();
    drop(store);

    // 删除同样是「读清单 → 裁掉一条 → 整体写回」，必须与上传/还原互斥；否则一次
    // 并发上传的条目会被这次写回整条抹掉。
    //
    // 认领走 `CloudOperationClaim` 守卫，整个函数不再有手写释放：这里没有工作线程，
    // 守卫在函数返回时 `Drop` 即释放 —— 包括 `load_baidu_client` 与 `delete_cloud_version`
    // 两处 `?` 早退。收窄前作者只想到了后者，漏了前者。
    let _claim = CloudOperationClaim::claim(&state, &game_uid)?;

    let client = load_baidu_client(&app)?;
    let deleted = CloudSaveService::delete_cloud_version(
        &client,
        &game.game_key,
        &game.game_uid,
        &version_id,
    );
    Ok(deleted?.versions)
}

fn upload_save_worker(
    app: &AppHandle,
    task_id: &str,
    game: &Game,
    profile: &SaveProfile,
    version: &SaveVersion,
    keep_limit: usize,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        5,
        "正在准备上传存档",
        None,
    );
    let client = load_baidu_client(app)?;
    CloudSaveService::upload_save_version(
        app,
        &client,
        game,
        profile,
        version,
        keep_limit,
        |pct, msg| {
            TaskService::update(&state, task_id, TaskStatus::Running, pct, msg, None);
            true
        },
    )?;
    Ok(())
}

fn restore_save_worker(
    app: &AppHandle,
    task_id: &str,
    game: &Game,
    profile: &SaveProfile,
    remote_version: &CloudSaveManifestVersion,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    TaskService::update(
        &state,
        task_id,
        TaskStatus::Running,
        5,
        "正在连接网盘下载存档",
        None,
    );
    let client = load_baidu_client(app)?;
    CloudSaveService::download_and_restore_cloud_save(
        app,
        &client,
        game,
        profile,
        remote_version,
        |pct, msg| {
            TaskService::update(&state, task_id, TaskStatus::Running, pct, msg, None);
            true
        },
    )?;
    Ok(())
}

fn begin_sync(state: &AppState, title: &str, game_uid: &str) -> Result<String, String> {
    let task_id = TaskService::create(
        state,
        "sync_cloud_save",
        TaskCategory::CloudSaveSync,
        Some(game_uid.to_string()),
        title,
    )?;
    TaskService::update(state, &task_id, TaskStatus::Running, 0, "任务已创建", None);
    Ok(task_id)
}

/// 把一次同步的结果写成任务的终态。
///
/// 吃 `&AppState` 而不是 `&AppHandle`：它本来只用 `app.state::<AppState>()` 取一下绑定，
/// 把这一步移到调用方之后，这一层就能直接测 —— 终态、错误文案、重试载荷都是在这里拼的。
/// 与 `8d09f9f` 的接缝同款做法：**外层解析依赖，内层只吃解出来的值**。
fn finish_sync(
    state: &AppState,
    task_id: &str,
    result: Result<(), String>,
    success_message: &str,
    failed_prefix: &str,
    retry: TaskRetry,
) {
    match result {
        Ok(_) => {
            // 必须用 finish 而不是 update：update 只改内存、不落盘，任务终态和 error
            // 永远写不进 tasks.json。重启后加载兜底会把它标成「异常中断」——一次成功的
            // 同步看起来像故障，一次真实的失败则连原因都丢了。
            TaskService::finish(
                state,
                task_id,
                TaskStatus::Success,
                100,
                success_message,
                None,
                None,
            );
        }
        Err(error) => {
            // 只在失败时补重试参数，让传输中心的「重试」按钮有东西可点。
            let _ = TaskService::set_retry(state, task_id, retry);
            TaskService::finish(
                state,
                task_id,
                TaskStatus::Failed,
                100,
                format!("{failed_prefix}：{error}"),
                None,
                Some(error),
            );
        }
    }
}

fn load_baidu_client(app: &AppHandle) -> Result<BaiduNetdiskClient, String> {
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|err| format!("解析应用数据目录失败：{err}"))?;
    BaiduNetdiskClient::load_from_app_data(&app_data_dir)
}

/// 编排层的测试。
///
/// 这一层此前**零测试**（本文件 414 行）。补的范围刻意收在「不需要 Tauri 与网络」的
/// 那段：`begin_sync` / `finish_sync` 是任务状态机与重试载荷的拼装处，而它们只需要
/// `&AppState` —— `TaskService` 全部方法都只吃 `&AppState`，事件推送走的是
/// `AppState` 里注入的函数指针，所以整条链在测试里跑得起来，不必碰 `AppHandle`
/// （一碰就会把窗口运行时链进测试二进制并直接启动失败，见 `app_state.rs` 开头）。
///
/// **不测**的是 `upload_save_worker` / `restore_save_worker`：它们要把 `AppHandle`
/// 一路传进 `CloudSaveService::{upload_save_version, download_and_restore_cloud_save}`，
/// 而那两支又继续传给 `package_save_version`、`protect_current_save_version`、
/// `SaveRepository::restore`、`GameRepository::persist`。要测就得在最破坏性的那几条
/// 路径上做四、五层接缝重构 + 注入假 client，而收益与 `cloud_save_service` 已有的
/// 16 条测试高度重叠（workers 本身只是「更新任务 → 取 client → 调服务」的胶水）。
/// 这是**有意不做**，不是遗漏。
#[cfg(test)]
mod tests {
    use super::{begin_sync, finish_sync};
    use crate::{
        app_state::AppState,
        domain::{AppStore, TaskCategory, TaskRetry, TaskStatus},
        repositories::TaskRepository,
        services::TaskService,
        test_support::TempWorkspace,
    };

    fn test_state(dir: &TempWorkspace) -> AppState {
        AppState::new(
            AppStore::default(),
            dir.to_path_buf(),
            std::collections::HashMap::new(),
            dir.join("tasks.json"),
        )
    }

    fn retry() -> TaskRetry {
        TaskRetry {
            operation: "sync_cloud_save".to_string(),
            game_uid: "game-1".to_string(),
            game_key: None,
            version_id: Some("version-1".to_string()),
            remote_path: None,
            remote_fs_id: None,
        }
    }

    /// `begin_sync` 建出的任务必须带「云端存档同步」分类。
    ///
    /// 分类不是装饰：它决定任务是否进传输中心、能否取消、是否计入角标（见
    /// `TaskCategory` 的注释 —— 这里写错，用户就看不到进度也没法取消）。
    #[test]
    fn begin_sync_creates_a_running_cloud_save_task() {
        let dir = TempWorkspace::new("cloud-save-commands");
        let state = test_state(&dir);

        let task_id =
            begin_sync(&state, "上传【样例】游戏存档至百度网盘", "game-1").expect("创建同步任务");
        let task = TaskService::get(&state, &task_id).expect("任务应当存在");

        assert_eq!(task.task_type, "sync_cloud_save");
        assert_eq!(task.category, Some(TaskCategory::CloudSaveSync));
        assert_eq!(task.game_uid.as_deref(), Some("game-1"));
        assert_eq!(task.status, TaskStatus::Running);
        assert_eq!(task.progress, 0);
        assert_eq!(task.message, "任务已创建");
    }

    /// 成功路径：终态 Success、进度 100、**不**挂重试载荷。
    ///
    /// 落盘那一条断言守的是**这一层的选择**：`TaskService` 自己已有测试证明 `finish`
    /// 会落盘，但那只证明得了 helper —— 若有人把 `finish_sync` 改回 `update`，
    /// helper 的测试照样全绿。这里断的是「本函数用的是 `finish`」。
    #[test]
    fn finish_sync_records_success_and_persists_it() {
        let dir = TempWorkspace::new("cloud-save-commands");
        let state = test_state(&dir);
        let task_id =
            begin_sync(&state, "上传【样例】游戏存档至百度网盘", "game-1").expect("创建同步任务");

        finish_sync(
            &state,
            &task_id,
            Ok(()),
            "【样例】游戏存档已成功同步至百度网盘",
            "【样例】游戏存档云端同步失败",
            retry(),
        );

        let task = TaskService::get(&state, &task_id).expect("任务应当存在");
        assert_eq!(task.status, TaskStatus::Success);
        assert_eq!(task.progress, 100);
        assert_eq!(task.message, "【样例】游戏存档已成功同步至百度网盘");
        assert!(task.error.is_none(), "成功路径不该记录 error");
        assert!(task.retry.is_none(), "成功路径不该挂重试载荷");

        let reloaded = TaskRepository::load(&state.tasks_path).expect("重新读取任务记录");
        let persisted = reloaded
            .get(&task_id)
            .expect("终态必须落盘，否则重启后会被兜底标成「异常中断」");
        assert_eq!(persisted.status, TaskStatus::Success);
    }

    /// 失败路径：终态 Failed、文案拼成「前缀：原因」、error 记下原因，且**必须**挂上
    /// 重试载荷 —— 传输中心那个「重试」按钮就靠它才有东西可点。
    #[test]
    fn finish_sync_records_failure_with_prefix_and_retry() {
        let dir = TempWorkspace::new("cloud-save-commands");
        let state = test_state(&dir);
        let task_id =
            begin_sync(&state, "上传【样例】游戏存档至百度网盘", "game-1").expect("创建同步任务");

        finish_sync(
            &state,
            &task_id,
            Err("token 已失效".to_string()),
            "【样例】游戏存档已成功同步至百度网盘",
            "【样例】游戏存档云端同步失败",
            retry(),
        );

        let task = TaskService::get(&state, &task_id).expect("任务应当存在");
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.message, "【样例】游戏存档云端同步失败：token 已失效");
        assert_eq!(task.error.as_deref(), Some("token 已失效"));
        let retry = task.retry.expect("失败必须挂重试载荷");
        assert_eq!(retry.operation, "sync_cloud_save");
        assert_eq!(retry.game_uid, "game-1");
        assert_eq!(retry.version_id.as_deref(), Some("version-1"));
    }
}
