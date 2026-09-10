# GameSaver 游戏状态管理与前端同步专项检查

> 检查日期：2026-09-10
> 范围：`running_games` 运行时状态机、任务状态流转、前端状态中心（`App.vue`）、前后端同步通道
> 关联：`docs/code-review-2026-09-10.md`（全局审查报告）。本文只覆盖「状态 + 同步」这一横切面，不重复全局报告里已登记的条目（如需交叉引用会在文中标注）。

---

## 0. 结论摘要

| # | 级别 | 问题 | 位置 |
|---|---|---|---|
| S1 | **高** | ✅ **已修（含跨重启会话重建）** 运行中关闭被拦截并要求确认；启动时扫描游戏目录，把仍在运行的会话连同任务记录一并接回，游戏退出后照常提交存档，「可二次启动」问题消除 | `launch_service.rs`（`restore_running_sessions` / `SessionRoot`）、`lib.rs` setup |
| S2 | **高** | ✅ **已修（方案 A）** `sync_cloud_save` 已纳入角标与传输中心，「finish_sync」改用 `finish` 落盘，失败可重试；库卡片显示「存档未同步」，角标红色提示 | `cloud_save_commands.rs`、`TransferCenter.vue`、`App.vue` |
| S3 | **高** | ✅ **已修** 详情页改以 `runtime` 为唯一真相源，并按 `runtime.taskId` 接回会话轮询；库卡片按活跃 `launch_game` 任务标记「运行中」 | `GameDetailPage.vue`、`App.vue` |
| S4 | 中 | ✅ **已修** 取消对话原只 kill 根进程，被跟踪的子进程（分离式启动器留在里面的游戏本体）继续存活并被当成正常退出照常快照；现改为终止整棵进程树并有界等待，取消路径不再提交存档 | `launch_service.rs`、`process_service.rs` |
| S5 | 中 | ✅ **已修** 后端新增 `task-changed` / `runtime-changed` 推送，前端共用唯一任务流；`App.vue` 与 `TransferCenter.vue` 的重复轮询与根组件常驻轮询均已删除，详情页会话轮询降为 5s 兜底 | `events.rs`、`taskFeed.ts`、`App.vue`、`TransferCenter.vue`、`GameDetailPage.vue` |
| S6 | 中 | ✅ **已修** 后台复查云端总览时不再把整个区块打成加载态（`force` 经核对必须保留，理由见正文） | `refresh()` `:205`、`cloudSaveStatusText()` `:378`、按钮 `:1277` |
| S7 | 中 | ✅ **已修** 详情页改为 `:key="gameUid"` 逐游戏重建，`loadCover` 补显式清空；失效的 uid watcher 已删 | `App.vue:618`、`loadCover()` `:1001` |
| S8 | 中 | ✅ **已修** 分类下沉到后端 `AppTask.category`，前端两份重复白名单已删除；`restore_save_version`、`launch_game` 现在可见，取消确认文案按分类分流 | `domain/task.rs`、`taskFeed.ts`、`App.vue`、`TransferCenter.vue` |
| S9 | 低 | 5 个前端 API 导入后从未调用，其中 `precheck_game_launch` 是后端整条死命令 | `GameDetailPage.vue:7`、`api.ts:324` |
| S10 | 低 | `cloudStatus` / `latestSaveVersionId` / `GameRuntime.taskId` 三个字段前端零引用 | `domain/game.ts:39/44/53` |
| S11 | 低 | 库分页越界摘要：clamp 用未过滤总数，显示用过滤后的列表 | `App.vue:220` |
| S12 | 低 | `search` 在游戏库与商店之间共用一个 ref，切页会带着关键词触发一次云端搜索 | `App.vue:28/191-199` |

**一句话结论**：后端的状态机本身是自洽的（锁序、generation 防竞态、任务中断兜底都做对了），真正的缺口在**「后端状态变化 → 前端」这一段没有任何推送**，以及**「应用退出」这个生命周期事件在状态机里完全没有被处理**。S1/S2/S3 都是同一个根因的不同表现。

> **2026-09-10 更新**：S1（退出拦截 + 跨重启会话重建，完整版）、S2（自动同步可见化）、S3（详情页运行时真相源）、S5（后端事件推送 + 轮询降级）、S4（取消会话清理整棵进程树）、S8（任务分类下沉后端）、S6（云端区块不再闪加载态）、S7（详情页逐游戏重建）均已落地。**高、中优先级已全部清空**，12 条里只剩 S9-S12 四条低优先级前端小修。

---

## 1. 现状：状态是如何流动的

```
启动游戏  launch_game ──► running_games[uid] = Launching ──► TaskService::create("launch_game", Pending)
   │                        │
   │                        ├─ 线程内 spawn 子进程，成功后 = Running(pid)
   │                        ├─ wait_for_game_session 每 500ms 循环（整场游玩）
   │                        ├─ 游戏退出 → status = Saving
   │                        ├─ SaveRepository::commit + with_store_mut 落盘
   │                        └─ 线程收尾：running_games.remove(uid) → TaskService::finish(Success)
   │                                        └─ 若产生新版本且开了自动同步 → start_upload_save_version_task（新任务，无人监听）
   └─ 前端 watchTask(taskId) 每 700ms 轮询 getTask + getGameRuntime，直到任务终结
```

关键点：**`launch_game` 这个任务的生命周期 == 整场游戏会话**（数小时级）。它既是「启动」任务，也是「游戏是否还在运行」的唯一代理信号。这带来两个后果：

1. 前端只要不在详情页（`watchTask` 的 timer 已随组件卸载清掉），就再也没有任何东西知道这场比赛结束了。
2. 应用一关，承载这个任务的线程直接消失，`running_games` 随进程消亡——而**游戏进程不会跟着死**。

---

## 2. 高优先级问题

### S1 运行中关闭 GameSaver：存档提交彻底丢失，且不可恢复 —— ✅ 已修（含跨重启会话重建）

**证据链**

- `services/launch_service.rs:169` 用 `thread::spawn` 持有 `std::process::Child`；`wait_for_game_session`（`:399-493`）在这个线程里阻塞整场会话。
- 全项目 grep `on_window_event` / `CloseRequested` / `ExitRequested` / `RunEvent` → **零匹配**。没有任何退出拦截，也没有托盘。
- `Cargo.toml:46` 设了 `panic = "abort"`，任何 panic 直接终止进程，同样走这条路径。
- `app_state.rs:14` `running_games` 是纯内存 `Mutex<HashMap<..>>`，不落盘。
- `lib.rs` 的 `setup` 里有一整套启动恢复（`recover_pending_updates`、`cleanup_archived_body_versions`、`cleanup_orphan_packages`…），**但没有一条与"运行中的游戏"相关**。

