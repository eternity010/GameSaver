# GameSaver 代码审查报告

> 日期：2026-09-10
> 范围：`src/`（Vue 3 + TS，4053 行）、`src-tauri/src/`（Rust，23916 行）、配置文件、`docs/`
> 方法：全文静态阅读 + 实际编译与静态检查验证（未修改任何代码）

---

## 0. 结论摘要

**实测验证结果**（全部在当前工作区实际执行）：

| 检查项 | 命令 | 结果 |
| --- | --- | --- |
| 前端类型检查 | `vue-tsc --noEmit` | ✅ 通过，0 错误 |
| Rust 编译 | `cargo check` | ✅ 通过，0 错误 0 警告 |
| Rust 静态检查 | `cargo clippy --all-targets` | ⚠️ 31 条警告（22 条在 lib） |
| 单元测试规模 | `grep -c '#\[test\]'` | 151 个测试，分布在 30 个模块 |

**整体判断**：这是一个架构分层清晰、工程纪律明显高于同类个人项目的代码库。领域模型以 `Game` 为聚合根、存档用内容寻址（CAS）、本体更新用 `journal + swap + rollback`、ETW 有三级降级链——这些设计都经得起推敲，`docs/` 里的设计文档与代码基本对得上。测试也写得认真（`task_repository.rs`、`game_library_service.rs` 的测试覆盖了中断恢复、缺失本体、路径校验等边界）。

**共发现三类系统性风险**（另有一条经实测否证、已撤回），按严重程度排列：

1. **`store.json` 的写入存在一个崩溃窗口，会静默丢失整个游戏库**（见 P1-1）—— ✅ **已于 2026-09-10 修复**（新增 `repositories/store_file.rs`，四个单文件 JSON 存储统一收口并具备崩溃恢复）
2. **约 20 处「克隆 → 修改 → 整体覆盖写回」的模式，存在丢失更新竞态**（见 P1-2）—— ✅ **已于 2026-09-10 修复**（新增 `AppState::with_store_mut`，21 处调用点全部收口；顺带发现并修复了游戏库迁移后**内存态从未回写**的独立缺陷）
3. **百度网盘 token 只在客户端构造时刷新，长任务中途过期即整体失败且不可重试**（见 P1-3）—— ✅ **已于 2026-09-10 修复**（token 改为可在线刷新；鉴权失败自动刷新并重放，同时覆盖 HTTP 401 与百度 `errno`）
4. ~~目录联结（junction）可绕过 `safe_join` 的符号链接检查~~（见 P1-4）—— ❌ **实测否证，已撤回**（Rust 的 `FileType::is_symlink()` 已识别 `IO_REPARSE_TAG_MOUNT_POINT`）

---

## 1. 项目概览

| 项目 | 说明 |
| --- | --- |
| 名称 / 版本 | GameSaver / 1.0.18 |
| 形态 | Windows 桌面应用（Tauri 2） |
| 前端 | Vue 3.5 + TypeScript 6.0 + Vite 8，无 vue-router、无 Pinia |
| 后端 | Rust 2021，Windows API 直接调用（`windows-sys` 0.61） |
| 数据 | 纯 JSON 落盘，无 SQLite |
| 云端 | 百度网盘 REST API（OAuth + 分片上传） |
| 打包 | ZIP（优先外部 `bin/7z.exe`，回退 Rust `zip` crate） |
| 关键依赖 | `reqwest`(blocking/rustls)、`sha2`、`image`(png)、`zip`、`walkdir` |
| 打包特性 | `lto=true`、`codegen-units=1`、`panic="abort"`、`strip=true` |

**注意**：`panic = "abort"`（`Cargo.toml:46`）意味着任何一次 panic 都会直接终止进程且不展开栈——这让下文的 P1-1、P2-9 影响被放大（没有 unwind 清理的机会）。

---

## 2. 目录结构与模块职责

### 2.1 前端 `src/`

| 文件 | 行数 | 职责 |
| --- | --- | --- |
| `App.vue` | 625 | 应用外壳。自维护 `activePage` 做页面切换，同时充当全局状态中心（games / search / selectedGame / coverUrls / 各安装进度） |
| `components/GameDetailPage.vue` | 1192 | 单游戏详情。混合了封面裁剪编辑器、云存档抽屉、本体包、存档版本、启动检查、任务轮询、事件监听 |
| `components/AddGameWizard.vue` | 540 | 添加游戏向导（目录/EXE 选择 → 复制 → 存档学习） |
| `components/GameStorePage.vue` | 345 | 云端游戏商店（列表、分页、安装、删除版本） |
| `components/PlatformSettings.vue` | 309 | 全局设置（库路径迁移、百度账号、同步策略） |
| `components/TransferCenter.vue` | 241 | 后台传输任务列表与重试 |
| `api.ts` | 499 | 全部 `invoke` 命令的类型化封装 + 统一错误上报 |
| `domain/game.ts` | 247 | 与 Rust 领域模型对应的 TS 类型 + 默认排除规则 |
| `error_reporting.ts` | 43 | 全局 `error` / `unhandledrejection` 钩子（有 `installed` 幂等守卫） |
| `main.ts` / `style.css` | 12 | 入口与样式 |

