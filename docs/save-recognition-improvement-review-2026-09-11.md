# 存档识别与规则生成：性能 / 精准度 / 自动化 审视

> 2026-09-11。范围：`services/save_learning_service.rs`（2696 → 现 3555 行）、`services/learning/{etw_capture,native_etw,transactions}.rs`、`repositories/save_repository.rs`（2724 → 现 3531 行）、`domain/save_profile.rs`。
> **行数已按 2026-09-13 实测更新**（本行原记 2696 / 2724）。原记并非随手估的：`save_repository.rs` 的 2724 与审视当时那一版（`00ad9ee`，2026-09-10）**逐行吻合**；`save_learning_service.rs` 的 2696 略高于该版实测的 2514 —— 应是审视当时尚未提交的中间状态。该中间态已不可回溯：`.git` 曾在 2026-09-12 损坏，丢失的工作由 `4c4fa55` 重新落地（`00ad9ee` 是其父提交）。标注方式沿用本文惯例：保留审视当时的数字，另附现值。
> 目标按主人给的三个方向：**识别性能与精准度**、**规则的性能与精准度**、**提高自动化程度**，并要求覆盖不同经典游戏类型。
> 本文只做审视与建议，**未改任何代码**。

---

## 0. 结论先行

主干是对的，不需要重做：ETW 写入证据 + 快照差异双路、容器名启发式、明确的包含/排除规则、显式文件清单。问题出在**边界的口径**上，而且当前设计同时存在两个相反的失效方向：

> **容器目录被判定时，规则是「整目录收集」；没被判定时，规则是「只认当时看到的文件」。**
> 于是同一套模型下，一边可能把**别人的存档**收进版本库，一边又**收不到自己新增的存档**。

按收益排序，建议先做 4 项：**R1（ProgramData 缺失）→ R2（新文件不纳入）→ R3（事务口径）→ P1/P2（两处性能）**。R1、R3、P1、P2 都是小改；R2 是结构性改动，收益最大但需要设计守卫。**这 4 项已全部落地**（分三批，见第 5 节）。

> **第 1 批（R1 + P1/P2）与第 2 批（R3）已于 2026-09-11 实施，第 3 批（R2 + R4）与第 3.5 批（R2b）已于 2026-09-12 实施**，落地记录与变异结果见第 5 节。下面各条保留审视当时的原文，并在末尾补上「已解决」说明。
> **A2、R6 已于 2026-09-12 实施**（第 4 批与备选批的一部分）。
> **A1 已于 2026-09-13 部分实施**：只做了第 4 项「允许跳过学习」（只读初稿 + 前端旁路），profile 形状复用与保存后自动分析**未做**，目录推断前置只算部分落地 —— 取舍理由见 A1 条。
> **A3 已于 2026-09-13 实施**：只做「说清病因 + 指明明路」，**不做自动猜测**（理由见 A3 条）。
> **R5 已于 2026-09-13 结案（不做）**：按「暂不考虑非管理员情况」的前提，它的两个半边分别是「不成立」与「刻意取舍」；核对时发现本文档这条的因果只对了一半 —— 见 R5 条。
> **P3 / P4 已于 2026-09-13 实测结案（均不做）**：按本文档「等实测再做」的要求，用固化的测量夹具跑出真实数字后，P3 的「重复遍历」诊断被证伪（遍历 17ms vs 收集 2497ms），P4 的并行化收益被缓存快路径抵消 —— 理由与数字见 P3/P4 两条；实测顺带发现真正的瓶颈 **P5（逐文件 `canonicalize`）**。
> **P5 已于 2026-09-13 实施**：收集耗时 2497ms → 1598ms（**1.56×**，明显低于当时估计的 4× —— 估错的经过记在 P5 条里）；剩余耗时转移到 `is_save_candidate`。
> 仍未动的部分：**A1 的三项**（profile 形状复用、目录推断前置自动展示、保存后自动分析）与 **`is_save_candidate` 的开销** —— 见第 5 节的批次表。

---

## 1. 精准度

### R1. `%PROGRAMDATA%` 完全不在扫描范围内（高，静默丢存档）

- 证据：`discover_scan_roots`（`save_learning_service.rs:643-737`）的覆盖集合只有 APPDATA、LOCALAPPDATA、`USERPROFILE\{Documents, Saved Games, AppData\LocalLow}`、`PUBLIC\Documents`、各盘 Steam `userdata`。ETW 回推那条路 `infer_scan_root_for_etw_file`（`:833-932`）的 `bases` 数组（`:888-896`）同样没有 ProgramData，函数末尾直接 `None`（`:931`）。
- 后果链：`infer_scope_drafts` 对**不落在任何已知 root 下**的路径直接 `continue`（`:1344-1355`）。所以即使 ETW 已经证明游戏写了 `C:\ProgramData\<厂商>\<游戏>\save.dat`，这份证据也会被丢掉，用户看到的是「没有发现符合存档特征的变化」。
- 为什么值钱：ProgramData 是「全用户安装 / 老游戏 / 部分日系游戏」的常规落点，且不受 UAC 文件虚拟化影响。这一类游戏目前**无论用哪条采集路径都识别不出来**。
- 修法：`discover_scan_roots` 增加 ProgramData 根（`SaveRootType` 可能需新增变体，或复用 `Custom`）；`infer_scan_root_for_etw_file` 的 `bases` 加一项，回推规则用「`components[0]`（厂商）+ `components[1]`（游戏）」两级。两边都要改，只改一处会出现「快照能扫、ETW 扫不到」的分裂。
- **已解决（2026-09-11）**：新增 `SaveRootType::ProgramData`（不复用 `Custom`：它是环境变量可重定位的标准根，云端能靠 `root_type + sub_path` 在另一台机器上重建路径，而 `Custom` 只能携带源机器绝对路径）。两侧同时接通：`discover_scan_roots` 的环境变量根表新增 `PROGRAMDATA`，`infer_scan_root_for_etw_file` 的 `bases` 新增一项并加了「至少三段才取厂商 + 游戏两级」的守卫（否则第二段是文件名，范围根会变成文件路径）。变体本身是穷尽 `match`，编译器强制把 `cloud_account_service` / `cloud_account_commands` 的路径重建和前端标签一起补上。

### R2. 新存档文件不会自动进入已有规则（高，结构性问题）

- 证据：`include_directories` 只在 `protects_container` 为真时才被设为 `["."]`（`:1479-1482`，`protects_container = is_save_container_directory(scope_root)`，`:1411`），否则为空。而收集侧 `collect_profile_files`（`save_repository.rs:1261-1296`）只有两个来源：`confirmed_files` 的精确相对路径（`:1276-1281`）与 `include_directories` 的目录遍历（`:1282-1293`）。
- 后果：存档目录名不含 `save/savegames/userdata/remote/...` 时（最常见的形态是 `%APPDATA%\<游戏名>`），规则只有学习那一刻看到的文件清单。之后用户**新建档位**、游戏写**带时间戳的自动存档**（`autosave_20260911.sav`）→ 文件不在清单里 → **不进版本库，也不受保护**，且没有任何提示。
- 反向的坑同样存在：容器目录一旦命名命中，直接 `include_directories: ["."]` 整目录收集。而 `resolve_scope_root_and_relative` 取的是**最外层**命中的容器祖先（`:1223-1246`，`chosen_root` 在向上爬的过程中被反复覆盖）。以模拟器为例：`Documents\PPSSPP\PSP\SAVEDATA\<ROM_ID>\x.bin` 的最外层容器是 `SAVEDATA` 本身 → scope root = 整个 `SAVEDATA` 目录 + `include_directories: ["."]` → **会把该模拟器下所有 ROM 的存档都纳入版本库**。
- 结论：不能简单地把 `include_directories` 一律放开成 `["."]`。正确方向是「目录 + 候选启发式过滤」——即收集时对该目录做一次 `is_save_candidate` 式的筛选，而不是「目录 + 全收」或「只认历史清单」。这也正是 `unknown_file_policy`（见 R4）本该承担的语义。
- **已解决一半（2026-09-12，按原计划只做「提议」）**：草稿新增 `SaveScopeDraft::proposed_files`。对根目录**不是**命名存档容器的范围，扫一遍范围目录（深度 3、`is_save_candidate` 过滤、卡 `DEFAULT_MAX_FILE_BYTES`、剔除本次已确认的文件、最多 200 项），把「目录里已经存在、看起来也是存档、但**本次学习没有发生变化**」的文件列出来交给用户确认。前端用虚线样式单独成块（与已确认文件区分），一键「全部纳入保护」后并入 `confirmed_files`；提示语进 `notes`。
  容器命名的范围不列提议——它本来就 `include_directories: ["."]` 整目录收，再列一遍是噪音。
  **刻意不直接写进规则**：`is_save_candidate` 是启发式，而反向那个坑（容器名命中就整目录收，模拟器共享的 `SAVEDATA` 会收进别人的存档）已经证明「自动扩大范围」的代价比漏收更高。提议只改变「用户能不能看见」，不改变「未经确认就收」。
