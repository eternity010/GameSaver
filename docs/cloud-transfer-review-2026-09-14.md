# GameSaver 云上传 / 云下载模块审计（2026-09-14）

> 审计日期：2026-09-14
> 范围：百度网盘云存档同步（上传 / 下载 / 恢复）与游戏本体云传输（上传 / 下载 / 安装）
> 关联：`docs/game-state-sync-review-2026-09-10.md`（状态机与前端同步）、`docs/code-review-2026-09-10.md`（全局）

---

## 0. 结论摘要

| # | 级别 | 状态 | 问题 | 位置 |
|---|---|---|---|---|
| C1 | **高** | ✅ 已修（§0.1） | 云存档的远程路径由未校验的 `game_key` / `version_id` 直接拼接；`version_id` 一路来自 IPC，全链路无校验，而 `delete_cloud_version` 会用它**删除远程文件** | `cloud_save_service.rs:96/100/104/517`、`cloud_save_commands.rs:238` |
| C5 | 中 | ✅ 已修（§0.2） | 云端游戏安装的暂存目录 `.cloud-installing` **没有任何清理**，中断后该游戏永久无法再次安装，且报错让用户「重启应用」——重启并不清理 | `baidu_commands.rs:1538-1541`、`lib.rs:76/127` |
| C2 | 中 | 待办 | 云端清单是「读-改-写」，整个过程没有串行化；退出游戏的自动同步与用户手动同步可并发，导致丢条目 | `cloud_save_service.rs:195-238`、`launch_service.rs:626` |
| C3 | 中 | 待办 | `download_file` 的临时文件名由目标路径推导（非唯一），并发/重入下载同一目标会撞同一个临时文件；出错路径不清理 | `baidu_netdisk_service.rs:796` |
| C4 | 低 | 待办 | 云同步命令层（`cloud_save_commands.rs`）**没有任何测试** | 整个文件 388 行 |

> 剩余三条同属中/低，但**后果差别很大**：C2 需要一次竞态才会丢条目，C3 大概率只表现为下载失败，C4 是覆盖缺口而非缺陷。（C5 曾是最该先修的一条 —— 一旦发生就永久卡死该游戏且把用户引向无效操作，现已修，见 §0.2。）处置顺序见 §6。

**数据完整性主链路是扎实的** —— 这部分要先说清楚，免得下面的条目造成错误印象：

- **每个存档文件都有 SHA-256**：写入版本清单（`cloud_save_service.rs:212`），下载后逐项复核（`:339`），对象以哈希命名存储（`:342` `write_object(app, &hash, &data)`）⇒ 内容寻址，重复上传/下载天然幂等。
- **下载包整体也有 SHA-256**：`package_sha256` 记录在清单里（`:211`），下载后比对（`:277-282`）。
- **压缩包读取有解压炸弹防护**：`read_zip_entry_bounded` 同时按「zip 头部声明大小」和「清单声明大小」双重拦截后才读字节（`:328-337`），并有 4 条单元测试覆盖（含「头部撒谎」的情形）。
- **恢复是事务性的**：先 `protect_current_save_version` 保护当前存档（`:385`），再 `SaveRepository::restore`，失败时 `rollback_restore`，成功才 `finalize_restore`（`:397-411`）。存档目录不会被半写。
- **凭据处理正确**：`secret_key` 用 Windows DPAPI 加密落盘（`baidu_config_repository.rs:135-168`），API 视图只暴露 `secret_key_configured: bool`，`safe_network_error` 统一剥掉 URL——因为百度强制把 token 放在查询串里（`:960-962` 有专门注释说明为什么必须这么做）。

---

## 0.1 C1 落地记录（2026-09-14）

**做法**：`remote_save_dir` / `remote_manifest_path` / `remote_package_path` 三个构造函数改为返回 `Result`，内部对 `game_key` 与 `version_id` 各做一次 `is_safe_path_segment`。校验点放在构造函数**内部**而不是各调用点，是因为调用点多达 6 处，摆在调用点迟早会漏一个。

**收敛了四份重复校验，不是三份**（审计时只数到三份，落地时又发现第四处）：

| 位置 | 原状 | 处置 |
|---|---|---|
| `cloud_save_service.rs` 三个构造函数 | 拼接前完全不校验 | 新增 `is_safe_path_segment` |
| `cloud_account_service.rs::is_valid_game_key` | 与领域层逐条等价的私有实现 | 删除，改用领域层 |
| `baidu_commands.rs::remote_body_dir` | 内联的一段等价检查 | 改用领域层 |
| `add_game_commands.rs:30` | 手写分隔符检查 —— **漏了 `.` 与 `..`** | 改用领域层 |