**通信方式**：props 下行 + emit 上行，子组件也会直接 `import` api 调用后端（如 `GameDetailPage` 自己调 `launchGame`/`getTask`）。

### 2.2 后端 `src-tauri/src/`（四层）

```
lib.rs (234)           入口装配：单实例检查 → 启动期恢复/清理 → 注册 71 个 command
├── app_state.rs (58)  全局状态：8 个 Mutex 字段
├── commands/ (16 模块, 约 8000 行)   仅做参数校验、状态编排、任务派发
├── services/ (23 模块, 约 14000 行)  真正的业务逻辑
│   └── learning/      ETW 捕获三件套（etw_capture / native_etw / transactions）
├── repositories/ (6 模块)            持久化，全部 JSON + 原子替换
└── domain/ (7 模块)                  领域模型与状态枚举
```

**规模最大的 8 个文件**（也就是复杂度集中处）：

| 文件 | 行数 |
| --- | --- |
| `services/save_learning_service.rs` | 2463 |
| `commands/baidu_commands.rs` | 1947 |
| `repositories/save_repository.rs` | 1559 |
| `services/game_body_package_service.rs` | 1292 |
| `commands/game_body_commands.rs` | 1151 |
| `services/baidu_netdisk_service.rs` | 1131 |
| `services/learning/etw_capture.rs` | 1121 |
| `services/cloud_manifest_service.rs` | 1026 |

**磁盘布局**（`AppState::library_root_path` + 子目录，`app_state.rs:40-57`）：

```
<data_dir>/                      应用数据（app_data_dir）
├── store.json                   游戏库、存档配置、版本元数据（单一真相源）
├── tasks.json                   后台任务记录
├── library-config.json          库根目录配置
├── logs/gamesaver.log           诊断日志（5MB 轮转）
├── cloud-manifest-cache/        云端清单与封面缓存
├── baidu-netdisk-token.json     网盘 token（明文，见 P2-5）
└── <library_root>/              可迁移的库根目录
    ├── games/<uid>/             受管游戏本体
    ├── body-packages/<uid>/<ver>.zip
    └── saves/.gamesaver-repository/objects/sha256/<aa>/<hash>   CAS 对象
```

---

## 3. 关键实现逻辑

### 3.1 单实例与管理员重启（`services/instance_service.rs`）

三段式：先看命令行是否带 `--gamesaver-admin-relaunch=<pid>`，若有则先 `WaitForSingleObject` 等旧进程退出（最多 10 秒）；再用 `CreateToolhelp32Snapshot` 按 **exe 文件名**（而非路径）查找同名进程，找到就 `ShowWindow(SW_RESTORE)` + `SetForegroundWindow` 抢焦点并退出；最后才 `CreateMutexW("Local\\GameSaverNext.SingleInstance")` 建锁，用 `GetLastError() == ERROR_ALREADY_EXISTS` 兜底。

这是一个完整、经过考虑的实现——管理员重启时新旧进程的竞态被显式处理了。

### 3.2 存档仓库：内容寻址（`repositories/save_repository.rs`）

- 对象路径：`objects/sha256/<hash[..2]>/<hash>`（`save_repository.rs:1045`）
- 写入：`.{hash}.tmp-UUID` 临时文件 → `sync_all` → `rename` 原子提交（`:302-314`）
- 读取后重新校验 SHA（`:269-272`），提交时用哈希复用已有对象
- 恢复：`staging` 暂存物化 → `rollback` 目录备份被覆盖文件 → `rename` 提交，失败走 `rollback_group`（`:440-597`）
- 版本元数据 `SaveVersion { version_id, files: Vec<SaveFileEntry> }` 存在 `store.json`，`SaveFileEntry` 记 `object_hash/size/relative_path/root_type/deleted/mtime_ms`

**设计亮点**：`SafeJoin` 逐段拒绝 `..` 和符号链接（`:818-843`）；`read_stable_file` 用前后 mtime+size 比对防 TOCTOU（`:1032-1043`）。

### 3.3 ETW 三级降级（`services/learning/`）

| 层级 | 机制 | 触发条件 |
| --- | --- | --- |
| 1（首选） | `logman create trace` + `OpenTraceW`/`ProcessTrace` 解析 ETL，`TdhGetProperty` 按名取 `FileName`/`FileObject` | 管理员权限可用 |
| 2（回退） | 原生 ETL 解析失败时改用 `tracerpt` 转 CSV 再解析 | ETL 解码失败 |
| 3（回退） | 完全放弃 ETW，改用保存前后快照差异（`baseline` vs `final`） | ETW 无法启动或解析后事件为空 |

