# 命令层阻塞主线程审计（2026-09-14）

审计对象：`src-tauri/src/commands/` 下全部 73 个 `#[tauri::command]`。

结论一句话：**73 个命令里 async 有 0 个**，其中 16 个在命令体内**直接做阻塞 IO**
（网络 / 批量删除 / 大目录扫描），这些 IO 全部跑在 Tauri 的**主线程**上。

但要说清严重性的分布：网络那一类在这台机器上**当前不发作**（云未授权，走离线分支），
真正**每次都会发作**的是**批量删除**——它以 35.52 GB / 29,485 个文件为代价，而这是
一个日常操作。

---

## 1. 机制取证：为什么"同步命令"等于"主线程"

这一条是全部结论的地基，所以不引文档记忆，直接看源码。

**`tauri-macros-2.5.5/src/command/wrapper.rs`**：同一个宏按 `async` 生成两条完全不同的路径。

```rust
// 同步命令 —— body_blocking，第 384~390 行
let result = $path(#(match #args #match_body),*);   // ← 在调用线程里直接执行
let kind = (&result).blocking_kind();
kind.block(result, #resolver);

// async 命令 —— body_async，第 344~348 行
#resolver.respond_async_serialized(async move {      // ← 交给异步运行时，换个线程
  let result = $path(#(args),*);
  ...
});
```

同步命令没有任何 `spawn`，函数体在**收到 IPC 的那个线程**上跑完。

**`wry-0.54.4/src/webview2/mod.rs:892`**：Windows 上 IPC 由 WebView2 的
`add_WebMessageReceived` 回调进入 —— 该回调在**创建 webview 的线程**（即主线程）上触发。
Tauri 侧入口是 `tauri-2.10.3/src/webview/mod.rs:1724`。

所以：**同步命令体内的一切耗时，都是主线程的耗时。**

### 冻结的可见表现（比"画面不动"更隐蔽）

主线程被占住时：

- WebView2 渲染进程照常绘制 —— **画面和滚动仍然流畅**，滚动条也是合成器在管；
- 但**所有 `invoke` 都排在被阻塞的那一个后面**，慢命令会连带拖住它之后的一切 IPC；
- 窗口自身的消息循环（拖动、最大化、菜单）停摆；
- 前端 await 的按钮 handler 表现为"点了没反应"，只能显示 spinner。

第一点很重要：**这正是上一轮滚动卡顿调查里，渲染进程 trace 一直很干净的那种形态。**

### 网络客户端的超时

`services/baidu_netdisk_service.rs:272`：

```rust
let client = Client::builder()
    .connect_timeout(Duration::from_secs(20))
    .timeout(Duration::from_secs(120))
    .user_agent("pan.baidu.com")
```

**总超时 120 秒。** 所以主线程卡在网络上的量级不是"几秒"，而是"最长两分钟"。

---

## 2. 本机实测规模

| 量 | 实测值 |
|---|---|
| 托管游戏本体（`managedPath`）合计 | **35.52 GB / 29,485 个文件** |
| 单个最大（傻乎乎主播蔚来） | **7,289.7 MB / 20,717 个文件** |
| 其余大项 | 推币机 10,087.9 MB、DemonicMahjong 6,384.4 MB / 1,389 文件 |
| library root | `E:\GameSaverGames`（Disk1，NVMe SSD） |
| 存档对象库 | 72 个前缀 / **88 个对象** / 6.9 MB |
| 云授权状态 | **未授权**：`com.gamesaver.next` 下无 token 文件；旧路径 `com.gamesaver.desktop` 的 token 为空（08-29） |

最后一行决定了分级：**云相关命令在这台机器上走离线分支、没有网络往返**，因此它们是
**潜在**缺陷；而删除类命令不需要网络，**每次都发作**。

---

## 3. 发现

### F1（当前发作，最严重）`remove_game_from_library` 主线程删除 35 GB 级别数据