**后果（按严重度）**

1. **存档保护承诺失效**。用户在 GameSaver 里点启动 → 关掉 GameSaver → 继续玩 → 退出游戏。`SaveRepository::commit` 永远不会执行，本次游玩的存档一个版本都不会产生。这直接违背产品核心承诺「游戏退出后自动提交」。
2. **会话跟踪不可恢复 + 可二次启动**。进程在、`running_games` 不在。重启后 `precheck` 只看目录与启动器是否存在（`launch_service.rs:33-74`），不看进程，所以库卡片显示「可启动」、按钮可点，`launch()` 的 `running_games.contains_key` 检查（`:95`）也拦不住 → 同一游戏被拉起第二个实例。对存档型游戏，两个实例同时写同一份存档是很危险的。
3. **用户毫无感知**。中断的 `launch_game` 任务虽然会被 `TaskRepository::load:46-50` 标成 `Interrupted` 并给出"应用上次未正常完成此任务"，但 `TransferCenter.vue:147` 的 `isTransferTask` 白名单不含 `launch_game`，界面上根本看不到这条记录。

**修复方向（按性价比排序）**

- **最小可用**：注册 `on_window_event(WindowEvent::CloseRequested)`，当 `running_games` 非空时阻止关闭并提示「游戏正在运行，关闭 GameSaver 将无法自动保护本次存档」。这是把静默失败变成显式失败，成本极低。
- **推荐**：`lib.rs` setup 里增加「会话重建」——遍历 `store.games`，对每个 `managed_path` 调 `process_service::find_processes_in_directory`，命中则把该 game 以 `Running` 放回 `running_games` 并**重新挂起一个 `wait_for_game_session` 等价的会话线程**（不 spawn、只等待 + 提交）。这样跨重启仍能兑现"退出即提交"。
- **兜底**：无论如何都应把 `launch_game` 纳入传输中心的中断可见列表（见 S8），否则用户永远不知道上次的存档没提交。

**落地记录（2026-09-10，最小可用版）**

按上面「最小可用」档实现，未做「推荐」档的会话重建：

- `lib.rs` 的 `tauri::Builder` 注册 `on_window_event`：收到 `WindowEvent::CloseRequested` 时，若 `AppState::running_game_count() > 0` 且用户尚未确认退出，则 `api.prevent_close()`，并向前端推送 `app-exit-blocked { runningCount }`。
- 新增命令 `confirm_app_exit`（`commands/app_commands.rs`）：先置位 `AppState::exit_confirmed` 再 `app.exit(0)`。确认标记一旦置位，后续关闭请求直接放行，不会二次拦截。
- `AppState` 新增 `running_game_count()` / `exit_confirmed()` / `confirm_exit()` 与私有字段 `exit_confirmed: AtomicBool`，并补 1 条契约测试 `exit_guard_tracks_running_games_and_confirmation`。
- `App.vue` 监听 `app-exit-blocked`，用全站统一的 `window.confirm` 提示「有 N 个游戏正在运行……仍要关闭吗？」，用户确认后才调用 `confirm_app_exit`；带 `exitPromptOpen` 去重与卸载清理。
- `api.ts` 新增 `confirmAppExit()` 包装。

**效果与残留**：把「静默丢失存档」变成一次**显式确认**——这是本档的全部目标。用户确认退出、或强杀进程后本次存档依然不会提交，直到下面的「推荐」档补上。

**落地记录（2026-09-10，会话重建 —— 「推荐」档）**

应用退出不会带走游戏进程：重启后 `running_games` 已随进程消亡，但游戏还在跑。只做退出拦截是不够的 —— 用户一旦确认退出（或强杀），这场会话就彻底没人跟踪：重启后界面显示「可启动」，再点一次即拉起第二个实例，两个进程同时写同一份存档；而且本次游玩退出时不会再有人提交存档版本。现在启动时会把它们接回来。

- `launch_service.rs` 新增 `LaunchService::restore_running_sessions(app, state)`，在 `lib.rs` 的 setup 里**于事件推送器注入之后**调用：遍历 `store.games` 中 `Active` 且无运行时标记的游戏，用 `find_processes_in_directory(managed_path)` 找进程；命中则以 `TrackedProcessHandle` 持有句柄接手 —— 持有句柄同时阻止 PID 被回收，使后续按 PID 的存活探测与终止都不会打错目标。
- 引入 `SessionRoot { Spawned(Child) | Adopted { pid, handle } }` 与 `RootProbe`，把「等待会话结束」从 `&mut Child` 抽象到两条来源之上。`GameSessionEnd::Exited` 因此改为携带 `Option<ExitStatus>` —— 接手的进程不是我们的子进程，拿不到退出码。
- 抽出 `finish_game_session`（等待 + 提交存档）与 `conclude_session`（摘运行标记 + 写任务终态 + 触发自动同步）：**自己 spawn 与跨重启接手两条路径在这里合流**。「取消该不该提交存档」这类判断最怕两处不一致，合流是刻意的。
- 任务记录优先「复活」该游戏最近一条 `Interrupted` 的 `launch_game` 记录（玩家看到一条连续的会话记录，而不是「一条中断 + 一条新的」），并用它的 `created_at` 换算成秒作为会话起点；记录已被用户删除时才新建。`launch` 与接手共用 `session_inputs_from_store` 挑选存档配置与最近版本，避免两条路径认的配置不是同一个。
- `process_service.rs` 为 `TrackedProcessHandle` 增加 `unsafe impl Send`：Windows 句柄是**进程范围**的内核对象、不属于创建它的线程，把独占所有权交给会话线程正是 `Send` 的语义（未一并引入 `Sync`，句柄只被独占使用）。

**新增 3 条测试**（178 → 181）：`restore_finds_a_game_whose_process_outlived_the_app`（把 `ping.exe` 复制进「游戏目录」再运行，保证真的走到「按进程映像路径发现」那条路径）、`an_adopted_session_reports_exited_without_an_exit_code`、`cancelling_an_adopted_session_terminates_the_process`（接手的进程不是我们的子进程，取消仍必须能终止它）。**做了变异验证**：分别停掉「接手时终止进程」与「认领扫描到的进程」后，后两个测试精确失败，确认不是恒真断言。

**仍然残留**：用户点「仍要关闭」之后、GameSaver 真正退出之前，若游戏恰好也在这时退出，提交仍可能来不及（没有「退出前等待在途任务」）；同步未跑完就关应用，承载上传的线程随进程消亡，任务停在 running（重启后显示「异常中断」）。这两条叠起来才是「存档永远不丢」的完整闭环。