提供者固定为 `Microsoft-Windows-Kernel-File`（GUID `edd08927-...`，`native_etw.rs:22`），关键词掩码 `0x1EB0`（`etw_capture.rs:12`）。`QueryDosDeviceW` 把 `\Device\HarddiskVolumeN` 映射回盘符。

**事务归并**（`transactions.rs`）：按时间戳排序，用「2 秒窗口 + 同 PID + 同目录或同 FileObject」把文件事件并成 `TransactionGroup`，再按 `(pid, 路径, 操作)` 去重。

**证据分级**（`save_learning_service.rs:1492-1510`）：

- **Strong**（88/92 分）：`(写 && (关闭 || 重命名)) || 重命名`
- **Review**（68/75 分）：只有写/关闭/重命名/创建，缺完整提交证据
- 其余命中候选但无操作证据的走 **Review**（58/65 分）

### 3.4 本体更新：journal + swap + rollback（`services/game_body_update_service.rs`）

流程：`validate_source`（含磁盘空间预检）→ `copy_to_staging` → 提交当前存档 → 原子写 `*.update.json` journal → `swap`（`rename(managed→archive)` 再 `rename(staging→managed)`）→ 恢复存档 → 持久化。启动时 `recover_pending_updates` 扫描残留 journal，对照 `store.body_versions` 里已登记的 `archive_path` 判定「已提交」还是「未提交」，据此补完或回滚。

这是本项目最扎实的一段工程实现。

### 3.5 任务系统与中断恢复（`services/task_service.rs` + `repositories/task_repository.rs`）

- 任务全员走 `AppTask`，状态 `Pending/Running/Success/Failed/Cancelled/Interrupted`
- **中断恢复做对了**：`TaskRepository::load` 把落盘时还是 `Pending/Running` 的任务改写为 `Interrupted` 并附上可重试说明（`:41-45`），有对应单元测试
- 历史裁剪：只保留最近 100 条已结束任务（`trim`，`:72-83`）
- **重试机制是「后端写参数、前端重放」**：`TaskService::set_retry` 把操作类型和远端路径写进任务，前端 `TransferCenter.vue:83-101` 读 `task.retry` 重新调用对应命令。设计自洽，没有死数据

### 3.6 封面走自定义协议（`cover_protocol.rs` + `api.ts:207-218`）

`gamesaver-cover://game/<uid>` 由 Rust 侧 `register_uri_scheme_protocol` 处理，直接返回图片字节 + 正确 MIME（按魔数嗅探）。相比 base64 的好处：不把大图塞进响应式状态、可走浏览器缓存（`max-age=604800`）与 `loading="lazy"`。URL 带 `?v=<cacheKey>` 做缓存失效。

---

## 4. 问题清单

> 行号基于 2026-09-10 修复前的快照。已修复条目的行号会与当前代码有偏移；
> `save_learning_service.rs` 等未改动文件的行号仍然有效。

### P1 — 高影响，建议优先处理

#### P1-1 `store.json` 写回存在崩溃窗口，会静默清空整个游戏库 —— ✅ 已修复（2026-09-10）

> **修复说明**：新增 `repositories/store_file.rs`，把原先四份逐字复制的落盘逻辑收口，并提供
> `load_with_recovery(target, label, is_valid)`：主文件缺失时自动扫描 `.tmp-*` / `.bak-*` 残留，
> 按修改时间倒序（同刻优先临时文件）逐个校验，命中则原子写回主文件后继续启动。
> 残留全部不可解析时返回 `Err` 而不返回空数据——不再把「读不出来」伪装成「首次运行」。
> 改动文件：`store_file.rs`（新增）、`game_repository.rs`、`task_repository.rs`、
> `baidu_config_repository.rs`、`library_config_repository.rs`、`repositories/mod.rs`。
> 验证：`cargo test` 161 passed（新增 10 个），clippy 新模块 0 警告。
>
> 原问题描述保留如下。

`repositories/game_repository.rs:101-105`：

```rust
let had_target = target.exists();
if had_target {
    fs::rename(target, &backup)?;     // ← 旧文件已改名，target 此刻不存在
}
if let Err(err) = fs::rename(&temp, target) { ... }
```

两次 `rename` 之间若进程被杀 / 断电（注意 `panic = "abort"`，panic 也直接终止），`store.json` 不存在、`.store.json.bak-<uuid>` 孤立残留。而 `load` 的入口是：

```rust
// game_repository.rs:27
if !path.exists() { return Ok(AppStore::default()); }
```

→ **下次启动得到一个空库，游戏列表、存档配置、版本元数据全部消失，且没有任何报错**。备份文件就在旁边，但没有任何代码去发现并恢复它。

`repositories/task_repository.rs:85-120` 是同一份代码的拷贝，行为相同（后果轻一些，只丢任务历史）。

**修法**：`load` 时若目标不存在但存在同名 `.bak-*` 残留，自动从中恢复一份最新的；或改用 `ReplaceFileW` / 先写临时再 `MoveFileEx` 带 `MOVEFILE_REPLACE_EXISTING` 的单步替换。