> **状态：已修复。** 文件删除已移入后台线程，按任务上报进度与失败项；同步段只保留
> 「校验 → 认领 → 建任务 → 从 store 摘除并落盘」，顺序也改成**先摘除、后删文件**。
> 守卫见 `commands/game_commands.rs` 的 `mod removal_tests`（其中一条专门防回退到
> `let _ =`：删不掉的路径必须被报告）。

`commands/game_commands.rs:527`，同步命令，内部依次：

| 位置 | 删除对象 |
|---|---|
| `:579` | `managed_path` —— 托管游戏本体（本机最大 7.3 GB / 20,717 文件） |
| `:586` | `<games_root>/.versions/<uid>` |
| `:590` | `<games_root>/covers/<uid>` |
| `:594` | `<games_root>/.<uid>.updating` —— 更新中断留下的暂存副本 |
| `:607` | `<app_data>/cache/body_packages/<uid>` |

两个问题：

1. **全部在主线程**。删除 20,717 个文件是元数据密集操作，在 NVMe 上也是秒级，
   遇到杀软实时扫描会更久。这段时间窗口消息循环与所有 IPC 一起停摆。
2. **五处全是 `let _ =`**，删除失败**静默**。结果是：游戏已从库里消失（第 611 行起才写
   store），但空间还占着，而且**没有任何地方告诉用户**。这是一条静默的磁盘泄漏路径。
3. 附带的一致性问题：**先删文件、后写 store**。若在删除中途崩溃/被结束进程，
   库里还留着这个游戏，但它的本体已经被删掉一半 —— 这个操作没有日志或回滚。

这一条正好接在上一轮的确认框修复之后：现在用户点「从库中移除」→ 确认框（已修好，
真的会拦）→ **确认之后就是这段冻结**。

### F2（当前发作）主线程删除

> **状态：已修复。** `discard_pending_game` 的 `remove_dir_all(quarantine)` 挪进
> `std::thread::spawn`；`fs::rename`（元数据操作，很快）与 store 更新仍留在同步段，
> 保证"游戏已从待设置列表消失"在命令返回时就成立。
> 与下面第 5 节建议的偏差要写清楚：这次**没有**把删除任务化，因为
> `TaskService::create` 会同步落盘 tasks.json（实测 2.8 MB），而这里收拾的只是一个
> 已被摘除引用的隔离目录，建任务还会多出一张「未知游戏」的卡片 —— 不划算。
> 后台删除失败改为 `logging::error`，兜底交给启动清扫：
> `GameBodyUpdateService::cleanup_stale_update_artifacts` 现在也回收 `.<uid>.discarding`
> （与 `.updating` 同一套机制；**这是本轮补的**，之前这一类残留全项目没人收，
> 只有 `.discarding` 一个临时目录不在任何清扫里）。
> 守卫见 `lib.rs` 的 `commands_never_delete_directory_trees_on_the_main_thread`，
> 同步命令体内出现 `remove_dir_all(` 且不在 `thread::spawn` / `spawn_blocking` 闭包内即失败
> （已做变异验证：把它移回同步段，守卫报出
> `save_commands.rs:discard_pending_game（体内含 remove_dir_all(，且不在 thread::spawn 闭包内）`）。
>
> **量级（本机实测）**：脚本已入库为 `docs/command-blocking-audit-2026-09-14-delbench.rs`
> （用 std 的同名函数，在**游戏所在卷 E:** 上跑 20,000 个文件 / 625 MB）：
> `fs::remove_dir_all` **8,762 ms（438 µs/文件）**，按最大游戏 20,717 个文件折算 **≈ 9 秒**；
> 同一棵树 `fs::rename` 只要 **8~19 ms**。也就是说修前每放弃一个待设置游戏，主线程要停
> 约 9 秒，而 rename 留在同步段的代价只有十几毫秒。
> 附带一条测量教训：同一段代码在 %TEMP%（C: 盘、刚写完的树）上 rename 要 933 ms，
> 换到游戏卷上只剩 8 ms —— 这类测量必须放在真实卷上做，否则会得出反向结论。