---

### S2 自动云同步是"隐形任务"，失败静默且结果不落盘 —— ✅ 已修（方案 A）

**证据链**

- `services/launch_service.rs:192-216`：游戏退出且产生了新版本、且 `auto_sync_save` 开启时，直接调用 `start_upload_save_version_task(...)` 并丢弃返回值（`let _ =`）。
- `commands/cloud_save_commands.rs:314-318` `begin_sync` 建任务，类型固定为 `"sync_cloud_save"`。
- 前端两处任务类型白名单都不含它：`App.vue:315-320`（角标计数）、`TransferCenter.vue:147`（列表过滤）。
- 详情页只在「用户手动点上传/还原」时才 `watchTask` 这个 id；自动触发的那次没有任何监听者。

**后果**

- 自动同步失败（token 失效、网盘空间不足、网络中断、清单校验失败）**没有任何 UI 出口**：角标不涨、传输中心不列、详情页不知道。用户会合理地认为存档已经上云。
- 这是「云端增强、本地优先」原则里最容易被违反的一环——失败不影响本地，但**静默**失败让人产生错误的安全感。

**附带缺陷（同一处）**：`cloud_save_commands.rs:320-350` 的 `finish_sync` 用 `TaskService::update` 收尾，而 `task_service.rs:56-72` 的 `update` **只改内存不落盘**（`create` 写下的是 `Pending`）。所以 `sync_cloud_save` 的结果永远不会写进 `tasks.json`，重启后会被 `TaskRepository::load:46-50` 判定为"中断"。同文件其它任务（`upload_game_body_package` 等）用的是 `finish`，行为不一致。

**修复方向**：① 把 `sync_cloud_save` 纳入角标与传输中心（或按 S8 改成后端分类字段）；② `finish_sync` 改用 `TaskService::finish`；③ 自动同步失败时在游戏卡片/详情页写一个持久可见的提示（例如复用 `cloudStatus`，见 S10）。

**落地记录（2026-09-10，方案 A：同一列表 + 标题/图标区分，不新增类型筛选）**

先修地基，再谈可见：

- **`finish_sync` 由 `TaskService::update` 改为 `TaskService::finish`**（`cloud_save_commands.rs`）。这是本档的根：`update` 只改内存，终态与 `error` 从不落盘，重启后会被 `TaskRepository::load:46-50` 兜底成「异常中断」——一次**成功**的同步看起来像故障，一次**真实失败**连原因都丢掉。
- `finish_sync` 增加 `retry: TaskRetry` 参数，且**只在失败时** `TaskService::set_retry`，让传输中心的「重试」按钮有东西可点。上传记为 `sync_cloud_save`、还原记为 `restore_cloud_save`，两者都以 `versionId` 为参数。
- `TransferCenter.vue`：`isTransferTask` 纳入 `sync_cloud_save`；`taskTitle` 补分支（**此前会掉进默认分支显示成「修复云端清单」**，是个已存在的错误标题）；图标分支；`retry()` 接 `startUploadSaveVersionTask` / `startRestoreCloudSaveTask`；`retryIcon` 改为按 `retry.operation` 判断。
- `App.vue`：角标 `isTransfer` 纳入 `sync_cloud_save`（进行中计入）；新增 `unsyncedGameUids` / `syncAttentionCount`，**每个游戏只取最近一条同步任务**（否则一次早先的失败会永远钉在卡片上，哪怕后来已同步成功），终态失败才计入；库卡片挂「存档未同步」标记；角标在没有进行中任务时显示红色提示数。
- `style.css`：新增 `.nav-badge-alert`、`.status-label-warn`。

**三档可见性**：进行中 → 角标计数 + 带进度卡片；成功 → 只留一条记录、**不弹窗**（每局都弹太吵）；失败 → 角标红标 + 传输中心红卡可重试 + 库卡片标记。

**契约测试**：新增 `task_service::tests::failed_sync_task_keeps_status_error_and_retry_on_disk`，锁死「终态 + error + retry 必须落盘」。

**与报告的两处偏离**：

1. 标题没用「自动同步」前缀，改为「**云存档同步**」。因为 `sync_cloud_save` 同时覆盖**用户手动点还原**与**退出后自动上传**，一律加「自动」会把手动还原标错。
2. 失败提示**没有**引入全局 toast（项目本就没有这套机制），复用既有的「角标 + 卡片 + 重试」语言，零新增 UI 范式。

**残留**：若用户在同步未跑完时关闭 GameSaver，承载上传的后台线程随进程消亡，任务会停在 running（重启后显示「异常中断」）。这属 **S1 完整版**（退出前等待/守护在途任务），不在本档范围。两条叠起来才是「存档永远不丢」的闭环。

---

### S3 详情页「运行中」双真相源，重新进入后按钮状态错误 —— ✅ 已修

**证据链**

- `GameDetailPage.vue:39` `busy` 与 `:26` `runtime` 是两个独立 ref。
- `start()`（`:383-395`）置 `busy = true` 并进入 `watchTask`；`busy` 只在 `watchTask` 拿到终态时复位（`:404-416`）。
- `runtime` 只能从 `getGameDetailView` 得到（`:203`），且只在 `refresh()` 时读取。
- 按钮：`:979` `:disabled="busy || !precheck?.canLaunch"`，`:983` 文案 `busy ? "游戏运行中" : ...` —— **判定与文案都只看 `busy`，完全不看 `runtime`**。
- 库卡片同理：`App.vue:578` `:disabled="game.lifecycle !== 'active'"` + `quickLaunch`（`:386-397`）不检查 runtime。

**复现**：点启动 → 游戏起来了 → 返回游戏库 → 再次点进这个游戏。新组件实例 `busy=false`，`refresh()` 拿到 `runtime.status="running"`。此时详情页顶部标签正确显示"运行中"，但下面的大按钮显示「启动游戏」且可点；点了才由后端返回"游戏已经在运行"。库里卡片的启动按钮同样可点。

**更根本的一层**：`GameRuntime.taskId` 后端已经填好（`domain/game.rs:117`、`launch_service.rs:164`），前端零引用（见 S10）。这意味着组件一旦卸载，**前端就永久失去了对这场会话的追踪能力**——哪怕用户只是切了个标签页回来，也再拿不到退出事件与提交进度。这正是 S5 里"详情页 700ms 轮询整场会话"存在的唯一原因：它靠一个生命周期脆弱的组件撑住了整个会话的同步。

**修复方向**