#### P1-2 约 20 处「克隆 → 修改 → 整体覆盖」存在丢失更新 —— ✅ 已修复（2026-09-10）

典型形态（`commands/game_commands.rs:60-78`）：

```rust
let mut candidate = state.store.lock()?.clone();   // 第 64 行：锁在这里释放
let game = candidate.games.iter_mut().find(...)?;
game.display_name = new_name.to_string();          // 中间可能做校验、文件 IO
GameRepository::persist(&app, &candidate)?;
*state.store.lock()? = candidate;                  // 第 78 行：重新加锁，整体覆盖
```

第 64 行与第 78 行之间**没有持有锁**，另一个线程在这段时间的修改会在第 78 行被整块丢弃。

实测统计（`*state.store.lock() = candidate` 写回模式）：`baidu_commands.rs` 5 处、`game_body_commands.rs` 4 处、`cloud_save_service.rs` 2 处、`save_version_commands.rs` 2 处、`game_commands.rs` 2 处、`launch_service.rs` 1 处、`cloud_account_commands.rs` 1 处；另有 `save_commands.rs`(4)、`add_game_commands.rs`、`library_service.rs`、`game_repository.rs` 使用 `store.clone()` 变体。

**最容易命中的组合**：游戏退出时 `run_game_session` 提交存档版本（要写 `body_versions`/`save_versions`）与用户同时点「重命名游戏」或「删除版本」。`AppState` 里有 `running_games` / `save_operations` / `library_migration` 三个互斥标志，但它们只覆盖了部分路径，不构成通用的写保护。

**修法**：加一个 `with_store_mut(state, |store| -> Result<T> { ... })` 帮助函数，让 guard 覆盖整个「读-改-写」，而不是 `clone` 出作用域。

**已落地的修复（2026-09-10）**

新增 `AppState::with_store_mut(&self, mutate)`（`app_state.rs`）：一次加锁 → 克隆得 `candidate` → 在**锁内**执行闭包（闭包内部自行 `GameRepository::persist` 落盘）→ 返回 `Ok` 才提交回内存，返回 `Err` 则整体丢弃。配套 3 个单测钉住契约（提交 / 丢弃 / **闭包执行期间锁确实被持有**）。

共改造 21 处：

| 文件 | 处数 | 说明 |
| --- | --- | --- |
| `commands/baidu_commands.rs` | 5 | 本体包下载落库、清空/更新上传记录、云端安装、远端版本对账 |
| `commands/game_body_commands.rs` | 4 | 卸载标记、删除本体包、打包落库、本体更新提交 |
| `commands/save_version_commands.rs` | 3 | 恢复前保护、恢复结果落库、删除版本 + 清单更新 |
| `services/cloud_save_service.rs` | 3 | 导入版本落库、云端恢复结果、恢复前保护 |
| `commands/game_commands.rs` | 2 | 重命名、保存封面 |
| `commands/cloud_account_commands.rs` | 1 | 云端档案合并（`merge_profile` 改为接收 `&mut AppStore`） |
| `services/launch_service.rs` | 1 | 游戏退出后提交存档版本 |
| `commands/library_commands.rs` | 2 | 见下方「顺带修复」 |

**两个必须说明的实现细节**

1. **耗时/网络 IO 被移出锁外。** 若直接把整段逻辑塞进闭包，`install_cloud_game_task` 的封面下载、`library_commands` 的文件复制都会在持锁状态下执行，长时间阻塞 `list_games` 等只读命令。这两处把网络/文件操作用作闭包**之前**完成，闭包内只做内存改写 + 落盘。
2. **`games_root()` / `library_root_path()` 一律提到闭包外取。** 它们锁的是 `library_root` 而非 `store`；放在闭包内会形成 `store → library_root` 的加锁顺序，埋下与其他路径相反的锁序死锁风险。

**顺带发现并修复的独立缺陷（原报告未收录）**

`commands/library_commands.rs::migrate_library` 迁移成功后**只更新了 `library_root`，从未把迁移后的配置写回 `state.store`**。磁盘上的 `store.json` 已是新路径，内存里仍是旧 `managed_path`。后果比"丢失更新"更直接：此后任意一次读改写都会把旧路径覆盖回磁盘，等于**静默回退整次迁移**。

修法：新增 `LibraryService::rewrite_paths_in_place`，在**一次加锁内基于当前 store** 改写路径后落盘（而不是用迁移开始时那份可能已过时几分钟的快照），配置保存失败的回滚同理反向改写。

> 残留（未处理，属 P2 级）：迁移期间若恰好有新游戏加入，其文件仍写在旧库目录，会被收尾的 `cleanup_source` 删掉。要彻底关闭需在迁移期间阻止新增游戏（现有 `library_migration` 标志只在"开始迁移"时做了互斥检查）。

#### P1-3 网盘 token 只在客户端构造时刷新，长任务中途过期即整体失败 —— ✅ 已修复（2026-09-10）

**原问题**（行号基于修复前快照）：

