# 平台档案云端同步与手动恢复审查

日期：2026-11-15。

项目：GameSaverNext 1.0.31，基于 `ff98c26` 加当前工作区。此前云存档模块的未提交修复保持原状。

范围：平台设置中的“上传本机档案”“从云端恢复”、游戏与存档配置合并、跨设备路径重建、设置落盘和前端任务刷新。初次审查仅生成复现材料；同日按用户要求完成下述修复。全程未操作真实游戏库、存档、百度凭证或云端文件。

## 修复结果（2026-11-15）

7 项均已完成代码修复。原问题、复现说明和行号保留在后文，均针对修复前状态，不代表仍然存在。

| 编号 | 已实现的行为 |
| --- | --- |
| CA1 | 存档配置只按映射后的本地游戏归属选择，保留已有本地配置 ID；新配置生成本地 ID。远端 profileId 不再用于移除本地其他游戏的配置。本地 ID 本身已跨游戏冲突时拒绝恢复。 |
| CA2 | 云端范围补充可选 publicDocuments 标识，公共文档恢复到当前设备的公共文档根；个人文档仍使用个人根。校验拒绝将该标识用于其他根类型。路径比较复用 verbatim 前缀处理。 |
| CA3 | 游戏只按归一化 gameKey 匹配，源设备 UID 仅用于档案内部映射；校验拒绝归一化后重复的 key、同游戏重复配置。合并在候选 store 中完成，失败不提交部分结果。 |
| CA4 | 档案恢复使用现有操作集合中的全局独占标识；开始时拒绝游戏运行、学习会话、存档/本体操作、游戏库迁移及其他活跃任务。任务创建与独占认领共用操作锁，恢复期间阻止新任务和游戏操作，存档确认入口同步检查。任务建立失败、工作线程普通失败及捕获到的内部异常均释放占用。 |
| CA5 | 百度配置仓库统一串行化读改写，凭证和所有设置入口均从锁内最新值更新指定字段。档案恢复的配置修改与游戏库提交协调，提交失败回滚配置；回滚失败一并报告，不再静默丢弃。 |
| CA6 | 设置页改用现有共享任务流，移除账号档案独立定时轮询；IPC 失败后由共享流自动重试并显示跟踪警告。重新进入页面可接续活动任务，终态恢复按钮。 |
| CA7 | 恢复任务采用 restore_cloud_account 类型；成功后重新读取平台配置和账号状态，根组件使游戏库缓存失效并强制刷新。上传成功不会重复覆盖本地设置，历史终态不会反复执行成功处理。 |

合并与路径重建实现在 `src-tauri/src/services/cloud_account_service/restore.rs`，命令层只保留任务编排和持久化协调；未改变正式存档版本、CAS、云存档包或本体安装接口。

验证：

- Rust 全量单元测试：389 通过、0 失败、2 忽略，共 391 项。
- `cargo check --manifest-path .\src-tauri\Cargo.toml --offline` 通过。
- `cargo test --manifest-path .\src-tauri\Cargo.toml --no-run --lib --offline` 通过。
- `npm run build` 通过，包含 Vue/TypeScript 类型检查。
- 修复后前端隔离验证 7 个场景通过，使用实际 Vue setup、共享任务流和 App 任务 watcher，覆盖重进页面接续、暂时 IPC 失败后恢复、平台配置刷新、成功终态只处理一次、失败终态解锁、旧终态不重播和游戏库缓存失效。
- 新增 Rust 覆盖身份冲突、重复导入、归一化重复 key、公共文档往返、无效公共标识、配置并发字段更新、提交失败回滚及回滚失败、无本地凭证、互斥抢占竞争、运行/学习/迁移/活跃任务拦截、任务建立失败释放占用。
- 定向 rustfmt 检查与 `git diff --check` 通过。

修复备份和验证日志：`.perf/cloud-account-fix-20261115/`。该目录的前端验证使用 IPC 和定时器替身，不等于真实窗口或百度网盘端到端测试；本轮未构建安装包、提交 Git 或推送远端。