- 把 `runtime` 作为唯一真相源：`disabled = busy || !!runtime || !precheck?.canLaunch`，按钮文案由 `runtime.status` 推导（`launching` / `running` / `saving`），`busy` 只用于"命令已发出但 runtime 还没更新"的短窗口。
- 挂载时若 `detail.runtime?.taskId` 存在，**直接接回 `watchTask(runtime.taskId)`**，让进度与终态重新可见。
- 库卡片增加运行时标记（可复用同一份 runtime 数据），并让 `quickLaunch` 在 runtime 存在时改为"回到该游戏详情"而不是再发一次 launch。

**落地记录（2026-09-10）**

- `GameDetailPage.vue`：新增 `launchButton` computed —— `disabled = !!runtime || busy || !precheck.canLaunch`，文案由 `runtime.status` 推导（正在启动 / 运行中 / 正在保护存档）。新增 `launchPending` 只覆盖「命令已发出、`runtime` 尚未反映」的短窗口；**不用 `busy`** 是为了避免把上传/还原的 busy 误读成"正在启动"。
- `refresh()` 拿到 `detail.runtime` 后调用 `reattachRuntimeTask()`：若存在 `runtime.taskId` 且尚未处理过，就接回 `watchTask(taskId)`，让「运行中」与退出后的提交进度重新可见。`watchedTaskId` 记录已处理的 taskId，既防重复接回也防死循环（任务终态时后端已先清掉 `running_games`，`runtime` 必为 null）。
- `watchTask(taskId, gameUid)` 现在带游戏归属：组件切到别的游戏时立即停止跟踪，避免上一场的 `message` / 进度条写进当前页面。切换游戏的 watcher 也会显式清空 `runtime` / `busy` / `message` / 进度。
- `watchTask` 进入时置 `busy = true`：这样重新进入详情页接回会话时，进度面板仍显示「游戏正在运行，退出后将提交存档版本」。
- `App.vue`：`quickLaunch` 先查 `getGameRuntime`，已在运行则直接跳详情页而不再发起 launch；库卡片按**活跃的 `launch_game` 任务** 显示「运行中」并禁用按钮。

**偏离报告的一处（有意为之）**：库卡片的运行时标记没有复用 `runtime`（那需要对每个游戏单独查询，或新增推送通道），而是复用前端已有的 `listTasks` 轮询 —— `launch_game` 任务的生命周期恰好等于整场会话，与后端 `running_games` 同源同寿命，不增加 IPC。

**遗留整洁性**：`App.vue::updateTransferCount` 现在同时维护传输计数与运行中游戏集合，函数名已名不副实；留待 S5 统一重构轮询时一并处理。

---

## 3. 中优先级问题

### S4 取消启动任务不清理子进程树 —— ✅ 已修

`launch_service.rs:449-459`：检测到取消后只 `child.kill()`（根 PID）+ `child.wait()`，然后返回。而 `tracked_handles` 里那些通过"进程树展开"和"镜像位于受管目录"发现的子进程（`:411-447`，注释明确写了是为 UAC/分离式启动器准备的）**不会被终止**。

于是 cancel 之后完全可能：游戏仍在运行 → `run_game_session` 继续走到 `status = Saving` 并提交一次存档快照（`:287-318`）→ 外层把 `running_games` 条目移除（`:178-180`）。也就是说 GameSaver 会在游戏还在写存档的时候去快照它，然后再也不跟踪。

**当前状态是潜在问题**：取消入口只有 `TransferCenter` 的取消按钮，而 `launch_game` 被白名单挡在列表外（S8），所以 UI 上够不到这条路径。但一旦按 S8 放开可见性，这个缺陷就会立刻变成现实。修 S8 时必须同时修这里（kill 整棵 `tracked_handles`，或先确认无存活子进程再返回）。

**落地记录（2026-09-10）**

报告里把它写成"两选一"（kill 整棵 `tracked_handles`，或先确认无存活子进程再返回），实施时选了**终止整棵树**。过程中发现真正的拦路虎在权限层：按当时的代码，`tracked_handles` 根本杀不动。

*根因是三层，不是一层*

- **能力层**：`TrackedProcessHandle::open` 只申请了 `PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE`，**没有 `PROCESS_TERMINATE`**。报告里那句"kill 整棵 `tracked_handles`"落不了地。
- **语义层**：`wait_for_game_session` 返回 `Result<ExitStatus, String>`，取消分支返回 `Ok(status)`，与"游戏自己退的"在类型上完全无法区分。
- **上游层**：`run_game_session` 全程没有一处读 `is_cancelled`，拿着那个 `Ok(status)` 一路走到置 `Saving`、`commit`、写 `latest_save_version_id`，最后 `finish(Success)`。

*改法*

- `process_service.rs` 新增 `terminate_process(pid)`，用 `PROCESS_TERMINATE | SYNCHRONIZE` 单独打开句柄调 `TerminateProcess`；**刻意不动 `TrackedProcessHandle::open` 的权限** —— 它被 `is_process_running` 用来探测任意 PID，并进终止权限会让部分受限进程从「能读到退出状态」退化成「打开失败」，从而被静默判定为「不在运行」。函数拒绝 PID 0（System Idle Process）与自身。
- `wait_for_game_session` 返回类型改为 `GameSessionEnd { Exited(ExitStatus), Cancelled }`。取消分支交给新的 `terminate_session`：先 `child.kill()` 断根以阻断继续派生，再逐个终止 `tracked_handles` 中仍存活的进程，然后**有界等待**（5s）确认退出；超时只记一条错误日志 —— 宁可留日志，也不能让执行线程永久挂住、把界面卡在「运行中」。
- 顺手补上一个被忽略的边：原先「根进程已退出、正在等残留子进程」时若被取消，代码直接 `return Ok(status)`，连根的 kill 都跳过。现在同样走 `terminate_session`。
- `run_game_session` 返回类型改为 `GameSessionOutcome { Saved { .. }, Cancelled }`，收到 `Cancelled` 立即返回，**跳过** `Saving`、`commit`、`release_pending_objects` 与 store 写入。另外在 spawn 之前补了一次 `is_cancelled` 预检，避免任务在「创建」与「spawn」之间被取消时留下一个没人跟踪的游戏进程。
- spawn 线程收尾改为三分支：`Saved` → `Success`（照旧触发自动云同步）、`Cancelled` → `Cancelled`（不触发任何同步）、`Err` → `Failed`。取消既不是成功也不是失败，如实标记。
- `GameSessionEnd::Exited` 的退出码没有用 `allow(dead_code)` 压掉警告，而是接上了「非零退出记一条错误日志」—— 异常退出码本来就是最直接的排查线索。

*验证*