- `refresh_access_token` 全项目只有 **1 个调用点**：`load_from_app_data_with_credentials`（第 203 行），也就是**构造客户端时**
- 每次请求取的是实例内冻结的 `self.token.access_token`
- 鉴权失败没有任何补救路径：401 既不重试（`is_retryable_status` 只认 408/429/5xx），也没有「刷新 token 后重放」

后果：上传一个几十 GB 的本体包（30 分钟以上）时若 access_token 到期，剩余所有分片请求会连续失败，整个任务报废，用户只能从零重来。这也与设计文档「token 过期不会触发远程请求」的承诺不符——实现只保证「任务启动那一刻 token 是新鲜的」。

**修复**（`services/baidu_netdisk_service.rs`）：

1. `BaiduNetdiskClient.token` 由 `BaiduToken` 改为 `Mutex<BaiduToken>`，新增 `app_data_dir` 与 `refresh_context: Option<RefreshContext>`（AppKey/SecretKey）。刷新凭据结构体刻意**不实现 `Debug`**，避免密钥被格式化进日志或错误信息。
2. 新增 `refresh_token(force)`：全局刷新锁串行化；持锁后**先重读磁盘**，别的进程已刷新并落盘时直接沿用，避免重复刷新；无凭据且磁盘无更新时返回 `false`。
3. 新增统一入口 `request_json_value` / `request_json` / `send_raw`，取代原先散落的 `send_with_retry` + `parse_json` 组合（`parse_json` 随之删除）。
4. 鉴权失败判定 `is_auth_failure` 同时覆盖 **HTTP 401** 与百度在 **HTTP 200 下返回的鉴权类 `errno`**（`-6` / `110` / `111`）以及 OAuth 的 `invalid_token` / `expired_token`。只判状态码会漏掉百度最常见的失效表现，那样等于修了个空气。
5. 命中鉴权失败时强制刷新一次并**重放**该请求（每个请求最多重放一次，不会无限循环）。
6. 分片上传的三个并发线程共享 `&self`，`Mutex` 化后仍满足 `Sync`；长任务中途过期可由任一线程触发刷新，其余线程自动取到新 token。
7. 顺带修正 `load_from_app_data` 路径：即使没有 AppKey/SecretKey，也会在刷新前重读磁盘，能拿到其他进程写入的新 token。

**验证**：新增 3 个单测——`is_auth_failure` 覆盖 HTTP 与 errno 各分支且不误伤业务错误（`errno: 0` / `-9`）；无凭据时报告无法刷新；**模拟另一进程刷新落盘后，本进程能取到新 token**。

#### P1-4 ~~目录联结（junction）可绕过 `safe_join` 的链接检查~~ —— ❌ 结论有误，已撤回（2026-09-10）

**原始判断**：`repositories/save_repository.rs:828-841` 用 `FileType::is_symlink()` 判断，而 Windows 目录联结（`mklink /J`，reparse tag 为 `IO_REPARSE_TAG_MOUNT_POINT`）在该 API 下返回 false，因此路径校验会被放行。

**实测否证**（`rustc 1.96.0`，项目无 `rust-toolchain` 固定，与实际构建环境一致）：

```
real_dir              is_symlink=false is_dir=true  attrs=0x00000010 reparse_bit=false
junction (mklink /J)  is_symlink=true  is_dir=false attrs=0x00000410 reparse_bit=true
symlink_dir           is_symlink=true  is_dir=false attrs=0x00000410 reparse_bit=true
```

Rust 标准库在 Windows 上的 `FileType::is_symlink()` **已经覆盖 `IO_REPARSE_TAG_MOUNT_POINT`**，junction 返回 `true`。`safe_join` 以及三处 `WalkDir` 的符号链接检查（`add_game_service.rs:81`、`game_body_package_service.rs:934`、`game_body_update_service.rs:193`）**均未失效**，不存在越界写入。**无需修改，本条撤回。**

附带发现（低影响，不作为问题）：junction 条目的 `is_dir()` 返回 `false`（Rust 对 reparse point 一律不认作目录）。`game_library_service.rs:92-98` 的 `root.join(value).is_dir()` 因此会把 junction 目录判为不可访问——方向是**更保守**（拒绝而非放行），不构成安全缺陷。

---

### P2 — 中等影响

> 共 15 条，截至 2026-09-10：**P2-14 已随 S5 顺带解决**（两处轮询合并为唯一任务流）、**P2-2 已随存档管理审查 V6 解决**（解压改为先校验后读取），其余 13 条未处理。