旧云端档案若已丢失公共文档来源信息，无法从相同的 Documents + subPath 无损推断回来；应在配置正确的原设备重新上传一次平台档案，以写入新标识。现有不带标识的普通个人文档档案仍按原语义读取。

## 结论

发现 7 项待修复问题，其中 1 项 P1、6 项 P2。4 个后端缺陷场景和 2 个前端缺陷场景已在隔离环境复现；其他并发问题来自明确的调用及写入交错分析，尚未在真实应用中制造并发复现。

| 编号 | 优先级 | 问题 | 证据 |
| --- | --- | --- | --- |
| CA1 | P1 | 云端 profileId 与本地其他游戏冲突时，合并会移除其他游戏的存档保护配置。 | 隔离 Rust 复现 |
| CA2 | P2 | 公共文档存档范围恢复成当前用户的个人文档目录。 | 隔离 Rust 复现、学习扫描调用链 |
| CA3 | P2 | 游戏关联以 UID 或 gameKey 任一命中即覆盖；大小写等价的重复 gameKey 也能通过校验。 | 两个隔离 Rust 复现 |
| CA4 | P2 | 档案恢复未与运行中游戏、存档操作及游戏库迁移协调。 | 调用链、确定的状态交错 |
| CA5 | P2 | 恢复自动上传开关时写回整个旧百度配置，可能覆盖同期修改的其他设置或凭证。 | 读改写交错分析 |
| CA6 | P2 | 定时轮询的一次 IPC 失败会停止后续轮询，并让本页同步按钮一直禁用。 | 隔离前端函数复现 |
| CA7 | P2 | 恢复完成未刷新平台设置和游戏库缓存，界面可继续展示旧开关与游戏信息。 | 隔离前端函数复现、Vue 调用链 |

## CA1：导入配置 ID 未做本地归属检查

位置：`src-tauri/src/commands/cloud_account_commands.rs:253`、`:287`、`:296`；`src-tauri/src/services/cloud_account_service.rs:461`。

合并时查找已有配置的条件为 `profile_id == incoming.profile_id || game_uid == local_uid`，随后通过 retain 同时移除相同 ID 或目标游戏的配置，最后沿用远端 profileId。输入校验只检查云端档案内部的 ID 唯一性，没有检查该 ID 在本地是否属于其他游戏。

复现：本地游戏 A 使用配置 P，本地游戏 B 使用配置 Q；传入 B 的合法云端配置恰好也叫 P。校验通过，合并后 A 的配置被移除，A.saveProfileId 仍指向 P，而 P 已属于 B。A 无法再解析自己的保护配置。`LaunchService::precheck` 不因缺少保护配置禁止启动，`session_inputs_from_store` 按游戏 UID 和配置 ID 双重匹配，因而不能找回 A 原有配置。

这是人为构造的本地/远端身份冲突复现，不代表正常生成的随机 UUID 经常碰撞。问题在于面对冲突时会修改无关游戏，而不是安全拒绝或重新分配本地 ID。

建议：配置身份按已映射的本地游戏归属解析；更新本游戏配置时保留其本地 ID，新建时生成本地 ID，或至少在发现跨游戏 ID 冲突时拒绝整次恢复。不得按远端 ID 删除其他游戏的配置。

## CA2：公共文档范围被重建为个人文档范围

位置：`src-tauri/src/services/save_learning_service.rs:732`、`:740`；`src-tauri/src/services/cloud_account_service.rs:334`、`:362`；`src-tauri/src/commands/cloud_account_commands.rs:316`。