- 新增 4 个测试（172 → **176**）：`test_terminate_process_kills_a_live_child`、`test_terminate_process_refuses_self_and_idle`、`cancelling_a_session_terminates_the_tracked_process_tree`、`a_session_that_ends_on_its_own_reports_exited`。
- 取消测试刻意模拟**分离式启动器**：`cmd` 只负责派生，真正的「游戏」是它拉起的 `ping`，断言取消后两者都不再存活。
- 做了**变异验证**：临时把子进程终止循环去掉后，该测试精确地在「取消后子进程不应仍然存活（pid=…）」处失败 —— 证明它在真正守护这个行为，而非恒真断言。
- `cargo test` 176 passed / 0 failed；`cargo clippy --all-targets` 保持 lib 30 / lib test 34（与改动前一致）；`rustfmt --check` 干净。

*残留*

- **提交阶段的取消仍然无效**：一旦进入 `SaveRepository::commit`，`cancel` 仍会置 `cancel_requested`（任务还是 Running），但没有任何代码再读它，最终会 `finish(Success)`。彻底修需要让 commit 支持中途放弃并回滚已复制的文件，成本高，登记为已知边界。
- **前端确认文案会错配**：`TransferCenter.vue:51` 那句「取消后会停止后续分片处理，已上传的临时分片可能由百度网盘自动清理」是写死给上传任务的。S8 把 `launch_game` 放进列表后必须按 `taskType` 分流，否则点「取消」会弹出完全不相关的话。
- 取消语义定为**强杀整棵进程树**（2026-09-10 拍板）：取消 = 结束这场游戏会话。代价是误点会丢失当前进度，因此按钮文案需在 S8 一并调整。

### S5 没有推送通道，全靠各组件自建轮询 —— ✅ 已修

| 位置 | 周期 | 生命周期 |
|---|---|---|
| `App.vue:312-329` `updateTransferCount` | 活跃 1s / 空闲 3.5s / 出错 5s | **自续期，`onUnmounted` 才清；App 是根组件 ⇒ 等于永不停止** |
| `TransferCenter.vue:62-67` | 仅 `activeCount > 0` 时 700ms | 组件级，正确 |
| `GameDetailPage.vue:417` | 700ms，**持续整场游戏会话**；`:401` 每次还额外拉一次 `getGameRuntime` | 组件级，正确但代价高 |
| `PlatformSettings.vue:87/132` | 任务期间 700ms | 组件级，正确 |
| `AddGameWizard.vue` | 任务期间轮询 | 组件级，正确 |

问题不在"用轮询"（Tauri 场景下这是可接受的），而在：
- **同一份 `listTasks` 被 `App.vue` 与 `TransferCenter.vue` 各轮询一遍**（已登记于全局报告 P2-14），且根组件那份最小周期 1s、永不停止，连窗口最小化时也一样。
- **轮询周期与"状态变化频率"完全不相关**：整场游戏会话里，真正有意义的状态变化只有 3 次（Running / Saving / 结束），却付出了 700ms × 数小时的 IPC。

**修复方向**：在 `TaskService::update/finish/cancel/create` 与 `running_games` 的写入点统一 `emit` 事件（例如 `task://updated`、`runtime://changed`），前端 `listen` 后刷新本地状态，轮询退化为低频兜底（如 10s）。这同时是 S1/S2/S3 的地基：有了推送，"自动同步失败"和"游戏已退出"才可能被主动告知。

**落地记录（2026-09-10）**

事件名沿用项目既有的 kebab-case 约定，最终定为 `task-changed` / `runtime-changed`（不是报告里草拟的 `task://updated`）。

*后端*

- 新建 `events.rs`：两个事件常量 + 两个 payload（`{ taskId, kind, gameUid }` / `{ gameUid, running, status, taskId }`）。推送是**尽力而为**的：未注入推送器（单元测试）或发送失败时静默跳过，权威状态始终以后端为准，前端丢一次事件只会晚一个兜底周期。
- `TaskService` 的 `create` / `set_retry` / `update` / `finish` / `cancel` / `delete_many` 全部在**释放任务锁之后**广播（`notify_changed` 会再次加锁读 `game_uid`，持锁调用会自锁死）。
- **`update` 按 2% 粒度节流**：存档提交会按文件逐个回调，若每次回调都推一条，一场大存档会发出上千条消息。
- `running_games` 的 12 处直写全部收口为 `AppState` 的 `begin_runtime` / `set_runtime` / `update_runtime` / `remove_runtime`，写入即推送。错误信息 `"游戏已经在运行"` 由 `begin_runtime` 在同一次加锁内给出，与原行为一致。
- 新增 `AppState::runtime_of`（只在 `events.rs` 里用）。

*前端*

- 新建 `src/taskFeed.ts`：全应用**唯一**的任务流。监听 `task-changed` 后合并刷新（80ms），定时轮询只兜底（有活跃任务 2s / 空闲 10s）。`App.vue` 与 `TransferCenter.vue` 改为消费同一份数据，**同一份 `listTasks` 的双份轮询消失**，根组件那份自续期 1s 轮询也随之删除。
- `App.vue`：四个 `ref` 换成由任务流派生的 `computed`（角标计数 / 运行中游戏集合 / 未同步集合 / 未同步计数），删掉 `updateTransferCount` 与 `transferCountTimer`。
- `TransferCenter.vue`：删掉自建 700ms 定时器与 `refresh()`，改为 `taskFeed.refresh` + `taskFeed.release()`；读取错误与操作错误分离展示。
- `GameDetailPage.vue`：会话跟踪改为事件驱动（`runtime-changed` / `task-changed`），700ms 整场轮询降为 **5s 兜底**，事件侧再合并 120ms。事件匹配刻意收紧为「正在跟踪会话时只认任务 ID」，否则「游戏退出后的自动云同步」事件会误触发本场复核并连带整页刷新。

**效果**：整场游戏会话的高频 IPC 从 700ms×数小时降到「3 次运行时事件 + 每 2% 一次任务事件 + 5s 兜底」。

**过程中踩到并修掉的坑（重要）**：最初把 `tauri::AppHandle` 直接存进 `AppState` 用于推送，结果**单元测试二进制直接启动失败**（`0xc0000139 STATUS_ENTRYPOINT_NOT_FOUND`）。原因是 `AppState` 在测试路径上被引用，链接器因此把整条 Tauri 窗口运行时拉进测试二进制，多出 user32 / gdi32 / comctl32 / uxtheme / dwmapi / shcore / shell32 / ole32 / oleaut32 / advapi32 共 10 个 GUI 依赖。改为注入 `Box<dyn Fn(&str, serde_json::Value)>`（在只有 `run()` 才会执行的 setup 里构造），测试二进制的导入表已与基线逐项一致。

