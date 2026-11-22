//! 「这个文件像不像存档」的路径判定。
//!
//! 放在领域层的原因：收集侧（`repositories::save_repository::collect_profile_files`）
//! 与恢复侧（同文件的 `collect_protected_paths`）必须共用**同一道门** —— 只在一侧生效
//! 就会出现「恢复时把一个从没备份过的文件当成多出来的受保护文件删掉」—— 而仓储层不能
//! 反向依赖服务层。此前这一簇住在 `services/save_learning_service.rs`，R2b 第 1 步把
//! 它们整体下沉到这里（纯搬运，判定逻辑一字未改）。
//!
//! 判定全是纯逻辑：只看路径字符串与扩展名，不碰 IO，也不看文件内容。

use std::path::Path;

use super::path_utils::normalize_path;

/// 直接认作存档的扩展名。
///
/// 这张表只放**专属存档**格式：命中即成立，不需要其它线索。
///
/// `db` / `sqlite` / `vdf` 原先也在这张表里，2026-09-15 的识别误报复盘把它们摘了出去
/// （见 `docs/save-recognition-audit-2026-09-15.md` 的成因 A）：它们是缓存、索引、遥测、
/// 设置最常用的格式（`vdf` 更是 Steam 自己的配置格式），只看扩展名就会把
/// `archiveList.sqlite`、`<uuid>.sqlite` 这类**资源索引 / 安装配置库**当成存档收进保护范围
/// —— 实测那两处误报正是"文件名无线索、路径无线索、父目录不是容器"，
/// 唯一成立的就是扩展名。
///
/// 它们现在仍可能被接受，但必须另有依据——文件名命中 [`NAME_HINTS`]（如 `save.db`）、
/// 或落在命名容器 / 存档语义路径段下（如 `<范围>\SaveData\x.sqlite`）：
/// 这层语义已由 [`is_save_candidate`] 收尾处的
/// `has_save_container || has_save_path_hint || has_file_hint` 提供，不需要额外判定。
pub(crate) const SAVE_EXTENSIONS: [&str; 7] =
    ["sav", "save", "es3", "sl2", "ess", "savegame", "state"];
/// 资源类扩展名：出现即排除，它们几乎不可能是存档。
pub(crate) const RESOURCE_EXTENSIONS: [&str; 12] = [
    "dll", "exe", "pak", "pdb", "png", "jpg", "jpeg", "webp", "ogg", "wav", "mp3", "ttf",
];
/// 文件名（不含扩展名）里出现即算「像存档」的线索。
pub(crate) const NAME_HINTS: [&str; 8] = [
    "save", "slot", "profile", "userdata", "autosave", "progress", "system", "remote",
];
/// 目录名命中即认为「这个目录本身就是存档容器」。
pub(crate) const SAVE_DIRECTORY_HINTS: [&str; 9] = [
    "save",
    "savedata",
    "saves",
    "savegame",
    "savegames",
    "profile",
    "profiles",
    "userdata",
    "remote",
];