| 编号 | 位置 | 问题 |
| --- | --- | --- |
| P2-1 | `services/game_body_package_service.rs`（打包入口 `:114`） | 打包**没有磁盘空间预检**。`add_game_service.rs:104` 和 `game_body_update_service.rs:210` 都有 `ensure_available_space`，唯独打包没有——而打包恰恰要额外写出一份与游戏同体积的 ZIP。磁盘满时 7z 写坏 `.tmp` 包再报错 |
| P2-2 | ~~`services/cloud_save_service.rs:317-323`~~ | ✅ **已解决（2026-09-10，随存档管理审查 V6）**：抽出 `read_zip_entry_bounded`，**校验全部前移到读取之前** —— zip 头部声明值先与版本清单比对（不一致直接拒绝、一个字节都不读），再叠加一道与清单无关的硬上限（数据条目 1 GiB / `meta.json` 32 MiB，后者原先也是裸 `read_to_end`），最后用 `Read::take(declared + 1)` 钉死读取量。本体包（`game_body_package_service.rs`）那条解压路径不属本条：它是**流式写盘 + 1 MiB 缓冲 + 清单长度上限**，不在内存里展开 |
| P2-3 | `commands/baidu_commands.rs:1165-1171`、`services/cloud_save_service.rs:507` | 远端删除**非原子**：先 `delete_file` 再重建清单。若清单重写失败，文件已删而清单仍引用它。`cloud_save_service.rs:507` 更是 `let _ = client.delete_file(...)` 直接吞掉失败，导致清单剔除版本但远端残留孤立 ZIP |
| P2-4 | `cover_protocol.rs:49-53` | 封面路径只拒绝绝对路径，**不拒绝 `..`**。`display_path` 若被污染（store 被改 / 数据异常），可读出库目录之外的文件。项目其他地方（`safe_join`、`scope_is_accessible`）都做了 `..` 检查，此处漏了 |
| P2-5 | `cover_protocol.rs:182-183` | `percent_decode` 把 `+` 解码成空格（表单语义），但这是**路径**上下文，`+` 应是字面量。当前前端用 `encodeURIComponent` 会编码成 `%2B` 所以不触发，属潜在隐患 |
| P2-6 | `services/baidu_netdisk_service.rs:885` | token **明文落盘**（`baidu-netdisk-token.json`），而 AppKey/SecretKey 在 `repositories/baidu_config_repository.rs:163-196` 是 DPAPI 加密的。同一份配置里两种存储策略不一致。另外 `logging.rs` 没有敏感信息脱敏层 |
| P2-7 | `services/task_service.rs:56-72` | `TaskService::update`（进度更新）**只改内存不落盘**，只有 `create`/`finish`/`cancel`/`set_retry` 才持久化。崩溃时任务进度丢失（状态本身能被 `Interrupted` 兜住，可接受，但要知道这个设计） |
| P2-8 | `save_repository.rs:18-20` | `REPOSITORY_LOCK` 是全局 `Mutex<()>`，**所有 commit/restore/GC 串行**；而 `PENDING_OBJECTS` 引用计数的增减时机与 GC 读取不在同一个锁边界内，理论上存在「回收正在提交的对象」的窗口 |
| P2-9 | `game_body_update_service.rs:291/297`、`:316` | 目录 `rename` 在 Windows 上遇到**目标非空或文件被占用**即失败，无等待/重试；`rollback` 也依赖 `rename`，文件被锁时可能回滚失败，留下半更新状态 |
| P2-10 | `domain/learning.rs:9-15` | `LearningStatus::Finished` / `Cancelled` **全项目零赋值**（已 grep 验证），会话状态写入后永远是 `Capturing`。学习会话列表若依赖它做筛选会失效 |
| P2-11 | `save_learning_service.rs:561-566` | `wait_for_process_tracker` 只等 **2 秒**就继续，追踪线程可能仍在跑。若该线程尚未收敛，进程树扩展不完整，导致分析漏文件、存档提交过早 |
| P2-12 | `save_learning_service.rs:205-213`（打包清单）、`:818-843` | 清单内相对路径以 `-` 或 `@` 开头时，7z 会把它当开关或嵌套列表文件（参数注入）。已检查 `\r\n` 但未拦 `-`/`@` |
| P2-13 | `src/App.vue:28, 530, 540` | 游戏库与游戏商店**共用同一个 `search` ref**，切页不重置。在库里搜完切到商店会立刻带着旧关键词触发商店搜索（`watch` 在 `App.vue:191-199`） |
| P2-14 | ~~`src/App.vue:312-324` + `src/components/TransferCenter.vue:44-67`~~ | ✅ **已解决（2026-09-10，随 S5）**：两处轮询合并为 `src/taskFeed.ts` 唯一任务流，根组件那份自续期、永不停止的常驻轮询已删除；轮询降级为活跃 2s / 空闲 10s 兜底 |
| P2-15 | `src/App.vue:453, 479` | `cloudInstallTimer` 被 `watchCloudInstall` 与 `watchCloudDeletion` 共用同一个 `let`，互相覆盖时前一个 `setTimeout` 句柄丢失、无法 `clearTimeout`（目前靠 `cloudInstallUid` 单锁抑制并发，属侥幸） |

---

### P3 — 低影响 / 整洁性

> 截至 2026-09-10 **全部未处理**。其中 `repositories/` 相关行号已因 P1-1 的修复发生偏移
> （`baidu_config_repository.rs` 的 `CryptProtectData` 两条从 `:176/:218` 变为 `:148/:190`）；
> P1-1 顺带消除了「四份 `atomic_replace` 重复实现」这一项。