**残留**：`PlatformSettings.vue` / `AddGameWizard.vue` 的任务轮询未接本通道（它们只在自身任务期间轮询，代价有限）；`launch_game` 仍未进入传输中心（属 S8）。

### S6 详情页 `refresh()` 每次强制重拉网盘 —— ✅ 已修（修法经复核后调整）

`refresh()`（`GameDetailPage.vue:205`）里的 `void refreshCloudState(gameUid, generation, true)`（`:225`）始终带 `force`，`loadCloudSaveOverview`（`:277`）因此每次都绕过 `cloudSaveLoaded` 缓存直接请求 `getCloudSaveOverview`。而 `refresh()` 在挂载、切换游戏、任务成功、任务失败、改保留数、恢复/删除版本后都会被调用。

**落地记录（2026-09-10）—— 原建议经核对不成立，改修"看得见的那一半"**

报告原本建议"把 force 默认关掉，只在「同步最新」「打开云端存档抽屉」「手动刷新」时 force"。实施前逐条核对调用点，发现**照做会读到过期数据**：这条建议把"重拉网盘"当成了纯粹的网络开销，但

- `getCloudSaveOverview` 返回的不只是云端版本列表，还有一份**「本地 ↔ 云端」的对比状态**（`localVersionCount` / `cloudVersionCount` / `syncState`）。本地一改，这份对比就过期。
- 本页每一条会走 `refresh()` 的路径都**可能改动本地存档**，没有一条是真正的"纯本地、与存档无关"：
  - `changeKeepVersions` 看着最像纯本地操作，但 `update_save_profile_keep_versions`（`save_commands.rs:725-735`）会按新上限**真的删掉多余版本**；
  - `restoreVersion` / `deleteVersion` / `pruneVersions` 分别对应 `restore_save_version` / `delete_save_version` / `prune_save_versions`；
  - 游戏会话结束会提交新存档版本；
  - 任务失败也可能已经留下部分上传结果。
- `syncLatestSave()` **直接按 `syncState` 决定动作**（`local_ahead` → 上传、`cloud_ahead` → 还原）。拿过期状态点「同步最新」，轻则白传一版，重则把旧云端版本覆盖回本地。

所以 `force` 不能去掉。真正的问题不是"多了一次请求"，而是**这一次后台请求会把整个云端区块打成加载态**：`cloudSaveLoaded = false` 让「同步最新」变灰（`:1277`）、版本计数消失，`cloudConnectionLoading = true` 让状态徽标闪回「查询中」——而此时用户看到的只是同一份数据的旧快照。

*改法：只动"呈现"，不动"何时重新读取"*（下面这些行号会随重构漂移，定位请以函数名为准）

- `refresh()`（`:205`）仅在**还没有本游戏摘要**（`!cloudSaveLoaded || cloudOverviewGameUid !== gameUid`，`:218`）时才清空并进入加载态；已有摘要时保留它，后台照常复查。首次进入某个游戏仍是完整的加载态。
- `cloudSaveStatusText()`（`:378`）只有**还没有任何摘要**时才返回「查询中」，避免每次后台复查都把徽标刷掉。
- 「同步最新」的禁用条件补上 `cloudSaveLoading`（`:1277`）——守住那条真正的安全约束：**复查在飞时不允许基于旧 `syncState` 发起动作**。缓存从此只有展示意义，动作永远等新数据。
  - 为此 `refresh()` 里是**同步**置位 `cloudSaveLoading`，而不是等 `loadCloudSaveOverview` 去置：`refreshCloudState` 在它之前还要先读 `getBaiduStatus` / `getBaiduConfig` 两次连接状态，这几毫秒里若不置位，「同步最新」会短暂可点并基于旧 `syncState` 下手。
  - 对应的收尾也要补全：`refreshCloudState` 里**绕过** `loadCloudSaveOverview` 的两条分支（网盘未就绪、以及 `Promise.all` 抛错）都必须自己清 `cloudSaveLoading`，否则这个标志会永久卡在 `true`、「同步最新」再也点不动。

**未做**：把"本地对比"从云端读取里拆出来——本地那个版本数完全可以由已经拉到的 `versions` 现算，只有云端列表必须走网络。那需要后端新增一个只做本地对比的命令、或把对比逻辑下沉，属独立改动。

### S7 详情页 A→B 切换时封面残留 —— ✅ 已修（采用结构性修法）

```js
// GameDetailPage.vue:1001  loadCover()
function loadCover() {
  if (props.coverUrl) { coverDisplayUrl.value = props.coverUrl; return; }
  if (props.game.cover) { coverDisplayUrl.value = getGameCoverUrl(props.game.gameUid, Date.now()); return; }
  // 补上的第三个分支：两个来源都没有时显式清空
  coverDisplayUrl.value = "";
}
```

`App.vue:618` 的 `GameDetailPage` 原先没有 `:key`，所以 A→B 复用同一实例，靠 `watch(() => props.game.gameUid)` 触发 `loadCover()`。而 `props.coverUrl` 在 B 无封面时是 `undefined`（App 传的是 `coverUrls[uid]`）→ falsy → 两个分支都不进 → **B 的详情页显示 A 的封面**。

**可达路径**：在 A 的详情页时按 Ctrl+Alt+S 截 B 的封面 → `App.vue:389-403` 的 `cover-capture-ready` 处理会跳到 B 的详情页（`:390` 只在 uid 相同时提前返回）。

**落地记录（2026-09-10）—— 选了 `:key`，并顺手删掉一个已经失效的 watcher**

报告给了两个选项（补 `else`，或加 `:key`）。两个都做了，但**主修法是后者**：

- `loadCover()`（`:1001`）补上显式清空。这一条是**兜底**：`coverDisplayUrl` 的来源只有 `props.coverUrl` 与 `props.game.cover` 两处，没有第三个分支就必然残留。即便将来 `:key` 被去掉，封面也不会串。
- `App.vue:618` 加 `:key="selectedGame.gameUid"`。这是**结构性修法**：切换游戏直接重建组件实例，所有局部状态天然是新局，不再依赖一份人工维护的"该清哪些字段"清单。
- 据此**删掉了 `watch(() => props.game.gameUid)`**。它存在的唯一理由就是"父级没有 `:key`、实例会被复用"；`:key` 加上后它永远不会触发。而且那份清理清单本来就不全——清掉了 runtime / message / progress，却漏了 `coverDisplayUrl`（就是本条缺陷），也漏了 `keepVersions`（B 没有存档配置时，输入框会留着 A 的保留数）。删除处留了注释，说明**不要再退回逐项手工清理**。