/// 纯噪音路径：日志、崩溃转储、浏览器缓存等，绝不可能是存档。
///
/// 这条判定同时被 ETW 事件解析与快照扫描使用，两侧口径必须一致。
pub(crate) fn should_ignore_event_path(path: &Path) -> bool {
    let text = path.to_string_lossy().to_ascii_lowercase();
    if text.ends_with(".log")
        || text.ends_with(".dmp")
        || text.ends_with(".etl")
        || text.ends_with(".pma")
        || text.ends_with(".ico.md5")
        || text.ends_with("chrome_shutdown_ms.txt")
    {
        return true;
    }
    if (text.contains(r"\appdata\local\") || text.contains(r"\appdata\roaming\"))
        && (text.contains(r"\user data\") || text.contains("/user data/"))
    {
        return true;
    }
    [
        "com.gamesaver.desktop",
        "com.gamesaver.next",
        "\\appdata\\local\\temp\\",
        "\\appdata\\local\\microsoft\\windows\\powershell\\",
        "\\shadervariantanalytics\\",
        "\\shadercache\\",
        "\\gpucache\\",
        "\\d3dscache\\",
        "\\blob_storage\\",
        "\\session storage\\",
        "\\local storage\\",
        "\\indexeddb\\",
        "\\code cache\\",
        "\\dawngraphitecache\\",
        "\\dawnwebgpucache\\",
        "\\grshadercache\\",
        "\\graphitedawncache\\",
        "\\autofillstrikedatabase\\",
        "\\clientcertificates\\",
        "\\data_reduction_proxy_leveldb\\",
        "\\shared_proto_db\\",
        "\\optimization_guide_hint_cache_store\\",
        "\\segmentation platform\\",
        "\\sync data\\",
        "\\safe browsing network\\",
        "\\platform notifications\\",
        "\\extension rules\\",
        "\\extension scripts\\",
        "\\extension state\\",
        "\\feature engagement tracker\\",
        "\\gcm store\\",
        "\\shared dictionary\\",
        "\\site characteristics database\\",
        "\\videodecodestats\\",
        "\\cache\\",
        "\\logs\\",
        "\\temp\\",
        "\\crashdumps\\",
        "\\crashreportclient\\",
        "\\crashpad\\",
        "\\nvidia\\dxcache\\",
        "\\nvidia\\glcache\\",
        "\\nvidia\\compute_cache\\",
        "\\wettype\\",
        "\\inputmethod\\",
        "\\microsoft\\inputmethod\\",
        "\\wer\\",
        "\\webcache\\",
        "\\player.log",
        "\\player-prev.log",
        "/cache/",
        "/logs/",
        "/gpucache/",
        "/shadercache/",
        "/webcache/",
        "/code cache/",
        "/player.log",
        "/player-prev.log",
        // Steam 模拟器（RUNE / CODEX / OnlineFix / Goldberg 等）自身的状态文件：它们由注入
        // 游戏进程的 DLL 写入，ETW 看到的就是"游戏进程写了它"，但属于模拟器配置而非存档。
        // 实测误报：`…\Steam\RUNE\1903340\achievements.ini` 与 `leaderboards.ini` 各 32 字节，
        // 却因为 appid 目录被判为命名容器而成了候选。
        // 审计依据：docs/save-recognition-audit-2026-09-15.md 的成因 B。
        "\\achievements.ini",
        "\\leaderboards.ini",
        "\\steam_emu.ini",
        "\\steam_appid.txt",
        "\\steam_settings\\",
    ]
    .iter()
    .any(|fragment| text.contains(fragment))
}

/// 快照扫描用的噪音判定：在 [`should_ignore_event_path`] 之上，再挡掉原子保存的中间产物。
///
/// `.tmp` / `.temp` / `.bak` 是「写临时文件再重命名」留下的，扫描阶段不该收；
/// 但事务证据那条路对它们有**例外**（那是保存动作的典型形态），见
/// `save_learning_service::is_transaction_evidence_path`。
pub(crate) fn should_ignore_snapshot_path(path: &Path) -> bool {
    let text = path.to_string_lossy().to_ascii_lowercase();
    if text.ends_with(".tmp") || text.ends_with(".temp") || text.ends_with(".bak") {
        return true;
    }
    should_ignore_event_path(path)
}

pub(crate) fn is_noise_path(path: &Path) -> bool {
    should_ignore_snapshot_path(path)
}

/// 目录名本身是不是「命名存档容器」。
///
/// 命中只是自动收集的路径线索。`userdata` 需要额外要求路径里含 `\steam\`：
/// 很多游戏的 `%APPDATA%\<厂商>\UserData` 只是普通用户数据目录，不是 Steam 的存档容器。
pub(crate) fn is_save_container_directory(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    if SAVE_DIRECTORY_HINTS.contains(&name.as_str()) {
        if name == "userdata" {
            let norm = normalize_path(path);
            return norm.contains(r"\steam\");
        }
        return true;
    }
    let norm = normalize_path(path);
    if norm.contains(r"\steam\")
        || norm.contains(r"\rune\")
        || norm.contains(r"\codex\")
        || norm.contains(r"\onlinefix\")
    {
        if name.chars().all(|c| c.is_ascii_digit()) && name.len() >= 2 {
            return true;
        }
    }
    false
}

pub(crate) fn path_has_segment(path: &str, segment: &str) -> bool {
    path.split(['\\', '/'])
        .any(|item| item.eq_ignore_ascii_case(segment))
}

pub(crate) fn path_has_save_container_ancestor(path: &Path) -> bool {
    let mut current = Some(path);
    while let Some(candidate) = current {
        if is_save_container_directory(candidate) {
            return true;
        }
        current = candidate.parent();
    }
    false
}

pub(crate) fn is_generic_config_file(
    path: &Path,
    normalized_path: &str,
    has_save_container: bool,
    has_save_path_hint: bool,
) -> bool {
    let is_ini = path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("ini"));
    let file_stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let has_explicit_save_name = ["save", "slot", "userdata", "autosave", "progress"]
        .iter()
        .any(|hint| file_stem.contains(hint));
    is_ini
        && path_has_segment(normalized_path, "config")
        && !has_save_container
        && !has_save_path_hint
        && !has_explicit_save_name
}

pub(crate) fn is_automatic_save_file(path: &Path) -> bool {
    if is_noise_path(path) {
        return false;
    }
    if path_has_segment(&path.to_string_lossy(), "analytics") {
        return false;
    }
    let extension = path.extension().and_then(|value| value.to_str()).unwrap_or_default();
    !RESOURCE_EXTENSIONS.iter().any(|resource| extension.eq_ignore_ascii_case(resource))
}

/// 「这个文件像不像存档」。
///
/// 四条任一成立即为真：住在命名容器目录下、路径里有存档语义的段、文件名含线索词、
/// 扩展名在白名单里；但先被噪音路径与资源扩展名一票否决，且 `config` 目录下的
/// 通用 `.ini` 会被特意摘出去。
///
/// 这是**启发式**，不是判决：误判为真会把别人的存档收进来，误判为假会漏收。
/// 所以它只用来「提议」或「过滤目录级收集」，不用来直接改写用户的规则。
pub(crate) fn is_save_candidate(path: &str) -> bool {
    let path_obj = Path::new(path);
    if !is_automatic_save_file(path_obj) {
        return false;
    }
    let path_lower = path.to_ascii_lowercase();
    let extension = path_obj
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let file_stem = path_obj
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let has_file_hint = NAME_HINTS.iter().any(|hint| file_stem.contains(hint));
    let has_save_container = path_obj
        .parent()
        .is_some_and(path_has_save_container_ancestor);
    let has_save_path_hint = [
        "savedata",
        "savegame",
        "savegames",
        "userdata",
        "profiles",
        "remote",
    ]
    .iter()
    .any(|hint| path_has_segment(&path_lower, hint));
    if is_generic_config_file(
        path_obj,
        &path_lower,
        has_save_container,
        has_save_path_hint,
    ) {
        return false;
    }
    has_save_container
        || has_save_path_hint
        || has_file_hint
        || SAVE_EXTENSIONS.iter().any(|save| extension.eq_ignore_ascii_case(save))
}