**Clippy 的 31 条警告**（`cargo clippy --lib` 实测，22 条在 lib）：

| 类型 | 位置 |
| --- | --- |
| `if` 分支内容完全相同 | `save_learning_service.rs:539`、`:865` |
| 布尔表达式可简化 | `save_learning_service.rs:1492`、`cloud_account_service.rs:413` |
| `impl` 可以 derive | `domain/game.rs:13/28/44`、`domain/save_profile.rs:24` |
| 函数参数过多（8/7） | `game_body_package_service.rs:114/151/316/550`、`game_commands.rs:135/286` |
| 连续 `str::replace` 可合并 | `logging.rs:54`、`cloud_manifest_service.rs:144`、`etw_capture.rs:836/841` |
| 可改用 `sort_by_key` / `div_ceil` / `is_multiple_of` | `save_repository.rs:115`、`etw_capture.rs:777`、`baidu_commands.rs:233`、`native_etw.rs:395` |
| 多余的 `PathBuf::from` / 不必要的 `&` / 多余 cast / `.clone()` | `lib.rs:148`、`game_body_update_service.rs:179`、`etw_capture.rs:657`、`cloud_save_commands.rs:345` |
| `CryptProtectData` 不需 `&mut` | `baidu_config_repository.rs:176/218` |
| 其它 | `admin_commands.rs:30` `native_etw.rs:193` `save_learning_service.rs:1105` |

其中 **`save_learning_service.rs:539-545` 值得单独看**——这是一个真的死分支：

```rust
let sleep_duration = if all_exited {
    Duration::from_millis(500)
} else if iterations < 10 {
    Duration::from_millis(500)      // ← 与上一分支完全相同
} else {
    Duration::from_secs(1)
};
```

`all_exited` 算了但完全不影响结果，第一层判断是无效的，作者原本的意图（进程都退出了该睡更短还是更长？）丢失了。

（`:865` 那条 clippy 警告属于可合并、非 bug：`my games` 分支与 AppData 系列分支的**结果表达式**相同，只是前置条件不同，不能直接删。）

**其它整洁性问题**：

- **死枚举**：`GameLifecycle::Removing` 全项目零赋值（`GameLifecycle::Removing` grep 无结果），`list()` 还会把它过滤掉；`GameHealth::NeedsSetup` 只在 `new_pending` 构造时赋过值
- **`health` 字段名不副实**：`GameLibraryService::list` 每次都用 `derive_health` 重新推导并写进返回的克隆（`game_library_service.rs:41`），落盘的 `health` 值实际是陈旧的死数据。同时 `derive_health` 只看 `is_installed` + 存档范围有效性，**不参考 `lifecycle`**，所以 `NeedsRepair` 的游戏可能显示成 `Ready`
- **重复实现**：`now_iso()` 有 4 份拷贝（`save_learning_service.rs:1758`、`save_repository.rs:1142`、`save_commands.rs:866`、`launch_service.rs:521`）；`normalize_path` 有 3 份**行为不一致**的实现（`save_repository.rs:1109`、`save_learning_service.rs:1749`、`process_service.rs:127`）；`managed_executable_path`/`safe_join` 有 3 份；「按 game_uid 过滤版本 → 排序 → skip(keep) → 删除 → GC」的保留策略逻辑逐字重复了 3 遍（`launch_service.rs:331-354`、`save_version_commands.rs:242-265`、`save_commands.rs:722-743`）
- **`unknownFilePolicy` 实际未生效**：`SaveScope.unknown_file_policy` 在 `save_repository.rs:651-662` 的保护判断里**没有被读取**，`Ignore` 与 `Protect` 在提交路径上行为一致，与设计意图不符
- **`percent_decode` / `instance_service.rs:194-203`**：`WideAsciiCaseEq` 把 `u16` 强转 `u8` 比较，非 ASCII 文件名会被截断（exe 名实际是 ASCII，暂不触发）
- **前端**：15+ 处阻塞式 `window.confirm`（`App.vue:422` 甚至连着弹两次），无键盘/无障碍处理，在 `Teleport` 弹窗里调用还会丢失焦点；`GameDetailPage.vue:179` 直接改 `props.game.displayName`（违反单向数据流，下次 `loadGames` 被覆盖）；`GameDetailPage.vue:1032` 用 `message.includes('更新')` 这类**中文字符串匹配**来决定是否显示 spinner，极脆弱
- **性能**：`library_service.rs:120-138` 的 `collect_usage` 每次 `get_library_settings` 都全库递归遍历、无缓存；`App.vue:70-133` 每次输入全量 filter+sort+`new Map`（无防抖）；`cover_protocol.rs:83-116` 的云端封面兜底路径会**遍历整个缓存目录并逐个解析 JSON**
- **可维护性**：`GameDetailPage.vue` 1192 行至少混了 6 个职责（封面裁剪画布、云存档抽屉、版本时间线、任务轮询、事件监听、启动检查），建议至少拆出 `CoverEditor` / `CloudSaveDrawer` / `SaveTimeline`