**为什么不是只补 `else`**：那只能修掉封面一个字段。同一实例被复用带来的残留是一整类问题，`keepVersions` 当时就已经在漏。`:key` 把这个类一次性消掉；代价只是切换游戏时多一次挂载（原本的 watcher 路径同样要跑 `refresh()` + `loadCover()`，增量很小）。

**未做**：没有补自动化回归测试。前端没有测试框架（`package.json` 只有 `dev` / `build` / `preview` / `tauri`），要组件级验证得先引入 vitest + @vue/test-utils。本次验证方式为 `vue-tsc --noEmit`、`npm run build` 与人工复核代码路径。

### S8 任务类型白名单：两份副本，覆盖 6/20 —— ✅ 已修

后端实际产生的任务类型（`TaskService::create` 调用点统计）：

```
add_game、learn_saves、verify_save_candidates、analyze_saves、launch_game、
sync_cloud_save、set_library_root、cloud_account_sync、
install_cloud_game、delete_remote_body_package、repair_cloud_body_manifest、
upload_game_body_package、download_game_body_package、
update_game_body、package_game_body、delete_game_body_package、uninstall_game_body、
restore_save_version、delete_save_version、prune_save_versions
```

前端白名单（`App.vue:63-68` 与 `TransferCenter.vue:131`，两份逐字重复的字面量副本）只有 6 类：`upload_game_body_package` / `download_game_body_package` / `install_cloud_game` / `delete_remote_body_package` / `repair_cloud_body_manifest` / `sync_cloud_save`（最后一项由 S2 补入）。

被漏掉的里面最值得点名的是 **`restore_save_version`（恢复存档）**——高风险写操作，用户一旦离开详情页就既看不到进度、也无法取消；**`launch_game`（启动游戏）**同样不在可见列表，其取消入口因此在界面上完全够不到（S4 的进程树逻辑已修好，但没有入口）。

**修复方向**：把分类下沉到后端 `AppTask.category`（例如 `transfer` / `maintenance` / `sync`），前端只按 `category` 过滤。这样白名单只存在一份，且新增任务类型时不会漏。

前端按 `taskType` 分支的地方实际有 **7 处**，散在 2 个文件：`App.vue` 的「白名单 → 角标计数」「`launch_game` → 运行中集合」「`sync_cloud_save` → 未同步集合」，`TransferCenter.vue` 的「白名单 → 列表过滤」「类型 → 中文标题」「类型 → 图标」等。修的时候这些判断应一并收口到 `category`；标题与图标这类展示映射可以留在前端，但要基于 `category` 而非枚举 `task_type`。

必须同时做的一件事：`TransferCenter.vue` 的取消确认文案写死了「取消后会停止后续分片处理…」，一旦 `launch_game` 可见就会原样弹给「启动游戏」，必须按类型分流——S4 的取消语义是**强杀整场会话**，文案要说明可能丢进度。

S4 已完成，放开 `launch_game` 可见性的硬前置已解除。

**落地记录（2026-09-10）**

分类已下沉到后端，前端那份逐字重复的白名单已删除。

*后端*

- `domain/task.rs`：新增 `TaskCategory`（`body_transfer` / `cloud_save_sync` / `save_restore` / `session` / `maintenance`）与 `AppTask.category`。
- `TaskService::create` 增加 **必填** 的 `category` 参数——20 个任务类型、25 处调用点全部显式表态。做成必填而不是按 `task_type` 猜，是因为「新增类型时忘记分类」正是这份白名单当初漏掉 14 个类型的成因；现在漏了会直接编译失败。
- `TaskRepository::load` 给升级前的旧记录补推分类（`TaskCategory::infer`）。不补的话前端拿不到分类，历史同步任务的「存档未同步」红标会在重启后消失。

*前端*

- `taskFeed.ts` 新增全应用 **唯一** 的分类策略表（可见性 / 可取消 / 角标 / 标题 / 取消文案），用 `satisfies Record<TaskCategory, …>` 把完整性钉在编译期：后端新增分类会直接报错。
- `App.vue` 删掉 `isTransferTask` 白名单，角标、「运行中游戏」集合、「未同步」集合三处改为按 `category` 派生。
- `TransferCenter.vue` 删掉第二份重复白名单；标题与图标改为「类型映射 + 分类兜底」——此前 `taskTitle` 的兜底分支会把所有未列出的类型一律显示成「修复云端清单」。

*两个刻意的设计决定*

1. **`save_restore`（恢复存档）可见但不可取消。** 还原会先 commit 保护当前存档、再成批覆盖存档目录，中途放弃会留下写了一半的存档；要支持取消得先设计回滚，与此前登记的「提交阶段取消无效」同源。所以这一类给出的是「不可取消」的明确说明，而不是一个按了没反应的按钮。
2. **`session`（游戏会话）进列表但不计角标。** 游戏会话不是「传输」，运行中已由库卡片与详情页表达，重复计入角标只会让两处信号互相打架。

*取消文案按分类分流*：会话是「会立即结束这场游戏并终止进程，本次会话不保存为新的存档版本」；上传保留原来那句「已上传的临时分片可能由百度网盘自动清理」；其余本体操作与云同步各有独立文案。此前那句分片文案是写死给上传任务的，一旦 `launch_game` 可见就会被原样弹给「启动游戏」——而它的真实后果是丢掉整场会话。

*验证*：`cargo test` → **178 passed / 0 failed**（176 → +2：旧数据分类补齐、新建任务分类落盘）；`vue-tsc --noEmit` 通过；clippy 保持 lib 30 / lib test 34；改动文件 rustfmt 干净。

---

## 4. 低优先级 / 整洁性

### S9 死导入与死命令

`components/GameDetailPage.vue:7` 导入后从未调用：`precheckGameLaunch`、`listSaveVersions`、`listGameBodyVersions`、`getSaveProfile`、`getGameCover`（已逐个 grep 确认，均只出现在 import 行）。对应的 `api.ts` 包装（`:287/324/336/352`）唯一调用者就是这条 import，因此也都是死导出。

其中 `precheck_game_launch`（`launch_commands.rs:8-18`）是**后端注册了整条命令但前端零调用**——详情页的 precheck 实际来自 `get_game_detail_view`（`game_commands.rs:645`）。两条路径算同一件事，建议保留 `GameDetailView.precheck`、删掉独立命令与其 API 包装，减少"同一状态两个来源"的隐患。

### S10 前端零引用的字段

| 字段 | 声明 | 后端写入 | 前端读取 |
|---|---|---|---|
| `Game.cloudStatus` | `domain/game.ts:39` | 3 处（`baidu_commands.rs:1586`、`cloud_account_commands.rs:193/212`） | **0** |
| `Game.latestSaveVersionId` | `domain/game.ts:44` | `launch_service.rs:360` | **0** |
| `GameRuntime.taskId` | `domain/game.ts:53` | `launch_service.rs:164` | **0** |