- `commands/save_commands.rs:545` `discard_pending_game`：`:566` `fs::rename` 到隔离区、
  `:587` `remove_dir_all(quarantine)`。同样是**可能 GB 级的删除**在主线程。
  比 F1 好的一点是它用了 rename 隔离（`discard` 语义更接近原子）。

> **更正：本文件初版在这里多写了一条，是我的错。**
> 初版把 `commands/add_game_commands.rs:137` / `:160` `start_add_game_task` 的两次
> `remove_dir_all` 也算作主线程删除。**它是假阳性**：该命令在 `:108` 就
> `std::thread::spawn` 出去了，两次删除都在任务工作线程里执行，主线程一次都没被阻塞。
> 错因在分类器：它取的是「`#[tauri::command]` 函数体全文」，而花括号配对会把函数体内
> **嵌套的闭包**一并算进来，于是线程内的调用被当成了命令体的直接调用。
> 教训是这类"体内含某标记"的静态判断必须区分闭包边界 —— 本轮补的守卫因此显式算了
> `thread::spawn` / `spawn_blocking` 闭包覆盖的区间，而不是只看字符串在不在体内。
> F2 的真实范围只有上面一处，已修完。

### F3（潜在：云一旦授权就发作）10 个命令在主线程内联网络

> **状态：已修复。** 10 个命令全部改为 `async` + `run_blocking`：阻塞工作由
> `tauri::async_runtime::spawn_blocking` 放到**阻塞线程池**执行，命令体只剩
> `run_blocking(move || xxx_blocking(app)).await` 一行，原实现原封不动搬进同文件的
> `xxx_blocking`。只把命令标成 `async` 是不够的 —— 那会让 `reqwest::blocking` 占住
> tokio 的 worker 线程（默认与 CPU 核数相同，是给异步任务用的）。
> 守卫见 `lib.rs` 的 `commands_never_call_the_network_on_the_main_thread`，
> 它扫 `src/commands/*.rs`，同步命令体内出现网络标记即失败（已做变异验证：塞一行
> 网络调用进去，它会报出具体文件与命令名）。

全部实测过代码，附带往返次数：

| 命令 | 位置 | 网络内容 |
|---|---|---|
| `delete_cloud_save_version` | `cloud_save_commands.rs:283` | `delete_file` + `fetch_manifest` + `save_manifest` = **3 次往返**（且是删除操作） |
| `start_restore_cloud_save_task` | 同上 `:205-213` | **在 `thread::spawn`（`:225`）之前**先 `fetch_manifest` 一次往返 |
| `list_cloud_games` | `baidu_commands.rs:149` | `client.list(REMOTE_ROOT)`：**列出整个远端根目录**再在 Rust 里分页 |
| `get_baidu_quota` | `baidu_commands.rs:36` | `quota()` → `.send()` |
| `list_remote_body_packages` | `baidu_commands.rs:608` | `client.list(...)` |
| `list_cloud_save_versions` | `cloud_save_commands.rs:72-73` | `fetch_manifest` |
| `get_cloud_save_overview` | `cloud_save_commands.rs:34` → `cloud_save_service.rs:689` | `fetch_manifest` |
| `get_cloud_account_status` | `cloud_account_commands.rs:24-25` | `list_account_files` |
| `exchange_baidu_code` | `baidu_config_commands.rs:109/:125` | OAuth 换 token，`.send()` |
| `get_baidu_status` | `baidu_commands.rs:27` | 仅当 token 过期时 `refresh_token` |

`start_restore_cloud_save_task` 这一处尤其刺眼：第 196~202 行的注释显示作者**专门**处理了
认领守卫的三处早退泄漏（`CloudOperationClaim`），锁与守卫的严谨程度很高；但同一个函数里，
一次网络往返就这么留在了主线程上 —— **严谨与疏忽出现在同一屏代码里**，说明问题不在
态度，而在**缺少一层机械检查**。

`build_baidu_authorize_url` 曾是候选，**已排除**：`:91` 的 `reqwest::Url::parse` 只是解析
URL 字符串，不发请求。