- **剩下的缺口（R2b，未做）**：本轮解决的是「学习时**已经存在**却没人看到」的档位。学习**之后**才出现的文件——新建档位、游戏写出的带时间戳自动存档——仍然进不来。补这一半要在收集侧把 `include_directories` 从「目录 + 全收」改成「目录 + 候选过滤」，前提是把 `is_save_candidate` 从学习服务下沉到 domain（`repositories/` 不能反向依赖 `services/`），并且会改变容器目录现有的收集结果（有些以前被收进来的配置类文件会不再收）——那是行为变更，不能和「提议」混在一笔里做。

### R3. 事务分析的输入口径与候选口径不一致（中高，分数可以被无关写入赚到）

- 证据（两条解析路径**不一样**，这是关键）：
  - 原生 ETL（主路径）：`operations.push` 在「是否为变更操作」判断**之前**（`native_etw.rs:152-160` vs `:161-168`），所以 `operations` ⊇ 候选文件集合，额外含 `Delete` 与非 write-like 的 `Close`；噪音路径过滤只作用于 `operation_path`（`:141-143`），**没有**做过 `is_etw_candidate`。
  - CSV 回退：`operations.push`（`etw_capture.rs:469-482`）在 `is_mutation` 判断（`:484-495`）之前，而且 `normalized_path` **连 `should_ignore_event_path` 都没过**（该过滤只作用于 `file_object_paths`（`:446`）与 `files`（`:501`））。也就是说回退路径下，`\logs\`、`\cache\`、`\temp\` 的写入会进事务评分。
- 后果：`analyze_save_transactions` 对全量操作排序分组（`transactions.rs:34-51`），`score_group` 里 write +30、close +30、多操作 +10、单 PID +10（`:233-247`），`confidence` 取**各组最大值**（`:76`），`completed_count` 只要「存在任一组 ≥80」（`:96-100`）。游戏写一张截图 `.png` 或一份 `settings.ini` 并正常关闭，就足以产出 `status = "completed"`；随后 `calculate_learning_confidence` 按 `("etw", Some(txn)) if completed` 给 **+10~15 分**（`save_learning_service.rs:1598-1606`）。
- 附带：`affected_files` 把整棵进程树碰过的所有文件都塞进汇总（`transactions.rs:215`），而这个字段前端是要展示的 —— 它展示的不是「存档候选」。
- 判定：与之前「置信度是评分不是概率」同源，但更实 —— 不只是不好读，而是**分数会被无关活动抬高**。修法是让 `operations` 过一次与 `files` 相同的口径（至少噪音路径 + `is_etw_candidate`），并在 `score_group` 里对「非候选路径」不计分。
- **注意**：`analyze_save_transactions` 的重试/回归面在 `transactions.rs` 尾部有 7 条测试，改口径要连同它们的输入一起收紧。
- **已解决（2026-09-11）**：新增 `transaction_evidence(&[FileOperation]) -> Vec<FileOperation>`，在 `save_learning_service` 里收口一次，事务摘要与 `classify_scope_evidence` 共用同一份结果。口径 = `should_ignore_event_path`（挡掉 `\logs\`、`\cache\`、`\temp\` 等）+ `is_etw_candidate`，**唯一例外是 `.tmp`/`.temp`/`.bak`**：它们不是候选存档文件（在草稿里落到 `noise_exact` → `exclude_exact`），但「写临时文件再重命名」正是最常见的保存动作，连同它们一起丢掉会让真实的原子保存在事务摘要里退化成「证据不足」，反而丢掉应得的置信度。噪音目录优先于这条例外，`\logs\x.tmp` 仍被挡下。

  **偏离原修法的一处**：原文说「并在 `score_group` 里对非候选路径不计分」，实现时改成只在入口收口一次。原因是候选判定 `is_etw_candidate` 住在上层的学习服务里，而 `transactions.rs` 在采集层——把上层启发式下沉进去是分层倒挂；若两处各判一次，就变成两条会漂移的规则（见技能 `duplicated-guard-single-choke-point`）。代价是这条不变式只由「唯一调用点 + 前置条件文档」保证，没有第二道防线；`analyze_save_transactions` 的文档注释已写明这条前置条件。**这个接缝没有单测覆盖**（`analyze` 需要 Tauri 句柄与真实文件系统），能测的是「收口 + 评分」这个组合。

  顺带修掉两个同源问题：`affected_files`（前端会展示）不再列整棵进程树碰过的文件；`has_missing_timestamp` 不再被噪音事件触发，因此真实保存不会再因为一条无关的缺时间戳日志被扣 20 分并降级成 `candidate`。

  **未改 `transactions.rs` 的评分逻辑**，尾部 7 条测试一字未动、全部通过（它们直接喂原始操作，测的是评分器本身）。新增 4 条测试覆盖收口，其中一条明确断言「未收口时同一批日志写入会拿到 `completed`」，把这道收口钉成承重件。

### R4. `unknown_file_policy` 是死字段：只写不读（中，语义谎报）

- 证据：全项目 grep `UnknownFilePolicy` 的 20+ 处出现，全部是构造、透传、断言（`domain/save_profile.rs:19-46`、`cloud_account_commands.rs:345`、`cloud_account_service.rs:280`、各测试）。**没有任何一处分支读取它**。学习侧还认真地算了这个值：`protects_container || SavedGames → Protect，否则 Ignore`（`:1457-1462`）。
- 后果：枚举名字宣称「未知文件是否受保护」，但实际保护范围完全由 `include_directories` 决定，改这个字段不改变任何行为。与 P2-10（`LearningStatus::Finished/Cancelled` 零赋值）同类：读代码的人会以为它管事。
- 修法：做 R2 时把它接上（正好是它的自然语义载体）；不做 R2 就删掉，别留个假开关。
- **已解决（2026-09-12）**：接在一个真实分歧上 ——「目录级收集时，目录里**没有**显式列进 `confirmed_files` 的文件（未知文件）算不算这个范围的一员」，由 `scope_admits_directory_file` 单点定义，**收集侧（`collect_profile_files` 的目录遍历）与恢复侧（`collect_protected_paths`）共用**。两侧必须共用：只在一侧生效就会出现「恢复时把一个从没被备份过的文件当成多出来的受保护文件删掉」（恢复是精确回到该版本）。`confirmed_files` 那条路刻意不过这道门（显式确认过的永远算数），用 `CandidateSource` 区分来源，不靠重新判一遍策略。
  **为什么是零回归**：现有所有带 `include_directories` 的范围都是容器目录（`["."]`），而容器目录的 policy 恒为 `Protect` —— 默认分支与新增分支结论一致。字段到现在才真的能改变行为，前端徽章「自动保护新存档 / 仅保护已确认文件」不再是一块假仪表。
  代价：对**没有**目录收集的范围（非容器范围，`include_directories` 为空），policy 仍无从生效 —— 没有目录可遍历，也就没有「未知文件」。这正是 R2b 那一半要填的洞。

### R5. ManagedGame 资产目录整棵剪枝 + 资源扩展名一刀切（低，理论缺口）

- 证据：`collect_snapshot` 的 `filter_entry`（`:1178-1186`）对 `is_managed_game_asset_dir` 直接跳过**整个子树**，名单含 `content`/`assets`/`sound`/`movies`/`textures` 等（`:97-123`）。另一处 `is_save_candidate` 把 `png/jpg/jpeg/webp/ogg/wav/mp3/ttf` 一律排除（`:1670`）。
- 后果：ETW 可用时无碍（走 `is_etw_candidate`），但 ETW 需要管理员权限（`etw_capture.rs:71`）。**非管理员用户是纯快照**，此时「存档住在 `content/` 下」或「存档是图片形式」就永久看不见。
- 说明：我在现有真实数据里**没有观察到**这种布局，属理论缺口，所以排低。改法也很便宜：把资产目录剪枝从「整棵剪掉」改成「剪枝但仍接受 `.sav` 等强扩展名」，代价是 ManagedGame 快照会慢一点。
- **结案（2026-09-13）：按「暂不考虑非管理员情况」的前提，不做。** 核对代码时发现本文档这条的因果**只对了一半**，两个半边要分开说：
  - **前半（资产目录剪枝）——文档说对了**：ETW 路径根本不走 `collect_snapshot`。`:473-492` 在 `etw_files` 非空时改走 `collect_targeted_snapshot`，那个函数只做「根内 + 非噪音 + 是文件」三件事，**没有** `is_managed_game_asset_dir` 剪枝、也没有 ManagedGame 的 `max_depth(4)`。所以「存档住在 `content/` 下」在 ETW 可用时是**看得见**的。这里有个容易被忽略的机关：`infer_scope_drafts` 里 `changed` 是拿 `etw_files` 去 `final_snapshot.contains_key()` 过滤的 —— 如果 `collect_targeted_snapshot` 也剪枝，ETW 证据会被**静默丢掉**。它没剪，所以没问题。
  - **后半（资源扩展名）——文档说错了**：`is_etw_candidate`（`:1888`）**同样**拒掉 `RESOURCE_EXTENSIONS`，而且是在看名字线索**之前**就一票否决。所以「存档就是一张 `.png` / 一段 `.wav`」在 ETW 下**照样看不见**，这不是「ETW 能绕过」的限制。但这一半应当**保留**：那 12 个扩展名是 `dll/exe/pak/pdb/png/jpg/jpeg/webp/ogg/wav/mp3/ttf`，没有一个像存档；放开就会把贴图、音频、字体、dll 当成存档收进来 —— 误收比漏收更难收拾。
  - **为什么「不做」而不是「放宽剪枝」**：文档建议的改法（剪枝仍接受强扩展名）只对非管理员有效，却要让**每次 ManagedGame 完整快照都变慢**；而且在「统一两个快照函数」这类重构里，剪枝极易被顺手搬进 `collect_targeted_snapshot`，那会把 ETW 路径一起拖慢。按当前前提，收益为零、风险为正。
  - **留下的是守卫而不是代码改动**：新增 3 条测试把结论所依赖的性质钉住 —— `targeted_snapshot_keeps_files_under_asset_directories`（前半的依据）、`full_snapshot_prunes_files_under_asset_directories`（成因的对照，说明「什么条件下会漏」）、`resource_extensions_are_rejected_by_both_candidate_paths`（后半是**刻意**取舍，不是疏漏）。将来若前提变化（开始支持非管理员），这三条会告诉你该从哪里接手。

### R6. `trailing_path_components_match` 名不符实（低，方向是错误恢复）

- 证据：`save_repository.rs:1030-1041`，函数名说「尾段匹配」，实现只比**最后一段文件名**（`parts_a.last() == parts_b.last()`）。
- 后果：跨设备恢复时，`pick_scope_by_root_path`（`:1069-1076`）用它给 `loose_matches` 消歧。同 `root_type` 下两个 scope 末段同名（例如两个游戏都用 `remote`）时会误判为同一范围，可能挑错 scope → **把 A 的版本恢复到 B 的范围**。需要「多 scope + 末段同名 + 精确路径失配」三重条件同时成立，触发概率低，但方向是错误恢复而非拒绝。
- 修法：改成比较**完整尾段序列**（从末段往回逐段相等，长度取短者），与函数名一致；或明确改名为 `same_leaf_name` 并保留现有行为 —— 但要先确认调用方真想要哪种语义。
- **已解决（2026-09-12）**：先确认了调用方要的语义，结论是**这条的「修法」不能照做**。`normalize_path` 不剥用户目录前缀，所以「比较完整尾段序列」会让 `C:\Users\Alice\...\Game\saves` 与 `C:\Users\Bob\...\Game\saves` 失配 —— 而 `loose_matches` 存在的意义恰恰是跨设备（用户名必然不同），照做会**直接打断跨设备恢复**。
  改法：把布尔判定换成**共同尾段长度** `trailing_path_component_match_len`（从末尾往回数完全相同的段数），`pick_scope_by_root_path` 取**唯一最长者**，同分仍然返回 `None`。
  这个改动是**严格超集**，不会把原来选对的变成选错：旧实现能给出结果 ⟺ 恰好一个候选末段同名 ⟺ 恰好一个候选共同尾段 ≥ 1，而它必然同时是最长者 → 新实现选中的仍是同一个；旧实现放弃的场景里，新实现只在最长者唯一时才给结果。
  真正新增的能力是「多个候选末段同名、但其中一个明显更近」—— 旧实现只能放弃并报错，新实现能唯一选出正确的那一个（测试 `find_scope_for_entry_picks_the_closest_root_when_leaf_names_collide`）。「末段同名且同样近」依旧拒绝（`find_scope_for_entry_still_refuses_when_shared_suffixes_tie`），保住「绝不随便挑一个」。
  顺带一提，既有的 `find_scope_for_entry_disambiguates_multiple_scopes_by_subpath` 之前是靠「末段不同」通过的，现在才真的是按子路径消歧。

---

## 2. 自动化

### A1. 全流程 5 个人工卡点，且没有「复用既有 profile」的入口（高）

- 证据：`AddGameWizard.vue` 的链路 = 选目录（`chooseSource:139`）→ 选 exe（`chooseExecutable:153`）→ 开始学习（`beginLearning:246`）→ **用户必须进游戏手动保存一次** → 点分析（`analyze:265`）→ 审阅 scopes 后确认。Rust 侧没有任何自动确认路径。
- 可自动化空间（按收益排序）：
  1. **复用既有 profile 的形状**。`SaveProfile.executable_hash` 已经存了，但只用于校验，没用于复用。同厂商 / 同引擎（`steam_emu.ini`、`steam_appid.txt`、Engine 目录特征）的既有 profile，其 scope 形状（root_type + include_directories + exclude_*）几乎可以直接作为新游戏的初始草稿，用户只在不符时修改。这一步能把「必走一次完整学习」降级为「先给一份草稿、多数情况直接确认」。
  2. **目录推断前置**：现在 `discover_scan_roots` 只在学习会话开始时跑一次。可以在用户选定 exe 之后立刻跑一次「只读推断」，把候选目录先展示出来，让用户在启动游戏之前就能确认或修正 —— 避免跑完一整套学习才发现目录不对。
  3. **保存后自动分析**：现在是用户手动点「分析」。检测到已跟踪进程在写入候选目录后可以自动触发一次分析（给一个「N 秒无写入后自动分析」的窗口，复用 `transactions.rs` 已有的 2 秒静默切分思路）。
  4. **允许「跳过学习」**：既然容器名启发式 + 扩展名白名单已经能给出可用的初稿，应当允许用户直接进入手工编辑（现在向导是线性的，`phase !== "ready"` 就拦住 `beginLearning`，没有旁路）。
- **已实施（2026-09-13）**：只做了第 4 项，第 1/3 项未做，第 2 项算部分落地。做法与取舍：
  - **先重构、后功能**：`infer_scope_drafts` 的入参从 `&ActiveLearningSession` 收成 `&[ScanRoot]` —— 这个函数**从来只用得到 `active.roots`**（用 awk 逐行核过函数体确认）。不收口就得为「只读初稿」另写一份归组 / 排除逻辑，两边的启发式必然随时间漂移 —— 正是 R2 教训里「同一个判定有两份实现」那类坑。
  - **只读初稿的诚实口径**：初稿走「空 baseline + 空 `etw_files`」调用同一份 `infer_scope_drafts`，于是快照里的每个文件都过 `is_save_candidate`（**快照口径**），得到的正是「这些目录里哪些文件看起来像存档」。代价是**没有任何写入证据** —— 所以草稿一律降为 `Review`、`changed_files` 恒为空、`event_capture_mode` 标成 `preview`。这三条不是文案问题：界面完全靠它们决定渲染「初稿」还是「识别结果」，错一条用户就会把猜出来的范围当成已确认的。
  - **前端不留死路**：`ready` 阶段加「跳过识别，先看初稿」入口 → 直接进审阅界面（摘要改成「只读初稿 / 只读推断 · 无写入证据」，置信度面板换成「未经写入证据校验」，不再显示评分）→ 同时给「改回完整识别」出口。没有这个出口的话，用户一旦选了跳过就只能「放弃添加」重来一遍，反悔的代价比不跳过大得多。
  - **第 1 项（profile 形状复用）刻意未做**：它要把 `SaveProfile.executable_hash` 的语义从「校验」改成「复用」，而 R6 刚证明过跨设备恢复那条路径很脆；在没有任何「同厂商 profile 复用」实测收益的情况下动它，风险与收益不成比例。
  - **第 2 项只算部分落地**：只读推断的机器已经建好，也已经在**启动游戏之前**可达，但它挂在用户主动点的按钮上，而不是「选定 exe 后自动展示」。自动展示会让每次添加都先付一次多级目录遍历（A3 的发现逻辑）的代价，用户没要求时不该先付。
  - **第 3 项（保存后自动分析）未做**：它改的是「会话何时结束」的判定，而 A2 刚给会话加了 30 分钟看门狗 —— 两处都在管会话生命周期，拆开做更安全。

### A2. ETW 采集期无任何自我限制（中，有实物证据）

- 证据：`logman` 参数只有 `-o/-p/level 4/-ets`（`etw_capture.rs:95-106`），未设 `-b/-bs/-ct`（无缓冲/丢弃策略）；PID 过滤在**解析期**（`:421`），采集期是全系统；去重结构 `file_object_paths` / `written_file_objects` / `operations` / `files` 整场只增不减（`:379-381`、`:497-503`）。
- 实物：本机 `%APPDATA%\com.gamesaver.desktop\events\` 下存在单个 **422 MB** 的 `.etl` 与 **450 MB** 的 `.etl.csv`。会话期间没有任何峰值保护。
- 修法：会话加时长上限（超时自动进入分析，而不是无限等）、给 `logman` 加缓冲与丢弃参数、对 `operations` 设条目上限（超出按「已见文件」收敛）。
- **已实施（2026-09-12）**：落地时先用本机 `logman`（10.0.26100.1150）实测了参数，发现**本文档给的 `-b` 是错的**：
  - `-b` 不是缓冲大小，而是「在指定时间开始收集」的起始时刻，且与 `-ets` 互斥 —— 加进去会让**整个会话创建失败**。缓冲大小是 `-bs`。
  - `-rf`（运行指定时长）同样与 `-ets` 互斥，报「参数"rf"不允许具有其他指定的参数」。所以**时长上限没法用 `logman` 原生做**。
  - 实测可用的组合：`-bs 64 -nb 16 256 -max 256`（`-nb` 限住内存缓冲池，`-max` 限住磁盘上的 `.etl`）。`-ct` 虽然也能用，但会改变 CSV 里 ClockTime 的量纲，而 `parse_trace_timestamp_ms` 是按数值大小猜格式的 —— 有静默解析错时间戳、进而打散 2 秒窗口事务聚合的风险，故**不采用**。
  实际改了三处：①`etw_trace_args`（参数抽成纯函数并加 `-bs/-nb/-max`，两条测试钉住「上界必须在」与「`-b`/`-rf`/`-ct` 必须不在」）；②时长上限改在 Rust 侧做 —— `MAX_CAPTURE_DURATION`（30 分钟）+ 采集看门狗，到点停止会话，已采到的证据全部保留、用户仍可正常点分析；③`-max` 那条**无法在本机验证**（创建 ETW 会话需要管理员），所以看门狗才是「由我们掌控的那一半」。
  **未做**：`operations` 的条目上限。它改变的是事务打分口径（`analyze_save_transactions` 按 `operations` 聚合），而磁盘/内存的真正上界已由 `-max` + `-nb` 给出，此时再加一个「超出就丢」的截断只会引入静默少算 —— 与 R2b 要修的是同一类 bug，故留作待办。
  ⚠️ **已知缺口**：装配看门狗的那一行落在 `start_learning_session` 里（需要 `AppHandle` + 真实游戏进程），单元测试够不到；helper 自身的契约有两组测试守着，但「这一行有没有被删掉」没有断言。变异测试里标为 GAP 而非 HIT。

### A3. 目录发现强依赖「游戏名出现在目录名里」（中）

- 证据：`find_candidate_directories`（`:771-798`）用 `game_name_hints(game)`（`:934-1005`：display_name 分词 + exe stem + `steam_appid.txt`/`steam_emu.ini` 里的 appid）逐层匹配目录名，命中才成为扫描根。
- 后果：目录名不含游戏名时（日系/老游戏常见的 `%APPDATA%\<厂商>\...`、或与游戏名毫无关系的目录名）非管理员用户永远找不到根。ETW 侧能靠 `infer_scan_root_for_etw_file` 兜住，但那需要管理员权限 —— 所以这是「**非管理员 + 目录名不含游戏名**」的组合缺口，与 R1、R5 同属一条「无管理员权限时的能力下降」主线。
- **已实施（2026-09-13）**：**只做「说清病因 + 指明明路」，不做自动猜测。** 取舍与发现：
  - **为什么不猜**：自动兜底只剩「目录名像存档容器」这一条路（`SAVE_DIRECTORY_HINTS` 那 9 个名字），而 `%APPDATA%` 下同时存在好几个这样的目录 —— 那些是**别人的存档**。猜错就是把它收进版本库，而误收比漏收更难收拾（同 R2 的一贯口径）。所以「猜」这一步留给用户，程序只负责让他知道发生了什么、该往哪走。
  - **症状原本是静默的**：发现失败时 `discover_scan_roots` 仍会返回游戏安装目录这一个根，学习照常跑完，用户看到的只是「没有发现变化」—— 完全不知道问题出在发现环节，还容易误以为「改用完整识别就能解决」。
  - **落地**：新增 `discovered_only_the_install_dir(roots)`，**只认根类型、不认路径**（安装目录本身也可能叫 `saves`，按路径名判会漏报）。在 `infer_scope_drafts` 的「没有候选」分支里，发现失败时改说「目录发现只找到了游戏安装目录：存档目录的名字里没有出现游戏名时，普通权限发现不了它（**改用完整识别也是同一份发现逻辑，同样找不到**）」，并指向手动添加 / 管理员重试。**学习路径与只读初稿路径共用这一处**，两边口径不会漂移。
  - **顺手修掉两处错误引导**：①初稿的「没有推断出候选范围」说明原先建议「改用完整识别」，对 A3 场景是**无效建议**，已去掉；②`preview_scope_drafts` 里的 `if roots.is_empty()` 是**死分支** —— `discover_scan_roots` 的第一个根恒为游戏安装目录、`retain` 只去重，结构上不可能返回空，所以它那句「没有推断出可扫描的存档目录，请改用完整识别」永远不会出现，已移除。
  - **未做**：容器名兜底自动纳入。理由如上；若将来要做，正确形态是「**只有唯一候选时才纳入**」并给 `ScanRoot` 加来源字段，以便把这类范围标成待确认，而不是无条件纳入。

---

## 3. 性能

### P1. `commit` 的「未修改判定」是 O(n·m)，最坏情况逐文件一次磁盘 stat（中）

- 证据：`save_repository.rs:52-56` 的 `files.iter().all(|f| old.iter().any(...))`；`:65-71` 每个文件再 `latest.files.iter().find(...)` 扫一遍旧条目。`collected_is_unmodified`（`:1573-1594`）在 size + mtime 命中时会调 `object_exists` → `fs::metadata`。
- 量级：2000 文件的存档目录（模拟器 memory card、带大量分槽元数据的游戏都能到这个量级）→ 两次各约 O(n·m) 的比较，其中一次带磁盘调用。
- 修法：一次性建 `HashMap<(root_type, normalize_relative(rel), normalize_path(root)), &SaveFileEntry>` 索引，把两次线性查找都降成 O(1)；顺带把「整体快速路径」与「逐文件复用哈希」统一成同一次查表。
- **已解决（2026-09-11）**：索引键用 `(root_type, normalize_relative(relative_path))`，值是**条目切片**而不是单条 —— `collected_matches_entry` 还要比 `root_path`，同名的不同范围（两个范围各有一个 `slot1.sav`）必须都留着再跑完整判定。三处线性扫描全部改走索引，包括「这一版里的文件现在还在不在」的反向查询。判定函数一行未动，所以语义完全不变。

### P2. `wildcard_matches` 每次调用重新分配（中）

- 证据：`save_repository.rs:1487-1489`，每次调用都把 `value` 与 `pattern` 各做一次 `to_ascii_lowercase().chars().collect::<Vec<_>>()`。调用链：`is_excluded`（`:1376-1379`，只对 `file_name` 调）← `add_candidate`（每文件一次，`:1316`）与 `scope_matches_entry_exact/loose`（每条目 × 每 scope）。
- 量级：400 个文件 × 8 条默认模式 ≈ 3,200 次堆分配，纯属浪费。
- 修法：模式在 scope 装载时预小写一次（或预编译成 token 序列）；至少让 `is_excluded` 接收已小写的模式。
- **已解决（2026-09-11）**：改成**逐字节**匹配，零分配。`to_ascii_lowercase` 只作用于 ASCII、非 ASCII 字节原样保留，而 UTF-8 是单射，所以「折叠后逐字节相等」与「折叠后逐字符相等」完全等价 —— 于是 `to_ascii_lowercase().chars().collect::<Vec<_>>()` 的四次堆分配可以整个去掉，判定结果一字不变。`?` 与 `*` 的「一个字符」按 UTF-8 首字节推进（ASCII 走一字节，多字节越过续字节），多字节字符不会被从中间劈开；`*` 的回退点同样按字符前进。原有 12 条 `wildcard_matches` 断言全部保持通过，另加一条多字节用例守这个边界。

### P3. 重复目录遍历（低-中）

- `collect_profile_files` 对每个 scope 各走一遍 `WalkDir`，同根多 scope 不去重（`save_repository.rs:1262-1294`）；
- 恢复侧 `collect_protected_paths` 把 `include_directories` 又 walk 一次（`:976`）；
- 学习侧一次会话最多三遍：基线快照（`save_learning_service.rs:221`）、容器兜底 `discover_save_container_files`（`:407`）、结束时的快照（`:459` / `:469`）。
- **实测结案（2026-09-13）：原诊断不成立，不做。** 夹具 9660 个文件，实测「多走一遍」的全部代价上限就是一次裸 `WalkDir` = **17.2ms**；而同一棵树下 `collect_profile_files` 要 **2497ms**（差 145 倍）。也就是说，遍历次数根本不是成本所在 —— 即便把三遍并成一遍也只省 ~17ms，相对学习一次 ~2.9s 的总开销是噪声。真正的开销是 `add_candidate` 在**任何过滤之前**对每个文件调的那次 `canonicalize`（见 P5），跟「走几遍」无关。所以这条按「诊断错了」结案，不是「不值得优化」。（数字与夹具见第 5 节 P3/P4 实测记录。）

### P4. 哈希阶段串行（低）

- `commit` 逐文件 `read_stable_file` + `sha256_bytes` + `write_object_locked`（`save_repository.rs:73-80`），全项目无 `rayon`/`par_iter`。读 + 哈希天然可并行（写对象需串行或加锁），对大批小文件是长尾。
- **实测结案（2026-09-13）：收益不抵成本，不做。** 实测 read + sha256 = **4874 文件/秒**（9660 个 1 字节文件 / 1982ms；1 字节文件下这几乎全是 open/read/sha256 初始化的系统调用开销，与数据量无关）。但 `commit` 有**缓存快路径**：与上一版逐字节一致的文件直接复用旧 `object_hash`、**不读不哈希**（`:88-105`），整份清单无变化时更在 `:69` 直接 `Ok(None)` 返回 —— 于是串行哈希循环**只覆盖新增/改动的文件**。常见路径（一次存档改几个文件）哈希 < 10ms，并行化收益测不出来；最坏路径（某游戏首次入库、近万文件全要哈希）约 2s，引入 rayon 可压到 ~0.5-0.7s，但那是**一次性**开销，且要为一个一次性收益新增并发依赖、还要处理 `write_object_locked` 的加锁语义。留一条触发条件：若将来有用户报「首次入库某游戏卡几秒」，再回来接这条（夹具已固化，可原地复测）。

### P5. 收集侧逐文件 `canonicalize()`（中，P3 实测新发现）

- 证据：`add_candidate`（改动前）在**任何过滤之前**对每个走进来的文件调 `path.canonicalize()`；而它的调用点 `collect_profile_files` 是**全目录、无深度上限**遍历。于是「目录里有多少文件」=「多少次 `canonicalize`」，与「其中几个是存档」无关 —— 被 `is_excluded` / `is_save_candidate` 拒掉的文件**照样**付了这次开句柄的代价。
- 量级（实测）：9660 文件 → 2497ms，其中约 2.4s 是 9660 次 `canonicalize`（Windows 上是 `GetFinalPathNameByHandle`，约 258µs/次）；同规模的 `collect_snapshot`（只 stat、不 canonicalize）只要 **415ms**。
- **已实施（2026-09-13）**：`add_candidate` 改成「先算相对路径 → 跑便宜的门 → **最后**才 canonicalize」。目录级来源（`CandidateSource::Directory`）用**词法**相对路径过门，只有幸存者才 canonicalize；显式确认的来源（`Confirmed`）保持原样先 canonicalize —— 那里可能指向符号链接，语义不能动。
  - **等价性依据**：`root` 在 `collect_profile_files` 开头已 canonical，遍历用 `follow_links(false)`，所以 `root.join(..)` 出来的路径没有符号链接或 8.3 短名需要解析；词法与 canonical 相对路径经 `normalize_relative` 后是同一个字符串 ⇒ **门看到的东西不变**；幸存者照旧 canonicalize ⇒ **存储路径逐字节不变**。
  - 相对路径的计算抽成纯词法的 `relative_path_of(path, root)`，两处复用，避免又出现「同一判定两份实现」。
- ⚠️ **实测修正：我估的「约 4×」是错的，实测只有 1.56×**（2497ms → 1598ms）。按差额推算，剩余耗时已转移到 `is_save_candidate`（约 90µs/文件）—— 它现在对 9660 个文件全跑一遍，之前只是被 4 倍贵的 `canonicalize` 盖住了。**它才是下一处真瓶颈**（判定里按祖先做 `String` 分配）。
  **教训同 P3，同一条第二次应验**：别按结构推算倍数，测了才知道 —— 第一次是 P3 的诊断错了，这次是 P5 的倍数估错了。
- 状态：**已实施**；`is_save_candidate` 的开销列为后续候选（**未实施**）。

---

## 4. 不同游戏类型的覆盖矩阵

| 类型 | 典型存档位置 | 现状 | 缺口 |
| --- | --- | --- | --- |
| Steam 游戏 | `Steam\userdata\<账号>\<appid>\remote` | ✅ 目录发现 + `is_save_container_directory` 的纯数字段判定（`:1148-1155`） | — |
| Unity | `%USERPROFILE%\AppData\LocalLow\<公司>\<游戏>` | ✅ LocalLow 在 roots；ETW 回推也覆盖 | PlayerPrefs 存注册表，文件层不可见（声明为不支持） |
| UE / 主流商业游戏 | `%LOCALAPPDATA%\<游戏>\Saved\SaveGames` | ✅ `<游戏>` 命中游戏名即成为 root，其下整树在快照内 | 「Saved」不在容器名单，靠 `SaveGames` 命中；正常 |
| 老游戏 / 日系 / 免安装 | `%APPDATA%\<厂商>\<游戏>`、或安装目录内 | ⚠️ 目录名不含游戏名时非管理员下发现不了（A3）；安装目录靠 ManagedGame root | A3 |
| **全用户安装 / ProgramData** | `%PROGRAMDATA%\<厂商>\<游戏>` | ✅ **已补（2026-09-11）**：快照侧按名字线索扫，ETW 回推取厂商 + 游戏两级 | — |
| **模拟器（经典主机/掌机）** | RetroArch `saves\<core>\*.sav`、PPSSPP `...\SAVEDATA\<ROM_ID>`、PCSX2 `memcards\` | ❌ 不在扫描根；且 ETW 回推得到的 scope root 是**共享的 SAVEDATA 目录** | **整个类别缺支持**：与「一游戏一目录」模型冲突（R2 的反向坑） |
| DOSBox / 老 PC 游戏 | 安装目录内 `.sav`/`.dat` | ⚠️ `.dat` 不在 `SAVE_EXTENSIONS`（`:31-33`），靠 hint 或容器名兜 | 扩展名白名单偏窄 |
| 注册表存档 | 注册表 | ❌ 文件层无解 | 明确不做 |

补充：模拟器这一类值得单独想一次。它们有两个与现有模型根本冲突的特征 —— ①存档不属于「这个游戏」，而属于「这个模拟器 + 这个 ROM」；②多个 ROM 共用一个 `SAVEDATA`/`saves` 目录。当前启发式会把整个共享目录当成一个 scope。要么为模拟器做「按 ROM 文件名/ROM ID 过滤」的特例，要么在 UI 上明确让用户手工把范围收窄到 `SAVEDATA\<ROM_ID>`。

---

## 5. 建议的实施顺序

| 批次 | 内容 | 性质 | 预估影响面 | 状态 |
| --- | --- | --- | --- | --- |
| 第 1 批 | **R1** ProgramData 补根（快照侧 + ETW 回推侧同时改） | 纯增量，无行为变更 | 小 | **已实施 2026-09-11** |
| 第 1 批 | **P2 + P1** 通配预小写、`commit` 建索引 | 纯性能，无行为变更 | 小 | **已实施 2026-09-11** |
| 第 2 批 | **R3** 事务输入口径收口（含 `score_group` 只对候选路径计分） | 会改分数，需同步 7 条既有测试 | 中 | **已实施 2026-09-11**（未动 `score_group`，改在入口收口，见 R3 条） |
| 第 3 批 | **R2 + R4**：草稿列出「疑似存档」提议交用户确认；`unknown_file_policy` 接上语义 | 结构性，收益最大 | 大 | **已实施 2026-09-12**（R2 只做「提议」一半，见 R2 条） |
| 第 3.5 批 | **R2b** 收集侧「目录 + 候选过滤」，让学习**之后**新出现的存档自动纳入 | 行为变更：会改容器范围现有的收集结果，且需先把 `is_save_candidate` 下沉到 domain | 大 | **已实施 2026-09-12**（分两步：①候选判定簇下沉 domain；②非容器范围开目录级收集。容器范围收紧**未做**，见 R2b 条） |
| 第 4 批 | **A2** ETW 会话上限 / 缓冲参数 | 防护性 | 小-中 | **已实施 2026-09-12**（参数经本机实测修正，见 A2 条） |
| 第 5 批 | **A1** profile 形状复用、目录推断前置、保存后自动分析 | 自动化，涉及前端 | 大 | **部分实施 2026-09-13**（只做「跳过学习」旁路；profile 复用 / 自动分析**未做**，见 A1 条） |
| 备选 | **A3 / R5 / R6** 边缘缺口 | — | 小 | **R6 已实施 2026-09-12**；**A3 已实施 2026-09-13**（只引导、不猜）；**R5 已结案 2026-09-13**（按「暂不考虑非管理员」不做，理由见 R5 条） |
| 备选 | **P3 / P4** 重复目录遍历、哈希阶段串行 | 纯性能，等实测 | 小 | **均已结案 2026-09-13**：P3「诊断不成立」、P4「收益不抵成本」，均**不做**；实测另发现真瓶颈 **P5 逐文件 `canonicalize`**，见第 5 节 P3/P4 实测记录 |
| 第 6 批 | **P5** 收集侧逐文件 `canonicalize` | 纯性能，行为等价 | 中 | **已实施 2026-09-13**：2497ms → 1598ms（**1.56×**），3 条守卫测试 + 变异 ×5；剩余耗时转移到 `is_save_candidate`（**未实施**），见第 5 节 P5 落地记录 |

第 1 批全部是「不加行为、只补覆盖与省开销」，可以一次做完；第 2 批要动评分，建议单独一笔并逐条变异验证；第 3 批是本轮真正的核心，但也是唯一可能**把范围收错**的改动，所以按这里写的做法执行了 —— **只做「疑似存档」的提议**（在草稿里列出、由用户确认），没有直接写进规则。真正会改变容器范围收集结果的 R2b 因此单列成第 3.5 批，与「提议」分开做。

**第 1 批落地记录（2026-09-11）**：`cargo test --lib` **242 passed**（238 → 242，+4）；`npm run build` 通过；`cargo fmt --check` 干净；clippy **31 条**（基线 32，少掉的那条是 `infer_scan_root_for_etw_file` 里一处既有的 `if_same_then_else`，未新增任何告警）。变异 ×5，各自精确命中：

| 变异 | 预期失败点 | 结果 |
| --- | --- | --- |
| 通配的 `?` 改回按字节前进 | `wildcard_patterns_step_over_multibyte_characters` 第一句 | ✅ 命中（`档.sav` vs `?.sav`）|
| ProgramData 两级判定 `>= 3` 放宽成 `>= 2` | 范围根变成文件路径 `…\classicgame\save.dat` | ✅ 命中 |
| `infer_scan_root_for_etw_file` 的 `bases` 去掉 ProgramData 项 | 回推返回 `None`，`.expect` 触发 | ✅ 命中 |
| `ENVIRONMENT_SCAN_ROOTS` 去掉 `PROGRAMDATA` | 环境变量根断言 | ✅ 命中 |
| `entries_at_location` 忽略传入的 `root_type` | 索引串桶，`root_type` 隔离断言返回 2≠1 | ✅ 命中 |

**第 2 批落地记录（2026-09-11）**：`cargo test --lib` **246 passed**（242 → 246，+4）；`npm run build` 通过；`cargo fmt --check` 干净；clippy **31 条**与第 1 批基线持平，`save_learning_service.rs` 里剩的 5 条（`:582`、`:1177`、`:1586`、`:2249`、`:2273`）全是本次之前就有的。变异 ×4，各自精确命中：

| 变异 | 预期失败点 | 结果 |
| --- | --- | --- |
| `is_transaction_evidence_path` 去掉 `should_ignore_event_path` 前置 | `\logs\scratch.tmp` 漏过临时文件例外 | ✅ 命中（保留集多出 1 条）|
| 候选门换成无条件 `true` | `screenshot.png` / `Game.exe` 被当成事务证据 | ✅ 命中（2 条断言）|
| 去掉 `.tmp`/`.temp`/`.bak` 例外 | 原子保存的中间产物被丢弃，保留集变空 | ✅ 命中（保留集 `[]`）|
| `transaction_evidence` 改成直通 | 组合测试从 `insufficient_evidence` 变回 `completed` | ✅ 命中 |

**第 3 批落地记录（2026-09-12）**：`cargo test --lib` **250 passed**（246 → 250，+4）；`npm run build`（含 `vue-tsc`）通过；`cargo fmt --check` 初检 4 处不规范，`cargo fmt` 后干净；clippy **31 条**（`--lib --all-targets`，与第 1/2 批基线持平）—— 落在本次触及文件上的告警逐条核对过，全是本次之前就存在的（`save_learning_service.rs` 的 5 条 + `save_repository.rs` / `domain/save_profile.rs` 各 1 条），只有行号因新增代码整体位移，**没有新增任何告警**。变异 ×5，各自精确命中：

| 变异 | 预期失败点 | 结果 |
| --- | --- | --- |
| 去掉 `propose_directory_saves` 里的 `is_save_candidate` 过滤 | 配置 / 截图等非存档混进提议 | ✅ 命中（`proposal_lists_only_unconfirmed_save_like_files`）|
| 去掉「剔除本次已确认文件」的去重 | 已确认的 `slot1.sav` 重复出现在提议里 | ✅ 命中（`proposal_lists_only_unconfirmed_save_like_files`、`only_non_container_drafts_carry_proposals`）|
| `scope_admits_directory_file` 恒放行 | `Ignore` 形同虚设，收集与恢复两侧同时失败 | ✅ 命中（`unknown_file_policy_decides_whether_directory_files_are_collected`、`protected_paths_share_the_unknown_file_policy_with_collection`）|
| `scope_admits_directory_file` 恒拦截 | `Protect` 反向失效：已收集的文件在恢复侧不再被认作受保护 | ✅ 命中（上述 2 条 + `collect_profile_files_skips_restore_artifacts`）|
| 容器命中的范围也列提议 | 与「容器整目录收、无需提议」的设计冲突 | ✅ 命中（`only_non_container_drafts_carry_proposals`）|

新增 4 条测试：`proposal_lists_only_unconfirmed_save_like_files`、`only_non_container_drafts_carry_proposals`（`services::save_learning_service::tests`）、`unknown_file_policy_decides_whether_directory_files_are_collected`、`protected_paths_share_the_unknown_file_policy_with_collection`（`repositories::save_repository::tests`）。

**第 3.5 批（R2b）落地记录（2026-09-12）**：分两步，各自独立验证。

- **第 1 步（纯重构，零行为变更）**：候选判定簇下沉到 `domain/save_candidate.rs`；`strip_verbatim_prefix` / `normalize_path` 收进 `domain/path_utils.rs`（顺手合并了服务层与仓储层两份逐字节相同的 `strip_verbatim_prefix`）。用 `.workbuddy/verify_pure_move.py` 逐项比对 **15/15 逐字节一致**，唯一差异是可见性由 private 提升为 `pub(crate)`。
- **第 2 步（功能主体）**：共用门 `scope_admits_directory_file` 改成三层 —— ①`confirmed_files` 里的永远算数；②`policy != Protect` 直接拒；③范围根是**命名容器** → 整目录收（行为保持），否则 → `is_save_candidate(绝对路径)`。收集侧（`add_candidate`）与恢复侧（`collect_protected_paths`）**共用同一道门** —— 只改一侧会出现「恢复时把从没备份过的文件当多出来的受保护文件删掉」。
- **一处自我修正（重要）**：只给非容器范围加 `include_directories: ["."]` **会静默失效** —— 非容器范围的 `unknown_file_policy` 默认是 `Ignore`，而门的第 2 层就是 `policy == Protect`。所以草稿构造改成对所有范围都给 `["."]` + `Protect`，容器与否改由门从 `root_path` 现推，policy 从此只当用户开关（前端徽章也从 `<span>` 变成真开关，提议区块只在 Ignore 档显示）。
- `cargo test --lib` **253 passed**（250 → 253，+3）；`npm run build`（含 `vue-tsc`）通过；`cargo fmt --check` 干净；clippy **31 条**与基线持平（告警位置只有行号位移）。变异 ×6 全部精确命中（`.workbuddy/mutate_r2b2.py`）。
- **刻意未做**：容器范围收紧（第 3 步）；目录遍历的深度/条数上界 —— 容器范围本来就无上界，加了不一致，且上界会造成**静默少收**（正是 R2b 要修的那类 bug），性能角度由 P3 覆盖。
- ⚠️ **踩坑**：仓储测试的 `temp_root` 原本用 `std::env::temp_dir()`，而它在 Windows 上位于 `\AppData\Local\Temp\` —— 正是 `is_noise_path` 明确挡掉的噪音目录。候选过滤把测试文件全判成噪音，3 个既有测试假失败。改用 `current_dir()`（与服务层测试一致），**测试期望值一个没改**。
- 补充：测试残留目录 `src-tauri/gamesaver-*` 的来源已查清 —— 每个测试的 `remove_dir_all` 都在最后一行，**断言失败就永远走不到清理**，所以「有残留」本身就是「这轮跑挂过测试」的信号；全绿的一轮为零。

**第 4 批（A2）+ R6 落地记录（2026-09-12）**：`cargo test --lib` **262 passed**（253 → 262，+9：A2 六条 + R6 三条）；`npm run build`（含 `vue-tsc`）通过；`cargo fmt --check` 干净；clippy **31 条**与基线持平。变异 ×9（`.workbuddy/mutate_a2_r6.py`）：

| 变异 | 预期失败点 | 结果 |
| --- | --- | --- |
| `etw_trace_args` 去掉 `-max`（磁盘上界） | `etw_trace_args_bound_the_capture_resources` | ✅ 命中 |
| `etw_trace_args` 去掉 `-nb`（内存上界） | 同上 | ✅ 命中 |
| 把文档建议的 `-rf` 加回来（与 `-ets` 互斥） | `etw_trace_args_avoid_flags_that_conflict_with_ets` | ✅ 命中 |
| 看门狗不等待就停会话 | `capture_watchdog_stops_the_session_when_the_deadline_passes` | ✅ 命中 |
| 会话启动时不挂看门狗 | `capture_watchdog_is_armed_for_an_active_session` | ⚠️ **GAP**（装配只有一行，落在需要 `AppHandle` + 真实游戏进程的 `start_learning_session` 里，单元测试够不到；helper 自身契约另有两条测试守着）|
| 没有采集句柄也照样挂表 | `capture_watchdog_is_not_armed_without_a_session` | ✅ 命中 |
| 尾段匹配退回「只比末段文件名」 | `find_scope_for_entry_picks_the_closest_root_when_leaf_names_collide` | ✅ 命中 |
| 去掉同分保护（同分也挑一个） | `find_scope_for_entry_still_refuses_when_shared_suffixes_tie` | ✅ 命中 |
| 共同尾段从前往后数（方向反了） | `find_scope_for_entry_picks_the_closest_root_when_leaf_names_collide` | ✅ 命中 |

R6 的结论见「1. 精准度」R6 条：**文档给的修法是错的**（比较完整尾段序列会打断跨设备恢复），实际改成共同尾段长度取唯一最长者，且证明为严格超集。

**第 5 批（A1「跳过学习」）落地记录（2026-09-13）**：只做 A1 的第 4 项，另外三项见 A1 条的取舍说明。`cargo test --lib` **266 passed**（262 → 266，+4）；`npm run build`（含 `vue-tsc`）通过；`cargo fmt --check` 干净；clippy **31 条**与基线持平 —— 逐条核对过落在本次新增代码上的告警，**零新增**。变异 ×9（`.workbuddy/mutate_a1.py`）：

| 变异 | 预期失败点 | 结果 |
| --- | --- | --- |
| 只读初稿不再统一降级证据（整段覆盖删掉） | `preview_drafts_are_review_only_and_skip_non_save_files` | ✅ 命中 |
| 只读初稿的说明不再写「只读初稿」 | 同上 | ✅ 命中 |
| 只去掉 `evidence_level` 赋值（保留说明改写） | 同上 | ⚠️ **REDUNDANT**（见下）|
| 空 baseline 技巧失效（`Some` 改 `None`） | `preview_drafts_are_review_only_and_skip_non_save_files` | ✅ 命中 |
| 初稿放宽候选过滤（不再过 `is_save_candidate`） | `preview_drafts_stay_empty_without_save_like_files` | ✅ 命中 |
| 初稿谎称「有文件发生变化」 | `preview_result_never_claims_observed_changes` | ✅ 命中 |
| 初稿不标记为 `preview` 模式 | 同上 | ✅ 命中 |
| 初稿凭空造一份事务摘要 | 同上 | ✅ 命中 |
| 空初稿不给下一步提示 | `preview_result_explains_an_empty_draft` | ✅ 命中 |

新增 4 条测试（均在 `services::save_learning_service::tests`）：`preview_drafts_are_review_only_and_skip_non_save_files`、`preview_drafts_stay_empty_without_save_like_files`、`preview_result_never_claims_observed_changes`、`preview_result_explains_an_empty_draft`。

- **一处自我修正（重要）**：初稿「证据诚实」的三条属性（`changed_files` 恒空 / 模式标 `preview` / 无事务摘要）原本写在 `preview_scope_drafts` 里，而那个函数要 `discover_scan_roots` + 真实目录快照，**单元测试够不到** —— 最要紧的诚实属性反而没有断言。为此把它抽成纯函数 `preview_result(drafts, notes, root_count)`，三条属性这才被测试钉住（变异 6/7/8 正是打在这上面）。
- **一处已知冗余**：`preview_drafts_from_roots` 里显式把 `evidence_level` 压成 `Review` 的那一行，**当前是冗余的** —— 只读初稿传的 `etw_files` 是空集，`classify_scope_evidence(..., snapshot_only = true)` 已经返回 `Review`。变异 3 因此不会被任何测试抓住。**刻意保留**：它是一条不依赖 `infer_scope_drafts` 内部行为的本地保证；将来若有人给初稿接上真实 baseline / ETW 文件，这行才是真正兜底的那道。在变异脚本里标成 REDUNDANT 而不是 HIT，避免下次把它误当成「有测试守着的守卫」。
- ⚠️ **踩坑**：`Edit` 工具在 `old_string` 以 `{\n` 结尾、`new_string` 以 `{` 结尾时，会把**下一行合并到同一行**（变成 `pub fn f() {    state: State<AppState>,`）。这种结果**照样能编译**（`) {    stmt` 是合法 Rust），只能靠回读文件发现。本轮又踩到一次，改完必须回读。

**A3 落地记录（2026-09-13）**：只做「说清病因 + 指明明路」，不做自动猜测（取舍见 A3 条）。`cargo test --lib` **269 passed**（266 → 269，+3）；`npm run build`（含 `vue-tsc`）通过；`cargo fmt --check` 初检 2 处不规范（`use` 列表换行、一处调用超宽），`cargo fmt` 后干净；clippy **27/31 条**与基线持平，落在 A3 新增代码上的告警**零条**。变异 ×7（`.workbuddy/mutate_a3.py`），**7 命中 0 问题**：

| 变异 | 预期失败点 | 结果 |
| --- | --- | --- |
| 「只找到安装目录」恒判为 `false` | `empty_drafts_explain_a_name_mismatch_instead_of_suggesting_a_retry` | ✅ 命中 |
| 「只找到安装目录」恒判为 `true` | `empty_drafts_stay_plain_when_discovery_found_more_than_the_install_dir` | ✅ 命中 |
| 判定方向反了（`!=` 改 `==`） | `install_only_discovery_is_decided_by_root_type` | ✅ 命中 |
| A3 说明退化成普通说明（去掉分支） | `empty_drafts_explain_a_name_mismatch_instead_of_suggesting_a_retry` | ✅ 命中 |
| A3 说明不再点破「改用完整识别也没用」 | 同上 | ✅ 命中 |
| 恒走 A3 分支（发现成功也说发现失败） | `empty_drafts_stay_plain_when_discovery_found_more_than_the_install_dir` | ✅ 命中 |
| 初稿空说明退回「建议改用完整识别」 | `preview_result_explains_an_empty_draft` | ✅ 命中 |

新增 3 条测试（均在 `services::save_learning_service::tests`）：`install_only_discovery_is_decided_by_root_type`、`empty_drafts_explain_a_name_mismatch_instead_of_suggesting_a_retry`、`empty_drafts_stay_plain_when_discovery_found_more_than_the_install_dir`。另给既有的 `preview_result_explains_an_empty_draft` 补了一条「不得出现『改用完整识别』」的断言 —— 否则那条错误建议的回归不会被任何测试挡住（变异 7 就是打在这条新断言上）。

- **一处未被测试覆盖的不变量（诚实记录）**：`discover_scan_roots` 「至少返回安装目录这一个根」是删掉死分支的依据，但它需要真实 `Game` + 完整环境扫描才能断言，**单元测试没覆盖**。这条不变量目前靠阅读确认，并写在 `preview_scope_drafts` 的注释里。
- **一处自证**：删除死分支后，如果将来 `discover_scan_roots` 真的返回了空，行为是「`drafts` 为空 → 走 A3 说明」—— 仍然安全，不会 panic、也不会给出错误建议。

**R5 结案记录（2026-09-13）**：结论是**不做**（理由见 R5 条），所以本轮的产出是「3 条守卫测试 + 一次文档纠错」，没有行为改动。`cargo test --lib` **272 passed**（269 → 272，+3）；`npm run build`（含 `vue-tsc`）通过；`cargo fmt --check` 初检 2 处 → `cargo fmt` 后干净；clippy **27/31 条**与基线持平，新增代码零告警。变异 ×4（`.workbuddy/mutate_r5.py`），**4 命中 0 问题**：

| 变异 | 预期失败点 | 结果 |
| --- | --- | --- |
| ETW 目标快照也剪掉资产目录里的文件 | `targeted_snapshot_keeps_files_under_asset_directories` | ✅ 命中 |
| 完整快照不再剪枝资产目录 | `full_snapshot_prunes_files_under_asset_directories` | ✅ 命中 |
| ETW 候选放过资源扩展名 | `resource_extensions_are_rejected_by_both_candidate_paths` | ✅ 命中 |
| 快照候选放过资源扩展名 | 同上 | ✅ 命中 |

新增 3 条测试（均在 `services::save_learning_service::tests`）：`targeted_snapshot_keeps_files_under_asset_directories`、`full_snapshot_prunes_files_under_asset_directories`、`resource_extensions_are_rejected_by_both_candidate_paths`。

- ⚠️ **一次「假 MISSED」——变异本身写错了，不是测试不够**：第 1 个变异最初写成在 `collect_targeted_snapshot` 里加 `is_managed_game_asset_dir(candidate)`，结果测试**照过**（MISSED）。查下来是变异失真：`is_managed_game_asset_dir` 看的是**条目自己的文件名**，对 `content/save.dat` 判的是 `"save.dat"`，永远不命中。`collect_snapshot` 之所以能剪掉整棵子树，是因为它用 `filter_entry` **拒绝下降进** `content` 这个目录 —— 而 `collect_targeted_snapshot` 是遍历显式文件列表、根本没有「下降」这回事。改成 `candidate.parent().is_some_and(is_managed_game_asset_dir)`（等价于「父目录是资产目录就跳过」）后立刻命中。
  **教训**：变异必须忠实模拟「真实会发生的那种破坏」。一个写歪的变异会伪装成「测试覆盖不足」，而照着它去补测试只会补出一堆没用的断言。判据是问一句：**这个变异真的改变了行为吗？**
- **一处刻意的测试不对称**：只钉住 `targeted_snapshot` 的「不剪枝」是不够的 —— 单看这一条容易让人以为 R5 压根不存在。所以同时钉住 `full_snapshot` 的「**确实**剪枝」，把「什么条件下会漏」写清楚，将来前提变化时才知道从哪里接手。

**P3 / P4 实测记录（2026-09-13）**：这两条的结论**完全依赖本机实测数字**（本文档原话是「等实测再做」），所以先把夹具固化成两条 `#[ignore]` 测试（不进常规门禁），再跑数。夹具：4 桶 × 40 组 × 60 文件 = 9600，外加一条 depth 6 的深链 60 个，共 **9660 个文件**。`cargo test --lib measure -- --ignored --nocapture`：

| 测量 | 对象 / 结果数 | 耗时 |
| --- | --- | --- |
| 裸 `WalkDir`（无深度上限、无剪枝） | 9660 个文件 | **17.2ms**（≈561k 文件/秒）|
| `collect_profile_files`（ManagedGame + `include_directories=["."]`） | 9660 → 收 **2400** | **2496.8ms**（961 文件/秒）|
| `collect_snapshot`（ManagedGame，`max_depth(4)` + 资产目录剪枝） | 9660 → 收 **7200** | **415.3ms**（≈17k 文件/秒）|
| `read_stable_file` + `sha256_bytes` | 9660 个 | **1982.0ms**（4874 文件/秒）|

三个由此得出的判断：

- **P3 是「诊断错了」，不是「不值得优化」**：`collect_profile_files` 比裸遍历慢 **145 倍**，但慢的不是遍历（17.2ms），而是 `add_candidate` 在**任何过滤之前**对每个文件调的那次 `canonicalize`（`save_repository.rs:1435-1439`）—— 9660 次 ≈ 2.4s，占全程 96%+。收 2400 个的原因也核过：只有 `save` 桶过 `is_save_candidate`（`save` 命中 `SAVE_DIRECTORY_HINTS` → `has_save_container`；`f000.dat` 既不中 `NAME_HINTS`、`.dat` 也不在 `SAVE_EXTENSIONS`），`bin`/`data`/`assets` 三桶被拒 —— 但**照样**付了 canonicalize。对照 `collect_snapshot` 只 stat、不 canonicalize，同规模只要 415ms。**所以 P3 建议的「合并遍历」省不到东西。**
- **P4 是「收益不抵成本」**：`commit` 的缓存快路径（`:88-105` 复用旧 `object_hash`、`:69` 整份清单无变化直接 `Ok(None)`）让串行哈希**只覆盖新增/改动文件**，常见路径已 <10ms；最坏路径（某游戏首次入库、近万文件全要哈希）约 2s，并行化可省 ~1.4s，但要新增并发依赖、且 `write_object_locked` 本就要串行。
- **P5 是实测顺带发现的真瓶颈**，单列**未实施**（取舍见 P5 条）。

- ⚠️ **一个「按结构推性能」的教训**：P3 当初是**照代码结构**推的（「同一个目录被走了好几遍，所以慢」），而这类结构性推断最容易错 —— 真实耗时往往不在「走了几遍」，而在**每个条目身上干了什么**。测量夹具的价值就在这里：它把「哪一段贵」从推断变成数字，本次直接把 P3 从「低-中收益」翻成「诊断不成立」。
- 夹具**刻意留在代码里**（`repositories::save_repository::tests::measure_p3_p4_traversal_and_hashing`、`services::save_learning_service::tests::measure_learning_snapshot_cost`，均 `#[ignore = "测量用，需手动触发"]`）：P3/P4/P5 的判断都依赖本机数字，换机器或换目录结构时可原地复测，不必重新推导。
- **本轮没有变异表**（与其它批次不同）：两条新增测试都是**测量夹具**，只计时、不断言，不含任何守卫判定分支 —— 没有可变的东西。真正的守卫（`canonicalize` 那条）属于 P5，等实施 P5 时再配变异。门禁：`cargo fmt --check` 干净、`npm run build` 通过、`cargo test --lib` **272 passed**（0 failed，2 ignored）、clippy **27/31** 与基线持平且落在新增代码上的告警**零条**。