前两个目前是纯存储字段（无害但会误导读者以为前端在用）。第三个则是**本可用于修复 S3 的现成钥匙**，建议在修 S3 时启用而不是删除。

`cloudStatus` 更值得留意：它语义上是「用户可见的云端状态」，而前端实际展示云端状态用的是 `getBaiduStatus` + `getCloudSaveOverview` 两套独立数据。按"用户可见状态必须少"的原则，这个字段要么真正驱动 UI（例如 S2 的失败提示），要么明确标注为内部字段。

### S11 库分页越界摘要

`App.vue:220` 用 `loaded.length`（未过滤）夹 `libraryPage`，而 `libraryPageCount`（`:135`）与 `pagedGames`（`:136-140`）基于 `filteredGames`。列表因过滤缩小后会出现"第 2 / 1 页"这类摘要，`prev` 按钮还能点但点了也不动。

不常触发（`watch([search, activeView, activeSort])` 会把页码重置为 1，覆盖了最常见路径），但在"需要处理"视图里问题被就地解决、或删除游戏导致列表缩小时可复现。修法：让 `libraryPage` 用 `computed` 从 `filteredGames.length` 派生（而不是 `ref` + 手动 clamp）。

### S12 `search` 跨页共享

`App.vue:28` 一个 `search` ref 同时绑在游戏库与游戏商店的搜索框上，且 `watch(search)`（`:191-199`）在 `activePage === 'store'` 时会去打云端搜索。结果是：在库里搜"艾尔登"→ 切到商店 → 立刻带着这个关键词请求一次云端列表。两个页面的搜索语义（本地过滤 vs 远程查询）不同，建议各持一个 ref。

---

## 5. 检查中确认「做对了」的部分

避免只报问题，以下几条经核查是正确且值得保留的：

- **锁序无环**。`get_game_detail_view`（`game_commands.rs:710-718`）先 `store` 后 `running_games`；`launch` 的 `running_games` 锁在 `store` 锁之前就已释放（`launch_service.rs:108` 前 drop），未构成交叉持锁。`save_operations` 也不与二者嵌套。
- **generation 防竞态到位**。`App.vue` 的 `gamesLoadGeneration` / `storeLoadGeneration`、`GameDetailPage.refreshGeneration`、`TransferCenter.taskLoadGeneration` 都在 await 前后做了校验，快速切页不会出现旧响应覆盖新状态。
- **`selectedGame` 的重绑定逻辑正确**。`App.vue:221-227` 在每次 `loadGames` 后按 uid 重新绑定，游戏消失时自动退回库页——避免了"详情页指向已删除对象"。
- **任务中断兜底正确**。`TaskRepository::load:46-50` 把重启后仍是 `Pending/Running` 的任务改为 `Interrupted` 并附可重试信息，不会留僵尸"进行中"任务卡住 `delete_tasks`。
- **封面缓存失效键是可靠的**。`App.vue:205` 用 `displayPath || lastPlayedAt` 做 cacheKey，而 `game_commands.rs:303` 每次保存生成新的 `cover_id`（Uuid）→ `displayPath` 必变 → 库卡片封面能刷新。
- **事件监听的卸载竞态处理正确**。`App.vue:351-356` 与 `GameDetailPage.vue:880-889` 都用 `disposed` 标志 + `.then(unlisten)` 兜住了"监听注册完成时组件已卸载"的窗口，没有泄漏。
- **`AppState::with_store_mut` 的契约完整**。锁覆盖整个闭包（`app_state.rs:136-148` 有专门测试断言），是 P1-2 之后所有读改写路径的正确基座。

---

## 6. 建议的处理顺序

| 顺序 | 事项 | 理由 |
|---|---|---|
| 1 | ~~**S1** 退出拦截（最小可用版：`CloseRequested` + 提示）~~ ✅ 已完成 2026-09-10 | 唯一一条会真实丢数据的路径，且改动面最小 |
| 2 | ~~**S3** 详情页以 `runtime` 为唯一真相源 + 用 `runtime.taskId` 接管轮询~~ ✅ 已完成 2026-09-10 | 修掉用户天天能碰到的错误按钮；同时为 3/4 奠定基础设施 |
| 3 | ~~**S2** 自动同步纳入可见范围 + `finish_sync` 改用 `TaskService::finish`~~ ✅ 已完成 2026-09-10（方案 A） | 把静默失败变成显式失败，成本低 |
| 4 | ~~**S5** 引入后端事件推送，前端轮询降级为兜底~~ ✅ 已完成 2026-09-10 | 一次投入，长期受益；S8 的分类筛选、任务状态的实时刷新都依赖它 |
| 5 | ~~**S4** 取消启动任务需清理整棵进程树~~ ✅ 已完成 2026-09-10 | 放开可见性的硬前置：不修它，S8 一放开就会在真机上对着仍在写盘的存档做快照 |
| 6 | ~~**S8** 任务分类下沉后端（`AppTask.category`）~~ ✅ 已完成 2026-09-10 | 消除前后端两份白名单；`launch_game` 与 `restore_save_version` 已可见，取消确认文案按分类分流 |
| 7 | ~~**S1（完整版）** 启动时会话重建~~ ✅ **已完成（2026-09-10）** | 启动时扫描游戏目录、接回仍在运行的会话，兑现「退出即提交」；剩下的「退出前等待在途任务」另计 |
| 8 | S9-S12（S6 / S7 已于 2026-09-10 完成） | 独立小修，可随时插入 |

---

## 附：本次检查使用的验证手段

```bash
# 确认没有任何窗口/退出事件处理（结论：零匹配）
grep -rn "on_window_event|CloseRequested|ExitRequested|RunEvent" src-tauri/src

# 确认 running_games 为纯内存、无启动恢复（结论：app_state.rs 中无持久化；lib.rs setup 无相关逻辑）
grep -rn "running_games" src-tauri/src

# 确认任务类型全集（结论：20 类，前端白名单覆盖 5 类）
grep -rn "TaskService::create(" src-tauri/src/commands src-tauri/src/services

# 确认前端死导入（结论：5 个符号仅在 import 行出现）
grep -c "precheckGameLaunch|listSaveVersions|listGameBodyVersions|getSaveProfile" src/components/GameDetailPage.vue

# 确认前端零引用字段（结论：cloudStatus / latestSaveVersionId / GameRuntime.taskId 均无读取点）
grep -rn "cloudStatus|latestSaveVersionId" src/
```

> 本报告只做检查与结论，**未改动任何代码**。