学习扫描会把 `%PUBLIC%\Documents` 下找到的目录标记为 `SaveRootType::Documents`。导出时通过 `\documents\` 标记提取相对路径，未保留公共文档这一来源；导入时却一律使用 `%USERPROFILE%\Documents`。

复现：`C:\Users\Public\Documents\ReviewGame\Save` 导出为 Documents + `ReviewGame/Save`，再恢复为当前用户个人文档下的同名目录。普通 LocalLow、受管目录和个人 Documents 子路径现有测试通过，不能覆盖这个分支。后果是已恢复配置指向错误目录，后续保护和恢复无法命中原公共存档。

建议：让公共文档与个人文档在跨设备表示中可区分，并按目标设备各自的标准根重建。无法可靠表示的来源不得静默推断为另一个系统目录。

## CA3：gameKey 关联规则与校验归一化不一致

位置：`src-tauri/src/commands/cloud_account_commands.rs:182`、`:190`；`src-tauri/src/services/cloud_account_service.rs:444`、`:449`。

场景一：本地 A 的 UID 与远端游戏相同，但 gameKey 不同；本地另有真正匹配远端 gameKey 的 B。当前 `position(gameKey 匹配 || UID 匹配)` 会由数组顺序决定覆盖谁。隔离测试中 A 被改成 B 的 gameKey，留下两个相同 gameKey 的本地记录。不能把源设备 UID 当作可优先覆盖另一个本地游戏的理由。

场景二：档案包含 `Review Game` 与 `review game` 两个不同原始 key。校验按原字符串去重，能够通过；合并又将 key 转小写，最终只留下一个游戏，后一个覆盖前一个。现有 build 通常产出归一化 key，该复现验证的是导入边界对冲突输入的处理。

建议：校验与合并共用 gameKey 归一化，并拒绝归一化后的重复项。以 gameKey 明确决定目标归属；如果保留 UID 辅助信息，只允许验证一致性，冲突时失败，不能通过 OR 选择任一记录。确认文案也应说明以游戏标识匹配，而非只说“同 UID”。

## CA4：档案恢复不参与现有操作互斥

位置：`src-tauri/src/commands/cloud_account_commands.rs:143`、`:145`、`:359`；`src-tauri/src/commands/library_commands.rs:43`、`:190`；`src-tauri/src/services/launch_service.rs:118`、`:373`。

`begin_sync` 只保护另一场账号档案同步，不检查或认领运行会话、存档操作、学习会话或游戏库迁移。合并拿到 store 锁只能保证内存集合更新互斥，不能使其与已经读取了旧游戏配置的后台操作互斥。

两条可由当前调用顺序推出的交错：

- 游戏会话启动时已克隆旧存档配置；运行期间账号恢复替换该配置，游戏退出后仍按旧配置提交。此时界面配置与本场退出提交采用不同范围。
- 档案恢复先取得旧 games_root，随后游戏库迁移完成路径重写；恢复再进入合并，为新增的云端待修复记录填入旧根目录。迁移与账号同步各自的状态标志互不检查，store 锁不能重新验证提前获取的路径。

本项为静态交错分析，未启动真实游戏或搬迁真实库目录。没有证据表明单独一次档案恢复会直接复制、删除或覆盖游戏本体。

建议：恢复前和提交前检查冲突操作，使用现有职责范围合适的互斥机制认领；游戏库迁移期间拒绝恢复，提交前确认库根未变化。不要为此引入另一套完整任务调度框架。

## CA5：单个设置恢复会写回过期的整份配置

位置：`src-tauri/src/commands/cloud_account_commands.rs:144`、`:147`、`:153`；`src-tauri/src/commands/baidu_config_commands.rs:27`、`:62`、`:72`。

账号恢复在进入 store 锁前加载 `old_config`，之后仅修改 auto_upload_body，但保存的是整个结构。其他配置命令也独立加载、修改、保存，没有共同的读改写锁。

例如恢复线程读取配置 A 后等待 store 锁；用户将自动同步存档设置改成 B，或更新 AppKey/SecretKey；恢复线程随后保存 A + 云端自动上传开关，会把无关字段退回 A。`cloud_account_sync` 的锁不能阻止普通配置命令。失败回滚同样使用该旧快照，且当前忽略配置回滚错误。

本项未读写真实百度配置，结论来自加载与保存的明确时序，不是已执行的并发端到端测试。

建议：在配置仓库内串行化“读取最新值—只更新指定字段—原子保存”，让所有配置写入口共用。协调游戏库与配置提交的失败处理，并报告回滚失败，不能仅依靠单文件原子替换避免丢失更新。

## CA6：轮询失败后任务在当前页面无法收尾

位置：`src/components/PlatformSettings.vue:72`、`:87`。

首次 watchCloudAccountTask 被 syncCloudAccount 的 try/catch 包住，但定时器里的后续调用是 `void watchCloudAccountTask(taskId)`，没有异常处理。一次 getTask 拒绝后，不再创建下一次定时器，cloudAccountTaskId 仍非空；上传和恢复按钮因该 ID 保持禁用。

隔离执行原 TypeScript 函数：第一次 getTask 返回 running，第二次模拟临时 IPC 失败，得到 1 次未处理 Promise rejection、0 个后续定时器、未清空的 taskId。不能据此断言后端任务失败；后端可能早已成功，失效的是本页跟踪。

建议：轮询本身捕获读取错误并进行有界重试，明确展示跟踪失败；后端结果与查询失败分开处理。页面重新进入时可从现有任务流恢复跟踪，避免再发起相同任务。

## CA7：恢复成功后显示的开关和游戏信息仍可能是旧值

位置：`src/components/PlatformSettings.vue:76`、`:80`、`:151`；`src/App.vue:313`、`:371`、`:766`。

成功分支只重新读取 cloudAccount 状态，没有重新读取 getBaiduConfig。隔离测试模拟云端将自动上传恢复为 false，函数成功结束后 config.autoUploadBody 仍为 true。页面显示与实际行为不一致，用户再点击开关时也以旧值取反。

此外，PlatformSettings 没有向父层发出恢复完成事件，App 对任务终态的结构性刷新只处理会话结束和 install_cloud_game，未处理 cloud_account_sync。缓存未到期时，恢复后返回游戏库仍可看到旧名称等信息；缓存刷新后才更新，不应描述为数据必然未写入。

建议：区分档案上传与恢复任务，恢复成功后重读受影响的平台配置，并使游戏列表/详情缓存失效。通过现有任务流或明确的完成事件协调刷新，避免只能重启或等待 TTL。

## 已验证的正常行为与范围边界

- 既有云端档案相关 Rust 测试 11 个全部通过；包含常规路径重建、启动路径和远端包路径校验、内部路径穿越拒绝、非 Active 游戏与无远端路径本体版本的排除。
- 档案由白名单 DTO 构建，不直接导出百度凭证、token、日志、任务记录或 managedPath。
- 自定义存档目录目前会按设计携带绝对路径；本报告未将这一既有语义误报成凭证泄露，也未宣称其可在新设备自动定位。
- 配置与游戏库都已使用单文件原子落盘；本报告的 CA5 是跨入口读改写与失败协调问题，不是说文件必然会被写成半个 JSON。
- 未将尚无具体可见后果的冗余字段、没有端到端证据的网络猜测列为确认缺陷。

## 验证材料

目录：`.perf/cloud-account-review-20261115/`。

- `generate.cjs` 从当前源码提取 build、校验、路径和合并函数；领域类型直接引用项目源码。`provenance.json` 记录来源哈希。
- `Cargo.toml`、`harness.rs` 为独立审查程序；`reproduction.log` 记录 4 个 Rust 场景成功复现。
- `frontend-reproduction.cjs` 提取真实轮询函数，使用 TypeScript 编译器转换，在隔离 ref、IPC 与定时器替身中运行；`frontend-results.json` 记录 2 个前端复现场景。
- `existing-tests.log` 记录项目现有 11 个相关单元测试通过。

执行命令：

```powershell
node .perf/cloud-account-review-20261115/generate.cjs
cargo test --manifest-path .perf/cloud-account-review-20261115/Cargo.toml --offline --target-dir src-tauri/target --lib review_tests -- --nocapture
node .perf/cloud-account-review-20261115/frontend-reproduction.cjs
cargo test --manifest-path .\src-tauri\Cargo.toml --lib --offline cloud_account -- --nocapture
git diff --check
```

“复现通过”表示断言成功观察到当前缺陷，不是已经修复或回归通过。审查程序首次编译缺少 serde_json 依赖，确认错误后已在独立 Cargo.toml 补齐，再次执行成功。未运行真实百度网络测试、挂载 Vue 页面或修改用户数据；未提交 Git、升级版本、推送远端或构建安装包。