**一处真实的行为变化**（原计划写的是「零行为变更」，落地后修正）：`add_game_commands` 那处原先只拒绝分隔符与空串，改用领域层后**开始拒绝 `.` 与 `..`**。实际影响仅限显示名恰好为 `.`/`..` 的游戏（这类游戏的远程路径本来就会指错地方），属预期收紧。

**验证**：

- 新增 2 条守卫测试（净 +2，276 → 278），其中 `remote_paths_reject_segments_that_escape_the_game_directory` 覆盖 `../../evil` / `..` / `.` / `a/b` / `a\b` / 空串 / 全空格 / 含换行，且对三个构造函数与 `version_id` 分别断言。
- 另加 `remote_paths_still_accept_keys_with_spaces` —— 钉住「不能误伤」，因为 `derive_game_key` 会保留空格（`"black myth wukong"`），收紧成字符白名单就会打断真实存在的 key。
- **变异 ×2**：分别去掉 `version_id` 与 `game_key` 的校验，两次都精确失败在该守卫测试上；还原后回到 278 通过。
- 门禁：`cargo fmt --check` 干净、`cargo test --lib` 278 passed / 0 failed、clippy 0 error 且警告数与基线持平。

**顺带确认的事实**（决定这次能收多紧）：生产路径上 `version_id` 恒为 `Uuid::new_v4().to_string()`，`remote_package_path` 只在本地版本上被调用；`fetch_manifest` 的兜底分支虽然会从远程文件名反推 `version_id`，但那条路径不经过远程路径拼接。因此本次可以按「单一路径段」从严判定，不会误伤合法值。
---

## 0.2 C5 落地记录（2026-09-14）

**做法**：两层。启动恢复里扫掉残留的暂存目录（治本，覆盖崩溃与强杀），外加一个 RAII 守卫包住「解到暂存 → `rename` 提交」这段窗口（治运行期，让解包或提交失败当场自清，不必等重启）。

- `BodyPackageService::cleanup_interrupted_cloud_installs(games_root)`：只扫 `games_root` 第一层，匹配 `.{game_uid}.cloud-installing`，且要求中间那段确实是 uid（名字里再含 `.` 就跳过，避免把 `.cloud-installing`、`..cloud-installing` 这类退化名字当成暂存目录）。
- 目录名由 `CLOUD_INSTALL_STAGING_PREFIX` / `CLOUD_INSTALL_STAGING_SUFFIX` 两个 `pub const` 统一，创建侧（`baidu_commands`）与清理侧共用 —— 原先创建侧是硬编码的 `format!(".{local_uid}.cloud-installing")`，两边若各写一份，改一处就会静默失配。
- 调用点紧挨 `SaveRepository::recover_interrupted_restores`，沿用那里已写明的时序论证：此刻还没进入运行期、没有安装任务在跑，所以只可能清到上一次进程的残留，不存在与进行中的安装抢文件。

**守卫的一个关键细节**：`CloudInstallStagingGuard` 用 `Option<PathBuf>` 而不是「bool + 路径」—— `Some` 即「还归我管」，`rename` 成功后就地置 `None` 解除。写错这里的后果不是「删不掉残留」，而是**把刚装好的游戏本体当成暂存目录删掉**，用户看到的是「安装成功但游戏不见了」。因此专门配了一条测试钉住解除后的行为。

**报错文案修正**：原为「已有未完成的云端游戏安装暂存目录，请重启应用后重试」。这句话在修 C5 之前是错的（重启并不清理，用户照做也没用）。现在改为「若确认没有安装任务在进行，重启应用会自动清理它」—— 前半句是真凶说明（守卫还在时通常意味着确有一次安装在进行），后半句才是本次改动真正兑现的承诺。

**验证**：

- 新增 5 条测试（278 → 283）：暂存目录被清且已提交的受管目录、同前缀无关目录都不受影响；退化名字不误伤；`games_root` 缺失不算错误；守卫武装时删除、解除后保留。
- **变异 ×2**：让启动清理不删任何东西、以及让 `Drop` 变成空实现，两次都精确失败在对应测试上；还原后回到 283 通过。
- 门禁：`cargo fmt --check` 干净、`cargo test --lib` 283 passed / 0 failed、clippy 0 error 且警告数与基线持平。

**未做的事**：那条 `staging.exists()` 判据仍然是 check-then-act，两次并发安装同一游戏仍可能双双通过检查（`:1574` 的 `managed_path.exists()` 兜住覆盖，所以不会损坏已装游戏）。这是 C2 一族「云操作没有按游戏串行化」的另一面，留给 C2 一并处理，本次没有扩大范围。

