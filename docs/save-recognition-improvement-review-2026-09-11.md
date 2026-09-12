# 存档识别与规则生成：性能 / 精准度 / 自动化 审视

> 2026-09-11。范围：`services/save_learning_service.rs`（2696 行）、`services/learning/{etw_capture,native_etw,transactions}.rs`、`repositories/save_repository.rs`（2724 行）、`domain/save_profile.rs`。
> 目标按主人给的三个方向：**识别性能与精准度**、**规则的性能与精准度**、**提高自动化程度**，并要求覆盖不同经典游戏类型。
> 本文只做审视与建议，**未改任何代码**。

---

## 0. 结论先行

主干是对的，不需要重做：ETW 写入证据 + 快照差异双路、容器名启发式、明确的包含/排除规则、显式文件清单。问题出在**边界的口径**上，而且当前设计同时存在两个相反的失效方向：

> **容器目录被判定时，规则是「整目录收集」；没被判定时，规则是「只认当时看到的文件」。**
> 于是同一套模型下，一边可能把**别人的存档**收进版本库，一边又**收不到自己新增的存档**。

按收益排序，建议先做 4 项：**R1（ProgramData 缺失）→ R2（新文件不纳入）→ R3（事务口径）→ P1/P2（两处性能）**。R1、R3、P1、P2 都是小改；R2 是结构性改动，收益最大但需要设计守卫。

> **第 1 批（R1 + P1/P2）与第 2 批（R3）已于 2026-09-11 实施**，落地记录与变异结果见第 5 节。下面各条保留审视当时的原文，并在末尾补上「已解决」说明。

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

### R6. `trailing_path_components_match` 名不符实（低，方向是错误恢复）

- 证据：`save_repository.rs:1030-1041`，函数名说「尾段匹配」，实现只比**最后一段文件名**（`parts_a.last() == parts_b.last()`）。
- 后果：跨设备恢复时，`pick_scope_by_root_path`（`:1069-1076`）用它给 `loose_matches` 消歧。同 `root_type` 下两个 scope 末段同名（例如两个游戏都用 `remote`）时会误判为同一范围，可能挑错 scope → **把 A 的版本恢复到 B 的范围**。需要「多 scope + 末段同名 + 精确路径失配」三重条件同时成立，触发概率低，但方向是错误恢复而非拒绝。
- 修法：改成比较**完整尾段序列**（从末段往回逐段相等，长度取短者），与函数名一致；或明确改名为 `same_leaf_name` 并保留现有行为 —— 但要先确认调用方真想要哪种语义。

---

## 2. 自动化

### A1. 全流程 5 个人工卡点，且没有「复用既有 profile」的入口（高）

- 证据：`AddGameWizard.vue` 的链路 = 选目录（`chooseSource:139`）→ 选 exe（`chooseExecutable:153`）→ 开始学习（`beginLearning:246`）→ **用户必须进游戏手动保存一次** → 点分析（`analyze:265`）→ 审阅 scopes 后确认。Rust 侧没有任何自动确认路径。
- 可自动化空间（按收益排序）：
  1. **复用既有 profile 的形状**。`SaveProfile.executable_hash` 已经存了，但只用于校验，没用于复用。同厂商 / 同引擎（`steam_emu.ini`、`steam_appid.txt`、Engine 目录特征）的既有 profile，其 scope 形状（root_type + include_directories + exclude_*）几乎可以直接作为新游戏的初始草稿，用户只在不符时修改。这一步能把「必走一次完整学习」降级为「先给一份草稿、多数情况直接确认」。
  2. **目录推断前置**：现在 `discover_scan_roots` 只在学习会话开始时跑一次。可以在用户选定 exe 之后立刻跑一次「只读推断」，把候选目录先展示出来，让用户在启动游戏之前就能确认或修正 —— 避免跑完一整套学习才发现目录不对。
  3. **保存后自动分析**：现在是用户手动点「分析」。检测到已跟踪进程在写入候选目录后可以自动触发一次分析（给一个「N 秒无写入后自动分析」的窗口，复用 `transactions.rs` 已有的 2 秒静默切分思路）。
  4. **允许「跳过学习」**：既然容器名启发式 + 扩展名白名单已经能给出可用的初稿，应当允许用户直接进入手工编辑（现在向导是线性的，`phase !== "ready"` 就拦住 `beginLearning`，没有旁路）。

### A2. ETW 采集期无任何自我限制（中，有实物证据）

- 证据：`logman` 参数只有 `-o/-p/level 4/-ets`（`etw_capture.rs:95-106`），未设 `-b/-bs/-ct`（无缓冲/丢弃策略）；PID 过滤在**解析期**（`:421`），采集期是全系统；去重结构 `file_object_paths` / `written_file_objects` / `operations` / `files` 整场只增不减（`:379-381`、`:497-503`）。
- 实物：本机 `%APPDATA%\com.gamesaver.desktop\events\` 下存在单个 **422 MB** 的 `.etl` 与 **450 MB** 的 `.etl.csv`。会话期间没有任何峰值保护。
- 修法：会话加时长上限（超时自动进入分析，而不是无限等）、给 `logman` 加缓冲与丢弃参数、对 `operations` 设条目上限（超出按「已见文件」收敛）。

### A3. 目录发现强依赖「游戏名出现在目录名里」（中）

- 证据：`find_candidate_directories`（`:771-798`）用 `game_name_hints(game)`（`:934-1005`：display_name 分词 + exe stem + `steam_appid.txt`/`steam_emu.ini` 里的 appid）逐层匹配目录名，命中才成为扫描根。
- 后果：目录名不含游戏名时（日系/老游戏常见的 `%APPDATA%\<厂商>\...`、或与游戏名毫无关系的目录名）非管理员用户永远找不到根。ETW 侧能靠 `infer_scan_root_for_etw_file` 兜住，但那需要管理员权限 —— 所以这是「**非管理员 + 目录名不含游戏名**」的组合缺口，与 R1、R5 同属一条「无管理员权限时的能力下降」主线。

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

### P4. 哈希阶段串行（低）

- `commit` 逐文件 `read_stable_file` + `sha256_bytes` + `write_object_locked`（`save_repository.rs:73-80`），全项目无 `rayon`/`par_iter`。读 + 哈希天然可并行（写对象需串行或加锁），对大批小文件是长尾。

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
| 第 3.5 批 | **R2b** 收集侧「目录 + 候选过滤」，让学习**之后**新出现的存档自动纳入 | 行为变更：会改容器范围现有的收集结果，且需先把 `is_save_candidate` 下沉到 domain | 大 | 待办 |
| 第 4 批 | **A2** ETW 会话上限 / 缓冲参数 | 防护性 | 小-中 | 待办 |
| 第 5 批 | **A1** profile 形状复用、目录推断前置、保存后自动分析 | 自动化，涉及前端 | 大 | 待办 |
| 备选 | **A3 / R5 / R6** 边缘缺口 | — | 小 | 待办 |

第 1 批全部是「不加行为、只补覆盖与省开销」，可以一次做完；第 2 批要动评分，建议单独一笔并逐条变异验证；第 3 批是本轮真正的核心，但也是唯一可能**把范围收错**的改动，建议先只做「新文件自动纳入」的**提议**（在草稿里列出，由用户确认），而不是直接写进规则。

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