### F4（规模依赖）`update_save_profile_keep_versions` 内联全盘 GC

`commands/save_commands.rs:754` 调 `SaveRepository::collect_garbage`，而后者
（`repositories/save_repository.rs:312`）会：

- 取**全局** `REPOSITORY_LOCK`（`:313-314`）；
- 遍历整个对象库：1 次根 `read_dir` + 每个前缀 1 次（`:331`、`:339-340`），逐个判断引用关系并删除。

**本机实测只有 72 个前缀 / 88 个对象 / 6.9 MB → 几十毫秒，不算问题。** 但代价随存档对象
线性增长（一个长期使用、频繁快照的库很容易到几万对象），而且它**在锁内**。因此列为
"规模依赖"，不列为当前缺陷。

同一函数在任务线程里也调用（`save_version_commands.rs:274`、`:386`）—— **任务路径是对的**，
只有 `update_save_profile_keep_versions` 这一条内联。

### F5（轻微）封面与元数据读取

`get_game_cover`（`game_commands.rs:228` `fs::read`）、`get_cloud_game_cover`
（`baidu_commands.rs:56`）、`get_cloud_game_cover_paths`（`:99/:115/:125` `read_dir` +
两次 `read_to_string`）、`list_game_body_versions`（`game_body_commands.rs:57` `metadata`）。
单次都是毫秒级，列出仅为完整；不做处理也可以。

---

## 4. 与上一轮"滚动卡顿"的关系（诚实结论）

**本审计不解释那三次录制里的卡顿。** 理由：

- 三次录制时云**未授权**，`get_baidu_status` / `get_cloud_save_overview` 都走离线分支，
  主线程没有网络阻塞 —— 这与"滚动满帧 120 Hz"的实测是一致的；
- F1/F2 发生在**弹窗交互之后**（移除游戏、丢弃待确认游戏），不是滚动中。

但它给出一个**可自证的对照**：如果你觉得"移除游戏"或"手动取消"时窗口有过明显发僵，
那就是 F1/F2 在发作，机制与量级都已明确。反过来，如果滚动卡顿与这些操作无关，
那卡因仍在别处 —— 主线程阻塞这条路已经可以排除掉大部分了。

---

## 5. 修法建议

### F1 / F2：不能只加 `async`

把 `remove_game_from_library` 改成 `#[tauri::command(async)]` **不够好**：
它只是把 7 GB 的删除挪到 tokio 线程，用户依然没有任何进度反馈，`remove_dir_all` 也没有
可中断点，而且删除失败仍被 `let _ =` 吞掉。正确做法是**任务化**，而仓库里已有现成范式：
`delete_save_version` / `prune_save_versions`（`save_version_commands.rs:86/:152`）
都是"`TaskService::create` → `thread::spawn` → 工作线程里做删除 → `collect_garbage`"，
并且 `SaveRepository::restore` 还带了 `rollback_restore`。

建议顺带修掉两点：
- `let _ =` 改为把失败写进任务 `error`（或至少返回给前端），别再静默留孤儿空间；
- 顺序改为"先把游戏标记为移除中/落盘，再删文件"，或保留 rename 隔离（照
  `discard_pending_game` 的写法），避免"删一半崩溃 → 库里留着半截本体"。

### F3：两类改法，需要你选一个方向

1. **逐点最小改**：给这 10 个命令加 `#[tauri::command(async)]`。注意 `reqwest::blocking`
   在 tokio worker 上跑会占住 worker，规模小的时候没问题，但严格说应该用
   `tauri::async_runtime::spawn_blocking`。
2. **根治**：`baidu_netdisk_service` 换用 async `reqwest`，命令全部 async。改动面大
   （该文件 1,425 行、11 条测试），但以后不会再有人往同步命令里加网络调用。

我的建议：**先做 1**（一次修完 10 个点，风险低、立刻见效），把 2 记为后续项。
`exchange_baidu_code` 与 `delete_cloud_save_version` 优先级最高（一个在授权流程里、
一个是 3 次往返的删除）。