---

## 1. 高优先级

### C1 云存档远程路径未做路径段校验，而 `version_id` 完全来自 IPC

**证据链**

路径构造函数直接字符串拼接，没有任何校验：

- `cloud_save_service.rs:96` `format!("{REMOTE_SAVES_ROOT}/{game_key}")`
- `cloud_save_service.rs:104` `format!("{REMOTE_SAVES_ROOT}/{game_key}/{version_id}.zip")`
- `cloud_save_service.rs:20` `const REMOTE_SAVES_ROOT: &str = "/apps/GameSaver/saves";`

`version_id` 从 IPC 到删除动作之间**没有任何一层校验**：

```
src/components/GameDetailPage.vue  deleteCloudSaveVersion(uid, versionId)
  → cloud_save_commands.rs:238  delete_cloud_save_version(state, game_uid, version_id: String)
       // 只查了 game_uid 对应哪个游戏，version_id 原样透传
  → cloud_save_service.rs:517  let remote_pkg = Self::remote_package_path(game_key, version_id);
  → :518  client.delete_file(&remote_pkg)   // 按这个路径删远程文件
```

**项目里已经有正确的工具，只是没在这里用。** `baidu_commands.rs:1843` 对本体目录做了校验：

```rust
if !is_safe_path_segment(game_key) {
    return Err("gameKey 包含不支持的远程路径字符".to_string());
}
```

而 `is_safe_path_segment`（`domain/path_safety.rs:14`）正是为这件事写的、且带 2 条测试的领域函数，注释里明确写着它的用途就是「会被拼进某个根目录之下、只占一层的名字」。全项目**生产代码只有一处调用它**（本体路径），存档路径一次都没用。

`cloud_account_service.rs:401` 还重复实现了同一份逻辑（`is_valid_game_key`，与本函数逐条等价）—— 加上 `add_game_commands.rs:30` 自己手写的一份分隔符检查，同一个判定在项目里存在**三份强度不一的拷贝**，唯独强度最弱的那条路（存档）没被覆盖。

**后果**

- `version_id` 经 IPC 传入，构造出的远程路径会离开 `/apps/GameSaver/saves/<game_key>/` 这层。字面量 `/` 会被百度按路径分隔符解释，因此可以指向该前缀之外的远程路径，而这段路径正是 `delete_file` 的入参。
- 同一族函数同时服务上传与下载 ⇒ 影响面不只删除，还包括「写到非预期远程位置」。
- 实际能删到什么，取决于百度对 `.` / `..` / 空段的解析策略，**这一点我没有环境验证**（见 §3 存疑项）。但「校验缺失」本身已在代码里成立，且作者显然认为该防——否则不会为本体路径专门加这道检查。

**建议**：让这三个函数返回 `Result<String, String>`，内部先过 `is_safe_path_segment(game_key)` 与 `is_safe_path_segment(version_id)`；同时把 `cloud_account_service.rs` 的 `is_valid_game_key` 与 `add_game_commands.rs:30` 的手写检查统一收敛到 `is_safe_path_segment`，消掉三份拷贝。

---

## 2. 中优先级

### C2 云端清单「读-改-写」无串行化，并发同步会丢版本条目

`upload_save_version` 的顺序是：拉清单 → 内存里加条目 → 传回清单（`cloud_save_service.rs:195` fetch、`:217-229` 改、`:238` save）。三步之间没有锁，`save_manifest`（`:491`）是整体覆盖写。

触发路径真实存在，且**不在同一把前端互斥里**：

- `launch_service.rs:626` 游戏退出后若开了 `auto_sync_save`，自动调 `start_upload_save_version_task`（后台线程，无 UI 互斥）
- `GameDetailPage.vue:320` 用户点「同步最新」也调同一个命令；它的 `busy` 只防本组件重复点击，管不到上面那条自动线程

两条路径传的 `version_id` **可以不同**（自动同步传本场新产生的版本，手动传用户在下拉里选的任一历史版本），所以：

| 时序 | 结果 |
|---|---|
| A 拉清单（只有 V_old） | |
| B 拉清单（只有 V_old） | |
| B 写清单（V_old + V2） | |
| A 写清单（V_old + V1） | **V2 条目丢失** |

**后果**：V2 的包仍在远端，但已不被清单引用 ⇒ 用户看不到这个云端版本，而保留策略（`:231-236` 只剪清单里的条目）也永远不会回收它 —— 变成一个查不到、也清不掉的远端垃圾。反过来，若先写的是新清单，用户会以为某个已上传的版本「不见了」。