---

## 5. 与设计文档的偏差

| 文档承诺 | 实际实现 |
| --- | --- |
| 「token 过期不会触发远程请求」（`docs/gamesaver-new-architecture.md` 阶段 6） | ✅ 已对齐：`request_json` 系列入口遇鉴权失败会刷新并重放（P1-3，2026-09-10） |
| `unknownFilePolicy` 区分 protect / ignore | 提交路径未读取该字段，两者行为相同 |
| `GameLifecycle::Removing` 用于表达「正在移除」 | 从未被赋值 |
| 阶段 6 待办「补齐 token 自动刷新、断点续传」 | token 自动刷新 ✅ 已实现（P1-3，2026-09-10）；**断点续传仍未开始**（代码中无续传逻辑） |

---

## 6. 建议的处理顺序

| 优先级 | 事项 | 状态 | 理由 |
| --- | --- | --- | --- |
| 1 | P1-1 加 `store.json` 的 `.bak` 启动恢复 | ✅ 已完成 | 改动极小（约 15 行），消除「整库静默丢失」这一最坏后果 |
| 2 | P1-2 抽出 `with_store_mut` 并在高风险命令上替换 | ✅ **已完成（2026-09-10）** | 竞态范围广、后果隐蔽（用户以为是偶发 bug），是最容易积累成「诡异故障」的一类；顺带修掉游戏库迁移后内存态不回写的缺陷 |
| 3 | P1-3 鉴权失败时刷新 token 并重放 | ✅ **已完成（2026-09-10）** | 长任务失败代价高，用户可感知；已同时覆盖 HTTP 401 与百度 `errno` 两种失效表现 |
| 4 | ~~P1-4 用 reparse point 检测替换 `is_symlink`~~ | ❌ **撤回** | 实测证明 Rust 的 `is_symlink()` 已识别 junction，本条不成立，无需改动 |
| 5 | P2-2 解压前大小校验 | ✅ **已完成（2026-09-10，随 V6）** | 校验前移到读取之前 + 与清单无关的硬上限；同时补掉 `meta.json` 上的同一缺陷 |
| 6 | P2-1 打包前空间预检 | ⬜ 待办 | 很便宜，直接消除「磁盘写坏」；同组里只剩这一条 |
| 7 | 清掉 clippy 警告 | ⬜ 待办 | `cargo clippy --fix` 可自动修大部分；`:539` 的死分支需要人工判断原意 |
| 8 | 合并 `now_iso`/`normalize_path`/保留策略三份重复实现 | ⬜ 待办 | 减少「改一处漏一处」（`normalize_path` 已经出现三份不一致） |
| 9 | 拆分 `GameDetailPage.vue` | ⬜ 待办 | 长期收益，不急 |

---

## 附：本次执行的验证命令

```bash
# 前端类型检查 —— 通过，0 错误
./node_modules/.bin/vue-tsc --noEmit

# Rust 编译 —— 通过，0 错误 0 警告
cd src-tauri && cargo check

# Rust 静态检查 —— 31 条警告
cd src-tauri && cargo clippy --all-targets

# 测试规模 —— 151 个 #[test]，覆盖 30 个模块
grep -rn "#\[test\]" src-tauri/src | wc -l

# 会话状态死代码确认 —— 无结果
grep -rn "LearningStatus::(Finished|Cancelled)" src-tauri/src

# 生命周期死枚举确认 —— 无结果
grep -rn "GameLifecycle::Removing" src-tauri/src
```

## 附：修复轮次的验证（2026-09-10）

| 轮次 | 关键验证 | 结果 |
| --- | --- | --- |
| P1-1 | `cargo test` | 161 passed（新增 10） |
| P1-2 | `cargo test` / `cargo clippy --all-targets` | 164 passed（新增 3）；clippy 35 条经逐条核对全部为既有项 |
| P1-3 | `cargo test` / `cargo clippy` / `cargo fmt --check` | **167 passed**（新增 3）；`baidu_netdisk_service.rs` clippy 0 警告、格式干净 |
| P1-4 | 独立探针 crate 实测 `FileType::is_symlink()` 对 junction 的返回值 | junction 返回 `true` → **本条撤回，未改任何代码** |

当前全量 `cargo test`：**167 passed / 0 failed**。

> 关于 P1-4 的方法论提示：涉及平台特定 API 语义的安全结论，应当先用最小可运行程序实测再采纳。本次若照原始结论直改，会引入一次无意义的改动并污染 `safe_join` 的语义。

# store 读改写写回点统计
grep -rn "store.lock() = candidate" src-tauri/src
```

> 说明：本报告基于静态代码阅读 + 上述实际执行的验证命令。所有引用的行号来自当前工作区（含未提交改动）的文件快照。少数条目为阅读推断而非实测，涉及运行时行为的判断已在正文标注。
