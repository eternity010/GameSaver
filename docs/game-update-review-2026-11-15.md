# 游戏本体更新模块审查

日期：2026-11-15。项目：GameSaverNext 1.0.31，基于当前工作区；保留此前云端安装、云存档、平台档案、目录迁移和游戏删除修复。

范围：游戏详情页“更新游戏本体”，从选取新版目录到预检、暂存复制、保护存档、交换目录、提交记录、清理旧本体，以及重启恢复、任务取消和界面反馈。

本报告先记录审查，再补充同日修复结果。下文“原始审查结论”及 GU1–GU6 的位置、复现和旧验证结果均描述修复前状态，行号不代表当前源码位置。

## 修复结果（2026-11-15）

6 项问题均已修复，并完成针对性回归和项目全量测试。本轮没有更新或删除真实用户游戏，没有操作真实存档或网盘，没有构建安装包、提交或推送 Git；现有 1.0.31 安装包不包含本轮修改。

| 编号 | 修复结果 |
| --- | --- |
| GU1 | 恢复失败保留事务日志及其暂存、新旧本体；启动清理跳过未完成事务。为游戏保留独占状态，阻止冲突操作；恢复或清理完成后才解锁。 |
| GU2 | 正式目标限定为当前库的 `games/<gameUid>`，历史目标限定为 `.versions/<gameUid>/<versionId>`；拒绝父级跳转、跨游戏日志、符号链接和 junction，恢复时还核对真实库位置。 |
| GU3 | 复用已有 Windows 路径规范化，统一 `\\?\` 形式；对真实来源和库目录检查双向重叠，复制前再次确认。 |
| GU4 | 使用现有 `TaskService::begin_commit` 在交换前原子检查取消并关闭取消入口；预检和复制阶段仍可取消，界面文案同步说明边界。 |
| GU5 | 清理或元数据保存失败直接显示为可见失败任务并保留重试信息；新增“恢复游戏更新与清理”入口，仅处理现有事务，不重新复制来源目录。 |
| GU6 | Tauri 命令仅做轻量准入、加载上下文和创建任务；新版目录扫描、空间检查和复制均在后台执行，预检显示文件计数并响应取消。 |

提交与恢复使用 Prepared、Committed、Complete 阶段：旧本体清理前先持久化 Committed；即使旧目录已删除、记录或日志清理随后失败，重试仍保留已提交的新本体，不会按未提交事务回滚。正常更新成功后清理完整旧本体；失败时只保留当前恢复所需现场，不增加长期本体副本。

复制阶段也会检查链接及来源文件数量、总字节变化。这里仍采用轻量元数据检查，不增加逐游戏文件 SHA-256，也不声称可以检测所有同大小的并发内容修改。

### 实现边界

- `services/game_body_update_service.rs`：目录预检、复制、交换、路径归属、事务日志及幂等恢复。
- `services/game_body_update_service/workflow.rs`：更新、存档保护接口调用、元数据提交、任务收尾和恢复重试。
- `commands/game_body_commands.rs` 与 `lib.rs`：命令准入、后台任务及启动恢复装配；新重试命令已注册。
- `AppState`：复用原有操作集合，保留未完成更新的独占状态；没有引入另一套全局锁服务。
- 传输中心：更新和恢复任务可见、重试 API 已接通；提交阶段禁止取消，终态使游戏库缓存失效。

### 修复后验证

| 检查 | 结果 |
| --- | --- |
| `cargo check --manifest-path .\src-tauri\Cargo.toml --offline` | 通过 |
| `cargo test --manifest-path .\src-tauri\Cargo.toml --offline --lib` | 445 通过，0 失败，2 项原有忽略，共 447 项 |
| 本体更新模块针对性单元测试 | 21 通过，含原 8 项及新增 13 项 |
| `cargo test --manifest-path .\src-tauri\Cargo.toml --offline --no-run --lib` | 通过 |
| 完整更新工作流隔离回归 | 9 通过 |
| 更新相关前端隔离回归 | 7 通过 |
| 上轮删除模块前端隔离回归 | 8 通过 |
| `npm run build` | TypeScript 检查与 Vite 构建通过 |
| `git diff --check` | 通过 |

工作流回归覆盖：正常提交清理、预检与复制取消、提交期拒绝取消、库外目标拒绝、主记录保存失败回滚、Windows 文件占用导致旧本体清理失败及重试、清理元数据保存失败、清理元数据后日志写入失败、重启恢复任务去重。文件占用采用真实 Windows 文件句柄，目录复制、交换、清理、任务状态和 AppState 使用真实实现；存档保护/恢复仅使用隔离进度适配器，配置保存使用隔离 JSON 和故障注入。因此这些结果不等同于真实 CAS 存档、Tauri 窗口或安装包端到端测试。

全量测试首次运行发现新增 junction 夹具向 `cmd mklink` 传入混合分隔符，报 `Invalid switch - "game-1"`；确认 stderr 后将夹具改为分段 `PathBuf::join`，再执行上述针对性和全量测试，全部通过。没有修改业务判断以绕过断言。

修复前文件备份、最终日志、隔离夹具与来源哈希均保存在 `.perf/game-update-fix-20261115/`；关键结果为 `final-check.log`、`final-tests.log`、`final-no-run.log`、`final-build.log`、`workflow-regression.log`、`frontend-results.json` 和 `source-map.json`。

## 原始审查结论（修复前）

发现 6 项需要处理的问题。应先修复恢复失败后的清理行为和路径边界，再处理取消、清理提示和预检阻塞。

| 编号 | 级别 | 问题 | 证据 |
| --- | --- | --- | --- |
| GU1 | P1 | 更新恢复失败后，仍继续清理该事务的暂存目录和旧本体；失败事务也没有像云端安装、删除事务一样持续锁定。 | 实际恢复与清理函数的磁盘现场复现；启动接线审查 |
| GU2 | P1 | 更新目标直接信任 managed_path，历史本体清理也未限制到该游戏的 .versions 子目录。 | 原样更新 worker 覆盖隔离库外目录；实际清理函数删除活动游戏目录和 ../ 指向的隔离外部目录 |
| GU3 | P2 | 路径比较没有统一 Windows 的 \\?\ 前缀，可绕过源目录与受管库的重叠检查。 | 实际命令辅助函数与 validate_source 的 Windows 测试 |
| GU4 | P2 | 本体已经交换、正在恢复存档时仍接受取消，但任务会继续提交成功。 | 原样 worker、实际 TaskService，在恢复进度处注入取消 |
| GU5 | P2 | 旧本体清理失败仍显示普通成功，整份旧本体可能继续占空间，界面没有提示或清理入口。 | Windows 文件占用；实际 worker 返回隐藏的 oldBodyCleanupPending |
| GU6 | P2 | 在同步 Tauri 命令中递归扫描全部新版目录，完成后才创建任务和启动后台线程。 | 前后端调用顺序与 WalkDir 实现审查 |

隔离复现 8 个场景全部命中上述缺陷；“测试通过”表示缺陷被成功重现，不表示问题已经修好。项目原有的 8 个本体更新单元测试仍通过，它们主要覆盖正常交换和正常恢复，尚未覆盖这些异常边界。

## GU1：恢复失败后仍清理恢复现场（P1）

位置：`src-tauri/src/services/game_body_update_service.rs:89`、`:93`、`:408`；`src-tauri/src/lib.rs:195`、`:200`、`:229`。

`recover_pending_updates` 将单个日志的解析、路径检查或恢复错误收集起来后，仍无条件执行 `cleanup_stale_update_artifacts`。后者按 `.<uid>.updating` 命名删除暂存目录，不区分目录是否仍被失败的事务引用；即使正式本体不存在，也照样删除。

随后启动入口仅记录恢复错误，继续调用 `cleanup_archived_body_versions`。失败事务的 archive_path 仍可能被这一步当成普通历史本体清掉。启动后也没有为失败的更新事务注册持续阻止冲突操作的状态；现有持续锁定只接入了云端安装、删除与迁移等流程。

复现一：创建含完整示例本体的 `.game-1.updating` 与损坏的 `.game-1.update.json`。实际恢复函数返回错误，日志还在，但暂存本体已经被清理。

复现二：创建已提交事务，正式路径被只读普通文件占据，暂存目录含新版，archive 含旧版，版本记录引用该 archive。实际恢复因路径冲突失败，但内部清理删除暂存新版；再按启动入口顺序执行历史清理，旧版也被删除。最后只剩冲突文件与事务日志，两个可用本体都不在了。只读属性只施加于隔离夹具，测试已恢复属性后清理。

边界：第二个场景人为构造了正式目录被异常文件占据的现场，不表示正常更新一定丢失本体。第一种“日志不能读取却仍删关联暂存”的行为不需要这个前提。它们共同说明恢复失败时没有保护现场的边界。

建议：从失败事务保留其全部目录，跳过关联历史清理，并沿用现有独占状态机制禁止冲突操作，直至重试或人工处理完成。不要为此额外复制整份本体，也不要仅靠忽略恢复错误继续启动。

## GU2：本体更新与旧本体清理的路径归属不足（P1）

位置：`src-tauri/src/commands/game_body_commands.rs:620`；`src-tauri/src/services/game_body_update_service.rs:145`、`:155`、`:452`。

更新 worker 从记录中的 managed_path 取父目录作为 games_root，没有核对它是否为当前配置库的 `games/<gameUid>`。前置校验只核对新版来源与库目录的重叠，不验证写入目标的归属。

隔离复现：保留配置库为 `fixture/library`，将游戏记录的 managed_path 改为 `fixture/outside/game-1`。原样 worker 成功以新目录覆盖了该库外目标。该复现使用原样更新业务流程；存档提交/恢复和配置保存使用隔离适配器，没有访问真实 Tauri 环境。

历史清理的 `path_is_within` 只做字符串前缀比较，既没有要求目标为 `.versions/<gameUid>/<versionId>`，也没有消解 `..`。实际复现了两种删除：

- 将另一活动游戏目录写入 archive_path，清理函数接受并删除该目录。
- 将 `games/.versions/../../outside` 写入 archive_path，清理函数接受并删除夹具内、games 根外的 outside 目录。

触发前提是游戏或版本记录的路径异常；不应将其描述成正常生成的记录必然越界。破坏性文件操作仍应在执行前重新验证归属，而不能让异常记录直接决定覆盖或删除目标。

建议：复用现有受管路径领域校验，分别约束正式目录、暂存目录、事务路径和历史目录；拒绝父级跳转及重解析路径。清理范围只限该游戏已确认、已提交且不再用于恢复的旧本体。

## GU3：Windows 路径形式不一致导致来源重叠检查漏判（P2）

位置：`src-tauri/src/commands/game_body_commands.rs:76`、`:952`；`src-tauri/src/services/game_body_update_service.rs:175`、`:490`、`:496`。

命令层对用户传入的 source_path 与当前 games_root 做字符串比较；服务层只将 source canonicalize，而 managed_path 仍保持记录中的形式。Windows 的 canonicalize 通常得到带 `\\?\` 的路径，normalize_path 只统一分隔符和大小写，没有去除此差异或解析目录别名。

复现直接使用同一真实游戏目录的 canonical 路径：实际命令辅助函数误判它不与受管 games_root 重叠，实际 validate_source 也接受它。两个路径指向同一个目录，没有使用伪造的不存在路径。

这使“新版来源不得位于受管库内”的保护能够被不同形式的同一路径绕过；如果来源实际指向整个库，而暂存目录又建在库内，复制还可能把自己的输出纳入来源遍历。

边界：这里只执行了预检，没有尝试触发递归复制或制造大量文件；递归风险根据 WalkDir 的输入目录与暂存位置推导。

建议：两侧采用同一套现有 Windows 路径标准化与真实归属检查，在创建暂存目录前确认源与目标不重叠。

## GU4：提交阶段取消请求被接受但不生效（P2）

位置：`src-tauri/src/commands/game_body_commands.rs:680`、`:701`、`:714`、`:120`；`src-tauri/src/services/task_service.rs:175`、`:204`；`src/taskFeed.ts:27`。

worker 最后一次检查取消位于写事务及交换目录之前。本体交换、存档恢复和记录提交期间没有调用现有的 `TaskService::begin_commit`，任务的 cancellable 也没有设为 false。传输中心按照 BodyTransfer 分类继续允许取消，后端 cancel 会接受请求。

复现使用原样 worker 和实际 TaskService，在交换已完成、恢复存档回调到达时调用 cancel。请求返回成功，cancel_requested 为 true，但 worker 仍正常返回 Ok，正式 game.exe 已替换为新版。外层线程对这一 Ok 固定标记为 Success。

测试中的存档恢复使用只触发进度的适配器，因此此处证明的是取消与提交控制流，不声称已经端到端验证真实存档恢复。

建议：以现有 begin_commit 原子检查取消并关闭取消入口，然后才进行不能安全中断的目录交换。不要在提交已开始后简单增加取消判断就直接退出，否则会留下更难恢复的半完成状态。

## GU5：旧本体清理失败只藏在结果里（P2）

位置：`src-tauri/src/commands/game_body_commands.rs:118`、`:833`、`:847`、`:857`；`src-tauri/src/domain/task.rs:112`；`src/components/GameDetailPage.vue:493`；`src/components/TransferCenter.vue:284`。

更新成功后清理旧本体失败，只记录日志并返回 `oldBodyCleanupPending=true`，外层任务仍写入普通成功和“游戏本体更新完成”。任务列表 DTO 有意不传 result；详情页成功分支也不读取这个标志。

Windows 隔离复现：在交换后的旧 archive/game.exe 上持有不共享删除的文件句柄，令实际清理失败。实际 worker 返回 Ok，标志为 true，更新日志及旧本体仍在。新本体可正常读取，但用户界面不能从普通成功文案知道还有完整旧本体没有释放。

边界：下一次启动可能成功清掉这份旧本体，不能称其为永久残留。本轮没有把问题扩大成需要长期历史本体备份；用户明确要求不保留这种占空间的副本。

另一个同领域的错误传播问题：清理后记录保存失败被捕获并转换为 Ok，可能清除事务但保留指向已删除 archive 的旧记录。这一分支做了代码审查，未纳入上述文件占用复现。

建议：区分“更新已完成”和“旧本体清理未完成”，将原因与重试入口放在现有可见任务信息中，并保留小型恢复记录；无需额外复制本体或扩大任务列表 result。

## GU6：新版目录预检发生在任务创建之前（P2）

位置：`src-tauri/src/commands/game_body_commands.rs:65`、`:80`、`:90`、`:105`；`src-tauri/src/services/game_body_update_service.rs:193`；`src/components/GameDetailPage.vue:637`。

update_game_body 是同步 Tauri 命令，先执行 validate_source；后者通过 WalkDir 递归访问整个新版目录并读取每个文件元数据，最后才返回到命令创建任务并起工作线程。

因此大游戏、海量小文件或慢盘会在“准备更新”阶段等待完整扫描，此时没有可见的任务 ID、扫描进度和任务取消入口。后续复制还会再次遍历同一来源。该发现来自调用顺序审查，没有为报告额外扫描用户游戏或假称测得具体卡顿秒数。

建议：让命令只做轻量准入、创建任务；将目录预检和空间检查放入同一个后台任务，提供扫描阶段进度与取消。继续采用轻量元数据检查，不增加逐游戏文件 SHA-256。

## 原始验证与局限（修复前）

- 项目原有测试：`cargo test --manifest-path .\src-tauri\Cargo.toml --lib --offline game_body_update_service -- --test-threads=1`，8 通过。
- 隔离复现：`cargo test --manifest-path .\.perf\game-update-review-20261115\Cargo.toml --target-dir .\src-tauri\target --offline audit:: -- --test-threads=1`，8 通过。
- 复现直接引用当前领域模型、AppState、磁盘空间和更新服务；worker 和 TaskService 在 prepare.cjs 中按完整函数边界原样提取，source-map.json 记录出处与 SHA-256。
- worker 夹具仅用隔离文件模拟存档接口及配置保存，不验证真实 CAS 对象、真实外部存档或 Tauri 窗口；目录复制、交换、恢复清理、Windows 文件占用及 TaskService 取消使用实际实现。
- 一次初始复现错误地假设 Windows 目录 rename 一定拒绝覆盖普通文件，断言未命中。随后确认真实行为，改用只读目标文件制造确定的拒绝，重新运行后全部通过；没有修改业务代码让断言通过。
- 初始审查尚未修复以上问题；同日后续修复及验证结果见本报告开头。1.0.31 安装包仍未重新构建。

日志与夹具：`.perf/game-update-review-20261115/`，包含 `rust-reproduction.log`、`existing-update-tests.log`、`harness.rs`、`worker.rs`、`task_service.rs`、`prepare.cjs` 与 `source-map.json`。