`state.save_operations` 是项目现成的「按游戏加锁」设施（`app_state.rs:27`），但云同步的 worker（`upload_save_worker` / `restore_save_worker`）**都没有取用**；它目前只被 `confirm:` 流程用（`save_commands.rs:425`）。

**建议**：在 `start_upload_save_version_task` / `start_restore_cloud_save_task` 里按 `game_uid` 取 `save_operations` 的占位，占不到就直接返回「该游戏正在同步」。这比在服务层加锁更贴合项目既有做法。

### C3 下载临时文件名不唯一，并发或重入会撞同一个文件

`baidu_netdisk_service.rs:796`：

```rust
let temporary = target_path.with_extension("download.tmp");
```

文件名只由目标路径推导，**不是** UUID。同一目标路径的两次并发下载会写同一个临时文件，随后各自 `fs::rename(&temporary, target_path)`（`:843`）。

对比同一文件里的其它临时文件：云端清单用 `save-manifest-put-{uuid}.json`（`cloud_save_service.rs:497`）、存档包用 `gamesaver-save-dl-{uuid}`（`:251`）、账号清单用 `.cloud-account-download-{uuid}.json`（`cloud_account_service.rs:248`）—— 都是唯一名。**只有 `download_file` 这个最底层的公共实现例外。**

清理覆盖也不完整：取消（`:823-825`）与大小不匹配（`:837`）两条路径**有**清理，但**响应体读取中途出错**（`:804-806` 的 `read` 失败）走 `?` 直接返回，**不清理**，于是留下 `.download.tmp`。调用方只清理自己那份 `target_path`，而且是 `let _ =` 静默忽略（如 `cloud_account_service.rs:264`）。

**后果**：并发场景下可能读到对方写到一半的字节（不过下载路径随后都有大小与哈希校验兜底，因此更可能是「下载失败」而不是「静默坏数据」）；失败后残留临时文件，长期累积。

**建议**：临时名加 UUID；把「删除临时文件」放进一个 RAII guard（项目里已有 `TemporaryFileGuard`，`cloud_save_service.rs:745` 的 Drop 实现可复用）。

---

### C5 云端安装暂存目录无清理，中断后该游戏永久装不上（中优先级）

`baidu_commands.rs:1538` 用固定名字建暂存目录，紧接一条「存在就拒绝」的检查：

```rust
let staging = games_root.join(format!(".{local_uid}.cloud-installing"));
if staging.exists() {
    return Err("已有未完成的云端游戏安装暂存目录，请重启应用后重试".to_string());
}
```

后续 `extract_package_with_known_hash` 把包解到 `staging`（`:1550`），成功后再 `fs::rename(&staging, &managed_path)` 原子提交（`:1577`）。**原子提交这一半做得对**；问题在暂存目录的善后：

- 解包中途失败（`:1550-1568` 的 `?`）**不清理** `staging`。
- 取消只有在**解包已经成功返回之后**才被检查并清理（`:1569-1572`）。
- 启动恢复里没有对应清理：`lib.rs:76` 的 `cleanup_temporary_packages` 只扫 `body-packages` 缓存目录（`game_body_package_service.rs:67-99`，只认 `.tmp` / `.download-` / `.manifest-` 三类名字，**且根目录就不是 games_root**）；`lib.rs:127` 的 `cleanup_orphan_packages` 只删未被引用的 `.zip`；`lib.rs:82` 的 `cleanup_removed_restore_archives` 只管本体恢复副本。三处都不认 `.cloud-installing`。