### F4：把 `update_save_profile_keep_versions` 的 GC 挪进任务

与 `save_version_commands.rs` 的既有写法保持一致即可。当前规模下收益为零，
属于"顺手对齐"，不急着做。

### 守卫：加一条源码扫描测试

> **状态：已实现。** 实际落地成两条独立的测试（都在 `lib.rs`）：
> `commands_never_call_the_network_on_the_main_thread`（F3，扫网络标记，同步命令体内命中即失败）
> 与 `commands_never_delete_directory_trees_on_the_main_thread`（F2，扫 `remove_dir_all(`，
> 豁免 `thread::spawn` / `spawn_blocking` 闭包内的调用）。两条都做过变异验证。
> 与下面原始建议的偏差：删除那条没有按"体内出现 `TaskService::create`"来豁免 ——
> 标志物和安全性并不是一回事（`discard_pending_game` 就是"后台删除但不建任务"），
> 判据改成"这次调用是否真的在后台闭包里执行"更贴近要防的东西。

仓库已有先例（`lib.rs` 里那条 `frontend_never_uses_window_confirm`，扫描 `.ts/.vue`）。
同一套路可以扫 Rust：**凡是体内调用了网络构造函数（`load_baidu_client`、
`BaiduNetdiskClient::`、`Client::builder`、`.send(`）或批量删除（`remove_dir_all`）的
命令，必须满足"`#[tauri::command(async)]`"或"体内出现 `TaskService::create`"**，
否则测试失败。这样下一个写命令的人会被机械挡住，而不是靠 review 记忆。

---

## 6. 本次未做的

- **没有实测真实冻结时长**。需要两个场景之一：授权百度账号后在弱网下调用
  `list_cloud_games`；或真的移除"傻乎乎主播蔚来"（7.3 GB / 20,717 文件）并计时。
  两条我都没擅自做（前者动账号，后者不可逆）。
- **没有逐层解到 3 层以上的调用链**。分类脚本跟到 3 层；附录 B 段里带 `→ finish`、
  `→ load`、`→ new` 的条目是**同名函数碰撞造成的假阳性**（例如命中 `test_support.rs`
  的测试辅助函数、`game_repository::load` 的哈希计算），已逐条排除，未计入发现。
- **没有审计异步/线程池自身**（`std::thread::spawn` 的使用是否有上限、任务线程里的
  锁与 `AppState` 的关系）—— 那是另一条线（锁审计）。

---

## 附录：分类结果

- **A 段（体内直接含网络/子进程/大文件 IO，已逐条人工核验）**：17 条，其中 1 条已排除，
  实为 16 条：
  `start_add_game_task`、`get_baidu_status`、`get_baidu_quota`、`get_cloud_game_cover`、
  `get_cloud_game_cover_paths`、`list_cloud_games`、`list_remote_body_packages`、
  `exchange_baidu_code`、`get_cloud_account_status`、
  `list_cloud_save_versions`、`start_restore_cloud_save_task`、`delete_cloud_save_version`、
  `list_game_body_versions`、`get_game_cover`、`remove_game_from_library`、
  `discard_pending_game`
  （第 17 条 `build_baidu_authorize_url` 已排除：`reqwest::Url::parse` 不发请求）
  （A 段只表示"命令体全文里出现了这些标记"，不代表阻塞就发生在主线程：
  `start_add_game_task` 的两次删除在 `:108` 的 `thread::spawn` 闭包内 —— 见 F2 的更正）
- **B 段（一层调用可达，多为假阳性）**：29 条，已逐条剔除同名碰撞；其中确认为真的只有
  `get_cloud_save_overview`、`update_save_profile_keep_versions`（已归入 F3/F4）
- **C 段（体内与一层调用都干净）**：27 条，含 `list_games`、`get_game_detail_view`、
  `get_task`、`list_tasks`、`launch_game`、`get_library_settings` 等

合计 17 + 29 + 27 = 73 ✓

原始机器可读输出：`docs/command-blocking-audit-2026-09-14-evidence.txt`（含 A 段每一条的**证据行**）。