**后果**：任何一次「云端安装游戏时应用被关闭 / 崩溃 / 解包失败」都会留下 `.{uid}.cloud-installing`，此后**该游戏的云端安装被永久挡住**，而错误信息让用户去「重启应用」—— 重启不解决任何问题。用户只能自己进 `E:\GameSaverGames\` 手工删掉那个隐藏目录。这是「提示与事实不符 + 需要手工干预」的组合，比单纯的失败更糟。

**附带**：`staging.exists()` 是 check-then-act，两次并发安装同一 `local_uid` 会双双通过检查，然后一方在 `:1577` 的 `rename` 上失败（`:1574` 的 `managed_path.exists()` 兜住了覆盖，所以不会损坏已装游戏——**兜底是对的**，但它拦不住暂存目录被留下）。

**建议**：在 `lib.rs` 的启动恢复里加一条「清理 games_root 下形如 `.{uid}.cloud-installing` 的目录」（用与 `cleanup_temporary_packages` 相同的模式匹配思路），并把报错文案改成与真实处置一致。更好的做法是给这个暂存目录也加 RAII guard。

---

## 3. 低优先级

### C4 云同步命令层零测试

| 文件 | 测试数 |
|---|---|
| `cloud_save_service.rs` | 12 |
| `cloud_account_service.rs` | 8 |
| `baidu_netdisk_service.rs` | 8 |
| `cloud_manifest_service.rs` | 6 |
| `baidu_commands.rs` | 3 |
| `cloud_account_commands.rs` | 3 |
| `baidu_config_repository.rs` | 1 |
| **`cloud_save_commands.rs`** | **0** |

`cloud_save_commands.rs` 承载上传/恢复任务的编排（`begin_sync` / `finish_sync` / `upload_save_worker` / `restore_save_worker`），却是唯一没有测试的云文件。C2 那条并发缺陷正好落在这层 —— 若这里有测试，加锁行为会被钉住。

---

## 4. 已检查且判定为干净的部分
写下来是为了让读者知道覆盖面，而不是「没查」：

- **凭据**：DPAPI 加密落盘；API 视图不外泄 `secret_key`；`safe_network_error` 统一剥离 URL（百度 OpenAPI 强制 token 进查询串，这是必要的对策，代码里有注释解释）。**未发现凭据泄露路径。**
- **压缩包路径穿越**：`normalize_package_relative_path`（`:682`）拒绝绝对路径与 `..`，且 `validate_package_data_path` 强制 `data/` 前缀；每个 `data/` 项还必须在清单里声明（`:322-324`），未声明即报错。
- **解压炸弹**：`read_zip_entry_bounded` 双重大小拦截 + 上限，4 条测试覆盖。
- **下载完整性**：包级 SHA-256 比对（`:277`）+ 逐文件 SHA-256 比对（`:339`）+ 内容寻址写入（`:342`）。
- **恢复原子性**：`protect → restore → rollback/finalize` 闭环（`:385-411`）。
- **token 中途过期**：`request_json_value`（`:894`）与 `send_raw`（`:932`）都会在鉴权失败时刷新 token 并重放一次；`send_with_retry` 对 408/429/5xx 有 3 次退避重试。
- **解压后崩溃**：文件以哈希命名写入 `write_object`，中途失败不会覆盖已有对象。

---

## 5. 存疑项（未能验证，需环境确认）

1. **C1 的实际可达伤害范围**：百度网盘服务端对 `..`、`.`、空路径段的解析/归一化策略未知，因此「最坏能删到 `/apps/GameSaver/saves/` 之外的什么」无法在本机判定。需要真实网盘环境或百度接口文档确认。
2. **`game_key` 能否带 `/` 抵达存档路径**：`add_game_commands.rs:30` 在本地添加游戏时**会**拒绝含分隔符的 key；但云端导入路径（`baidu_commands.rs:1591` `derive_game_key(&catalog.game_key)`）只做小写与空白折叠，**不拒绝分隔符**。该 remote catalog 的来源与可信度未追到底，因此这条只算可疑，未计入 C1 的确定部分。
3. **并发上传同一版本是否可能**：自动同步与手动同步若恰好都指向同一 `version_id`，C2 的丢条目不会发生（retain-then-push 对同一 id 幂等）。C2 的成立依赖两条路径取到不同版本，这一点在 UI 上是可能的（手动选历史版本），但我没有实测复现。

---

## 6. 建议的处置顺序

| 批次 | 内容 | 性质 |
|---|---|---|
| 第 1 批 | ~~**C1** 三个路径构造函数补 `is_safe_path_segment`，并收敛三份重复校验~~ **已完成（见 §0.1）** | 纯加固 |
| 第 2 批 | ~~**C5** 启动恢复里清理 `.cloud-installing` 暂存目录，并修正「请重启应用」的文案~~ **已完成（见 §0.2）** | 小改，直接消除需要手工干预的卡死 |
| 第 3 批 | **C2** 云同步命令按 `game_uid` 取 `save_operations` 占位 | 小改，需决定「已在同步」时的前端提示文案 |
| 第 4 批 | **C3** 临时文件名加 UUID + RAII 清理 | 小改 |
| 第 5 批 | **C4** 为 `cloud_save_commands.rs` 补编排层测试（可同时钉住 C2 的加锁行为） | 补验证 |

**优先级说明**：C5 排在 C2 之前，是因为它的后果最重且成本极低 —— C2 需要一次竞态才发生，而 C5 一旦发生就**永久**卡住该游戏，且错误信息把用户引向无效操作。C1 虽然级别最高，但它的实际可达伤害取决于百度服务端行为（见 §5 存疑项 1），而补上校验的成本接近于零，所以仍排第一。
