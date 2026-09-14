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
| C2 | 中 | ✅ 已修（§0.3） | 云端清单是「读-改-写」，整个过程没有串行化；退出游戏的自动同步与用户手动同步可并发，导致丢条目 | `cloud_save_service.rs:195-238`、`launch_service.rs:626` |
| C3 | 中 | ✅ 已修（§0.4） | `download_file` 的临时文件名由目标路径推导（非唯一），并发/重入下载同一目标会撞同一个临时文件；出错路径不清理 | `baidu_netdisk_service.rs:796` |
| C4 | 低 | ✅ 已补（§0.7） | 云同步命令层（`cloud_save_commands.rs`）原本**零测试**；逐行读后发现真缺陷 H3，收口时按「只测得动、且值得测的那段」补了 3 条测试，并**有意放弃** workers 一层 | `cloud_save_commands.rs` 全文件 |
| H1 | **高** | ✅ 已修（§0.5） | 云本体清单的 `version_id` 由远端 `manifest.json` 决定，却未做路径校验就被拼进**本地**缓存路径，随即被 `remove_file` 与下载写入 —— 可删除 / 覆盖本机任意 `*.zip` | `cloud_manifest_service.rs:753`、`baidu_commands.rs:1511`、`game_body_package_service.rs:69` |
| H3 | 中高 | ✅ 已修（§0.6） | **C2 引入的回归**：`cloud_save_commands.rs` 认领云端操作后有三处 `?` 早退漏了释放，泄漏的 key 是纯内存、只有重启才清 → 该游戏云同步永久报「已有同步任务正在进行」 | `cloud_save_commands.rs:200/202/203/209`、`:281` |
| H2 | 中 | ✅ 已修（§0.6，含同日补正） | `rebuild` 用 `.ok().flatten()` 把「清单读失败」当成「清单不存在」，覆盖回写会静默丢掉 `sha256`/`file_count`/`total_bytes` → 该版本此后下载不再比对哈希。补正：第一版修法把「解析不了」也当成「取不下来」，会让坏清单**再也修不回来** | `cloud_manifest_service.rs:469` |

> 云端存档侧 C1–C5 全部收口；云本体侧 H1/H2 已修，另有 §7 的 **L1 / L2 / L3** 与一处疑似死代码**判定为不值得做**。已修的七条里，C5、H3 都属「一旦发生就要用户承担后果」（永久卡住 + 文案误导），C1、C3、H1、H2 是加固与静默降级。**两点值得单独记住**：H3 是 C2 自己带进来的回归，性质与它要修的 C5 同类；而 H3 是**逐行读出来的、不是测试抓到的**（C4 那 3 条补测上线时 H3 已修）。处置顺序见 §6。

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

## 0.3 C2 落地记录（2026-09-14）

**现状比审计时记的更糟**：审计说「云操作没按游戏串行化」，落地后发现是三处程度不一：

| 入口 | 原状 |
|---|---|
| `start_upload_save_version_task` | **完全没有互斥** —— 自动同步与手动同步都走这里，正是 C2 指的那对并发 |
| `start_restore_cloud_save_task` | 有互斥，但用的是 `reserve_maintenance` 的**整游戏 key**，且认领发生在**读完云端清单之后** |
| `delete_cloud_save_version` | **完全没有互斥**。它同样是「读清单 → 裁掉一条 → 整体写回」，漏得最彻底 |

**做法**：整游戏独占与云端同步是**两把不同的 key**，互相可见。

- 为什么不共用一把：云端同步可以在游戏运行期间进行（手动上传、还原），而本体更新那类操作会拒绝「游戏运行中」。共用会被 `running_games` 那类无关判据挡掉。
- 为什么又必须互相可见：两边会读写同一份本地存档数据（云端上传要打包存档、本体更新要动受管目录与封面）。所以 `claim_operation`（整游戏）与 `claim_cloud_operation`（云端）都在插入**之前**检查该游戏的任意一把 key 是否已被占用。
- 判据放在插入之前而不是「先插入、冲突再回滚」：`insert` 会真的写进集合，回滚前的那一瞬间并发调用会看到「无冲突」。
- 启动游戏那条路径改查两把 key：原先只查无前缀的，于是**云端操作进行中仍能启动游戏**，而两者都在动本地存档目录。这是审计里没写到、落地时才看出来的第二个洞。

**顺带收掉的第三个问题**：还原原先「先读云端清单挑好版本、再认领」。两次操作之间，一次上传可能刚好按 `keep_limit` 把所选版本从清单裁掉并删掉远程包 —— 于是拿着一个已不存在的版本去下载。现在认领在**读清单之前**。

**收敛**：`save_operations` 原先在五处各自手写 `contains`/`insert`/`remove`，其中两处用 `contains` 再 `insert`、两处直接用 `insert` 的返回值、一处前缀不同（`confirm:`）。现在统一走 `AppState::claim_operation` / `release_operation` / `has_exclusive_operation`，key 由 `game_operation_key` / `cloud_operation_key` 构造 —— 与 C1 同一条教训：同一条判定存多份，任一处加强都会被另几处绕过。

**两个刻意的设计取舍**：

1. **凭据是「key 字符串」而不是 RAII guard**。凭据要活到工作线程结束，而 Tauri 命令签名里的 `State<AppState>` 只是短命引用，借用它构造的 guard 移不进线程（`state does not live long enough`）。工作线程本来就能用 `app.state::<AppState>()` 取到同一个 `AppState`，于是沿用项目里既有的「认领 → 起线程 → 线程内释放」写法。
2. **`has_exclusive_operation` 只提供「传 guard 进来」这一种形态**。`save_operations` 是普通 `std::sync::Mutex`、不可重入，多一个自行取锁的同名方法就多一次在持锁状态下误调而自锁死的机会。落地过程中确实先写出了这个自锁死的版本，是在写注释解释「为什么需要 `_locked` 版本」时发现论证不成立才改掉的 —— `&MutexGuard<HashSet<_>>` 本来就会解引用成 `&HashSet<_>`，两把方法本就是多余的。

**释放时机**：两个工作线程都**先释放 key、再 `finish_sync`**。任务状态一旦推送成功，前端就可能立刻发起下一次同步；此刻若 key 还没释放，用户会莫名收到「已有任务正在进行」。

**验证**：

- 新增 4 条测试（283 → 287）：同一游戏第二次云端同步被拒且释放后可再认领；不同游戏互不阻塞；云端与整游戏独占**双向**互相排斥（两个方向都断言）；持锁判定与认领结论一致。
- **变异 ×2**：把 `any_operation_for` 退回「只查整游戏 key」（3 条测试失败），以及去掉 `claim_cloud_operation` 的冲突检查 —— 后者正是 C2 的原缺陷形态（2 条测试失败）。还原后回到 287 通过。
- 门禁：`cargo fmt --check` 干净、`cargo test --lib` 287 passed / 0 failed、clippy 0 error 且警告数与基线持平（34）。
- 附带确认：C5 里那条 `staging.exists()` 的 check-then-act 与 `single-instance` 是安全的 —— `acquire_or_focus_existing` 在 `setup` **之前**执行（`lib.rs:26` vs `:64`），只有首个实例会进入 setup 并跑启动清理，所以清理不会去删另一个实例正在用的暂存目录。

**未做的事 / 局限**：本次是**单元级验证**（互斥语义 + 变异），没有构造真实的双线程并发上传去打真实网盘。清单丢条目的机制（读-改-写）已被互斥覆盖，但「两次真实传输互斥后网盘上的最终清单是否完整」需要真实环境才能端到端确认。

**事后追认（2026-09-14，见 §0.6）**：这一批引入的认领**有一处漏了释放** —— `cloud_save_commands.rs` 认领之后的三处 `?` 早退（`:202`/`:203`/`:209`）都不释放，而泄漏的 key 是纯内存、只有重启才清，于是该游戏云同步**永久**报「已有同步任务正在进行」，与 C5 同类。上面的「验证」一节只覆盖了**互斥语义**，没有覆盖**失败路径上的释放** —— 这正是它漏掉的原因。现已改为 RAII 守卫，并有 2 条测试专门钉住释放时机（含早退与 `disarm` 交出的两个方向）。

---

## 0.4 C3 落地记录（2026-09-14）

**审计只记了「临时文件名不唯一」，落地后实际是三条独立的缺陷**：

| 缺陷 | 原状 |
|---|---|
| 名字不唯一 | `target_path.with_extension("download.tmp")` —— 同一目标必然推出同一临时名 |
| 失败路径漏清理 | 只有「取消」和「大小不匹配」两条写了 `remove_file`；**读、写、flush、sync 四条用 `?` 直接返回，全部残留** |
| 名字写法错误 | `with_extension` 会把目标原有的扩展名**替换**掉：`...v1.0.7.zip` → `...v1.0.download.tmp` |

漏得最狠的是第二条：**「读取失败」正好是会被反复重试的那类失败**（网络中断、服务端提前关闭），每次都留下一个可能几 GB 的半截文件，而没有任何地方会回收它。

**做法**（两处都在 `download_file` 内部，因此 6 个调用点一起受益）：

- `download_temp_path`：`with_file_name` 追加 `.{uuid}.download-tmp`。用 `with_file_name` 而非 `with_extension` 是为了保留原名；带 UUID 是为了唯一；留在**目标同目录**是为了最后那步 `rename` 不退化成跨卷「复制+删除」（几 GB 的本体包会白白多写一遍盘）。
- `DownloadTempGuard`：从建文件起接管清理，提交成功后 `disarm`。与 `cloud_save_service::TemporaryFileGuard` 同形，区别是它多一个解除语义 —— 它守的文件**本来就该**变成成品。

**顺带确认的事**：`cloud_account_service.rs:264`、`cloud_manifest_service.rs:307/419/610` 那四处 `temporary.with_extension("download.tmp")` 清理，扫的是**调用方自己生成的**临时文件（名字本来就带 UUID），本次改动后它们变成对不存在文件的空操作。选择留着而不是删掉：它们是调用方的兜底，删除属于扩大范围，且无害。这一点在此写明，免得后来者误以为 `download_file` 至今仍会产生那种文件。

**验证**：

- 新增 3 条测试（287 → 290）：同一目标两次调用拿到不同临时名、同目录、且保留原扩展名；守卫在未提交时删文件；守卫在 `disarm` 后保留文件（写错这里会删掉刚下好的成品，调用方看到「下载成功但文件不存在」）。
- **变异 ×2**：去掉临时名里的 UUID（失败在「两次调用不能同名」），以及让 `Drop` 变成空实现（失败在「未提交必须清掉」）。还原后回到 290 通过。
- 门禁：`cargo fmt --check` 干净、`cargo test --lib` 290 passed / 0 failed、clippy 0 error 且警告数与基线持平（34）。

**未做的事**：没有造真实并发下载去撞同一个目标（要真实网盘）。三条缺陷的判据都是纯路径/生命周期逻辑，已被单元测试与变异覆盖；但「两个真实并发下载互不干扰」这一点与 C2 一样，只有真实环境能端到端确认。

---

## 0.5 H1 落地记录（2026-09-14）

**怎么发现的**：审云本体下载 / 安装路径时，拿它和刚从 C1、F1 学到的东西对照 —— 同一个仓库里「远端 JSON 不可信」这条已经确立，于是逐个检查「远端字段 → 本地路径」的拼接点。F1 修的是**远端**出口（`package_path` 拿去删远端文件），这一条是它的**本地镜像**。

**问题**：`install_cloud_game_task` 在 `baidu_commands.rs:1506-1511` 取 `package.version_id` 拼本地缓存路径，而这个 `version_id` 可能就是**网盘上 `manifest.json` 里的值**（`CloudManifestService::project` 在 `package_path` 命中远端列表时采用清单里的 `version_id`），`validate` 对它只要求非空。于是：

1. `:1511` `BodyPackageService::package_path` 是纯 `join` → 结果可逃出缓存根；
2. `:1512` 对结果 `remove_file` → **删除**该路径文件；
3. `:1523` 把攻击者的 zip **写入**该路径（且写入发生在任何哈希 / 结构校验之前）。

因为后缀被强制成 `.zip`，目标是「本机任意 `*.zip`」。**关键区别**：F1 还要赌百度服务端怎么解析 `..`，而这里是本地文件系统，行为是确定的 —— 不必赌。

**不对称是漏改而非设计**：同一个文件里 `download_body_task:1088` 早就查了 `version_id`（空 / `/` / `\` / `.` / `..`），只有安装这条没查。与 C1（三份重复校验漏了第四处）、E（键的推导源不对）同一形状。

**做法（三层，入口 + 出口 + 去重）**：

| 层 | 改动 |
|---|---|
| 出口不可误用 | `BodyPackageService::package_path` 改为**返回 `Result`**，内部对 `game_uid` 与 `version_id` 各做一次 `is_safe_path_segment`。放在函数内而不是调用点：调用点有 5 处，摆在外面迟早漏一个（C1 的教训）。返回 `Result` 而非内部 sanitize：静默改写会把「攻击」变成「写到别的名字」，**报错才是正确的失败方式**；同时让新调用方无法绕过 |
| 入口拒绝 | `CloudManifestService::validate` 对 `versions[].version_id` 的要求从「非空」提到「安全路径段」，让被污染的清单**在读取时就失败** |
| 去除重复判定 | `download_body_task` 那份内联检查收敛到 `is_safe_path_segment`（比原内联更严：多拒控制字符并先 `trim`）。保留显式检查而非只靠 `?`，因为这条路径要顺手删掉已下载的临时文件 |

5 个调用点全部更新（`baidu_commands.rs` ×3、`game_body_commands.rs` ×1、服务内 ×1）。

**顺带堵住 H2 的洗白**：`rebuild` 用 `Self::read(...).ok().flatten()` 读旧清单并沿用其中的 `version_id` **回写**新清单（`cloud_manifest_service.rs:504-509`），所以 H1 的污染本来会被本体应用自己"洗白"并持久化。清单一旦在校验处被拒，`existing` 即为 `None`，新清单的 `version_id` 退回本地记录或文件名 —— 污染不再扩散。**但 H2 的另一半（把「读失败」当成「清单不存在」并覆盖式回写、丢掉 `sha256` 等元数据）没有修**，见 §7。

**测试与变异**：

- 新增 2 条测试（300 → 302）：`package_path` 的逃逸拒绝与「结果在缓存根之下」；清单校验对不安全 `version_id` 的拒绝（含反向守卫 —— 含空格与非 ASCII 的合法标识必须仍然通过，避免把校验做过头）。
- **变异 ×3**：M1 让 `package_path` 的守卫恒假 → 被前一条测试抓到；M2 把 `validate` 还原成「只查非空」→ 被后一条抓到；**M3 把安装出口故意改写成内联 `join`（绕过 `package_path`）→ 测试全绿、clippy 34 不变，无任何信号**。M3 是**故意绕过**，不是误删：误删 `?` 会因为 `Result` 与 `PathBuf` 不匹配而编译失败。这与 F1 披露的残余限制同级（要更强需要 `VerifiedPackagePath` 之类的 newtype，本轮未做）。
- 门禁：`cargo fmt --check` 干净、`cargo test --lib` **302 passed / 0 failed / 2 ignored**、clippy 0 error 且警告数与基线持平（34）。

**顺带把一条「推断」实测钉死**：`Path::join` 遇到绝对路径会**替换**基路径 —— 此前只有子代理按 std 文档推断，现在有一条断言（在 Windows 上）实测确认。这正是绝对路径必须被校验拒绝、而不能指望 `join` 把结果限制在根之下的原因。

**未做的事**：没有端到端复现（需要真实网盘账号 + 被改写的 `manifest.json`）。可达性是静态推导：前端 `App.vue:534` 只传 `remote_path` / `fs_id`，`install_cloud_game` 只校验 `remote_path`，`version_id` 完全在 Rust 侧由清单决定。威胁模型前提与 F1 相同（网盘上的清单可被改写）。

---

## 0.6 H3 + H2 落地记录（2026-09-14）

这一轮的两条都是**前面几轮自己带出来的**，所以先记来源。

### H3（中高）C2 引入的认领泄漏 —— 云同步永久卡死

**怎么发现的**：C4 一直被记作「覆盖缺口，非缺陷」。真要给它排优先级，就得先读那 414 行 —— 逐行读下来发现里面不是「没测试」，而是**有缺陷**。

**问题**：`claim_cloud_operation` 认领后，正常路径在 `begin_sync` 失败或线程结束时释放。但 `start_restore_cloud_save_task` 在认领与释放之间**有三次 `?` 早退**，全都不释放：

| 行 | 早退原因 | 现实触发 |
|---|---|---|
| `:202` | `load_baidu_client(&app)?` | 凭据读取失败 |
| `:203-204` | `fetch_manifest(...)?` / 清单不存在 | **网络抖动**（仓库本来就为 408/429/5xx 写了退避重试，说明这是常态） |
| `:209` | 找不到指定版本 | 该版本在列表与点击之间被剪枝掉 |

`delete_cloud_save_version` 同一个漏法：`:281` 的 `load_baidu_client?` 在 `:279` 认领之后、`:288` 释放之前。**它的注释显示作者想过这条纪律**，但只管到 `delete_cloud_version`，漏了它上面那句。

而 `save_operations` 是 `Mutex<HashSet<String>>`、**纯内存、进程存活期**（`app_state.rs:27/57`），没有任何启动清理。于是泄漏后 `claim_cloud_operation` 永远返回「该游戏已有云端同步或本体操作正在进行」—— **该游戏的云同步在这个进程里彻底不可用，只有重启能解**，而错误文案还把用户引向「稍等再试」。这就是 C5 那一类后果。

**这是我上一轮的回归**：`git log -S` 确认 `claim_cloud_operation` 是 `33ae0ca`（C2）引入这些命令的，而 C2 修的是「并发丢条目」。也就是说 C2 在消除一类后果的同时，引入了同类的另一种后果。同时**核实了 `baidu_commands.rs` 里 5 处 `reserve_transfer` 都是干净的**（认领之后除 `begin_sync` 外没有可失败调用），所以这是局部漏，不是普遍性理解错误。

**做法**：不再靠「在每个 `?` 前手写释放」——那正是漏掉的原因。新增 `CloudOperationClaim` 守卫（`app_state.rs`）：

- **`Drop` 释放**：命令作用域内任何早退（含 `?`）都会释放，结构上不可能再漏；
- **`disarm() -> String` 交接**：需要起工作线程时把 key **原样交给线程**，由线程在结束时释放。返回 key 而不是让线程用 `cloud_operation_key` 重新推导，是为了让「释放的」与「认领的」在类型上就是同一个值；
- **`#[must_use]`**：把守卫当语句丢掉（`CloudOperationClaim::claim(..)?;` 不绑定）会让它立刻 `Drop`，互斥**静默失效**、退回 C2 之前的竞态 —— 这种写法必须报警告。
- **刻意持 `&AppState` 而不是 `AppHandle`**：持 `AppHandle` 才能把守卫移进线程（还能顺手覆盖线程 panic 的路径），但那会让 `app_state.rs` 依赖 Tauri 类型，而 `AppState` 被大量单测直接构造 —— 一碰 Tauri 就会把窗口运行时链进测试二进制并直接启动失败（本文件开头的 `EventEmitter` 注释已记录这个坑，`0xc0000139`）。所以交接用 `disarm`，不移动守卫。

**顺带简化**：`delete_cloud_save_version` 不再有手写释放，`begin_sync` 的失败分支也由 `?` + `Drop` 取代。

### H2（中）`rebuild` 把「读失败」当成「清单不存在」并覆盖回写

**问题**：`cloud_manifest_service.rs:469-471` 用 `.ok().flatten()` 读旧清单，读失败（网络、格式非法、校验不过）被静默吞掉，随后按远端列表**整体重写** `manifest.json`。别的设备写下的 `sha256` / `file_count` / `total_bytes` 就此消失 → **该版本此后下载不再比对哈希，完整性校验被静默降级**。

**§0.5 让它更容易发生**：H1 的入口校验让「内容非法」也变成 `read` 返回 `Err`，于是收窄后一个被改坏的清单会让**整份**旧清单被弃用（连带丢掉全部版本元数据）。这又是上一轮带出来的副作用。

**但「读失败就不覆盖」不能简单照做** —— 那会让「修复清单」失效：`rebuild` 正是修复损坏清单的工具，而 `download_body_task:1042` 对校验失败的清单是**硬停**的。若 `rebuild` 也报错，一个被改坏的清单就再也修不回来（用户只能去网页端手改）。**所以必须区分两种失败**：

| 情况 | 处置 | 理由 |
|---|---|---|
| 取不下来（下载失败 / 大小不符 / IO 失败） | **不覆盖**，返回错误 | 远端那份可能完好，这一趟只是不顺；覆盖等于静默丢元数据 |
| 拿到了完整字节却解析不了 | **继续重建** | 远端清单本身坏了，没有元数据可沿用；而「修复清单」正是要修这种 —— **见下方补正** |
| 远端列表里确实没有清单 | 按列表新建 | 正常路径 |
| 能解析但校验不过 | **继续重建**，沿用其中元数据 | 这正是「修复清单」要修的场合 |

**做法**：把 `read` 拆出 `fetch`（只下载 + 解析，**不校验**），`read = fetch + validate + 缓存`，`rebuild` 直接用 `fetch` 并对上面四种情况分开处置。`fetch` 的失败要带上**性质**（`FetchFailure::Unavailable` / `Unparseable`），因为 `rebuild` 对这两种的处置相反，而 `read` 一视同仁都当错误。

**由此带出的第二道守卫**：既然 `rebuild` 现在会沿用一份**没过校验**的清单，它的 `version_id` 就不能直接采信（该字段会被拼进本地缓存路径）——抽出 `rebuilt_version_id()`，沿用旧值前先过 `is_safe_path_segment`，不安全就退回本地记录或文件名。这是**出口侧兜底**：`rebuild` 用的这份清单绕过了 `validate`，不能只靠入口那一处挡。

#### 补正：这个修法的第一版是错的（同日）

第一版把「取不下来」与「解析不了」**都**当成错误、都不覆盖，理由写的是「旧清单可能完好」。**这句话对「解析不了」不成立** —— 字节数完整却解析不出来，说明远端那份清单本身是坏的。而：

- `repair_cloud_body_manifest` 正是修坏清单的工具；
- 下载侧（`download_body_task`）对坏清单是**硬停**的。

两条加在一起：坏得最彻底的清单**再也修不回来**，用户只能去网页端手改。这恰好是我在 H2 里想避免的「不可修复」状态，**方向对了一半、另一半把修复工具自己堵死了**。而修改前的 `.ok().flatten()` 在这条路径上反而是对的（一律当不存在 → 一律重建）。

**为什么能干净地分开**：`download_file` 收尾会比对 `written != remote.size`（`baidu_netdisk_service.rs:843-848`），**下载不完整在那一步就报错了**，走不到解析。所以「解析失败」不可能是「我们只下了一半」，只剩「远端内容本身不合法」这一种解释。正是这个大小校验让分类有依据，而不是靠猜。

**改法**：`fetch` 的失败带上性质；`rebuild` 只对 `Unavailable` 报错，`Unparseable` 继续重建；`read` 两种都当错误（保持原语义，不能让下载侧以为「没有清单」而跳过校验）。策略抽成 `existing_manifest_for_rebuild`、解析抽成 `parse_manifest`，两者都能离线测 —— 否则「分类对不对」这一环要真实网盘才跑得到。

### 测试与变异

- 新增 4 条测试（302 → **306**）：
  - 守卫在早退时释放，且释放后能重新认领（后者才是「用户重试能成功」的判据）；
  - `disarm` 后守卫**不**释放，且交出的 key 确实能释放 —— 两头都验，否则线程结束时就成了空操作；
  - `rebuilt_version_id` 的采信规则（安全值沿用 / 不安全值退回本地 / 都没有则退回文件名）；
  - `rebuild` 读不到旧清单时中止。**这条做到了离线可测**：让 `temporary_root` 指向一个**存在的文件**，`fetch` 会在任何网络动作之前失败，而「中止」与「继续覆盖」会停在不同阶段、文案不同（`下载目录` vs 回写那步的 `临时目录`），断言错误来自**取清单**那一步即证明没有走到覆盖回写。
- **变异 ×5，全部被抓**：M4 守卫 `Drop` 空实现 → 抓；M5 `disarm` 改成 `clone`（守卫仍会提前释放）→ 抓；M6 `rebuild` 还原 `.ok().flatten()` 语义 → 抓；M7 去掉 `rebuilt_version_id` 的路径段兜底 → 抓；**M8 把守卫当语句丢掉 → clippy 34→35 报警（`unused CloudOperationClaim that must be used`）**。
- **补正的变异 ×3，也全部被抓**（本文件此时 306 → 310）：M13「解析不了也拒绝修复」（=改错的第一版）→ 抓；M14「取不下来也照样覆盖」（=原来的 `.ok().flatten()`）→ 抓；**M15「解析失败错归成取不下来」→ 抓** —— 这一条本来会漏，因为 M13/M14 测的是**策略**、而 M15 错在 `fetch` 里的**分类**，两者是不同的环。为此把解析抽成 `parse_manifest`，让分类那一环也能离线断言。
- 门禁：`cargo fmt --check` 干净、`cargo test --lib` **310 passed / 0 failed / 2 ignored**、clippy 0 error 且警告数与基线持平（34）、`npm run build` 通过。

**未做的事**：没有端到端复现。H3 的触发要真实的网络失败或剪枝竞态；H2 的「取不下来」也要真实网络。两者的核心判据（守卫释放时机、清单分三种情况处置）都已被单测与变异覆盖，但「真实网络抖动下用户点重试能成功」这一点只有真实环境能端到端确认。

---

## 0.7 C4 落地记录（2026-09-14）——「零测试」收口，附一次自我更正

C4 原本只写着「覆盖缺口，非缺陷」。真要给它排优先级就得先读那 414 行，于是读出 H3（已修，§0.6）。H3 修完后回头收口 C4 本身。

### 先更正我自己的估价

我原本认为这一层「价值最高」的测试是守住「终态必须落盘（用 `finish` 而不是 `update`）」。动手时发现 **`task_service.rs:286-293` 已经有一条测试正是这件事**：

> 云存档同步的"失败要响"就建立在这条契约上：终态、错误原因、重试参数都必须落盘。如果这里退回 `TaskService::update`（只改内存、不落盘），重启后 `TaskRepository::load` 的兜底会把任务标成「异常中断」并覆盖掉 error。

也就是说**不变式本身早就被测住了**，只是在 `TaskService` 层。所以我的估价要降一档：C4 能加的**不是**「证明落盘重要」，而是**证明命令层确实用了落盘的写法** —— 若有人把 `finish_sync` 改回 `update`，helper 的测试照样全绿。这是「接线守卫」，不是「不变式守卫」，两者的价值不同，不该混为一谈。

### 做了什么

**一次接缝（纯重构）**：`finish_sync` 只用到 `app.state::<AppState>()`，所以把它从吃 `&AppHandle` 改成吃 `&AppState`，绑定那一步移到两个调用方。这就是 `8d09f9f` 的同款做法 —— **外层解析依赖，内层只吃解出来的值**。

**3 条测试**（本文件从 0 条到 3 条；全仓 306 → 309）：

| 测试 | 守住什么 |
|---|---|
| `begin_sync_creates_a_running_cloud_save_task` | 任务是 `sync_cloud_save` + `TaskCategory::CloudSaveSync` + 立刻 Running。分类不是装饰，它决定任务是否进传输中心、能否取消、是否计角标 |
| `finish_sync_records_success_and_persists_it` | 成功走 Success/100、**不**留 error、**不**挂重试载荷；并断言 `tasks.json` 里是终态（守「这里用的是 `finish`」） |
| `finish_sync_records_failure_with_prefix_and_retry` | 失败走 Failed、文案拼成「前缀：原因」、error 记下原因、**必须**挂重试载荷（传输中心的「重试」按钮靠它） |

### 有意没做的一层（写下来，不留给以后猜）

`upload_save_worker` / `restore_save_worker` **不测**。它们要把 `AppHandle` 一路传进 `CloudSaveService::{upload_save_version, download_and_restore_cloud_save}`，而那两支又继续传给 `package_save_version`、`protect_current_save_version`、`SaveRepository::restore`、`GameRepository::persist`。要测就得在**最破坏性的那几条路径上做四、五层接缝重构**，还要注入假 `BaiduNetdiskClient`。

而收益与 `cloud_save_service` **已有的 16 条测试高度重叠** —— 那些测的正是这些服务的逻辑件（路径构造、清单校验、包元数据、逐文件哈希）。workers 本身只是「更新任务 → 取 client → 调服务」三段胶水。**风险与收益不对等，故不做。**

### 测试与变异

4 个变异全部被抓，且都是**被对应的那一条**抓住：`finish_sync` 的 Ok 分支改回 `update`（不落盘）→ 落盘断言失败；Err 分支终态写成 Success → 失败路径测试失败；删掉 `set_retry` → 重试载荷断言失败；`begin_sync` 用错任务分类 → 分类断言失败。还原后 **309 passed / 0 failed / 2 ignored**，clippy 警告数与基线持平（34）。

### 一个必须说清的限制

**这 3 条测试抓不到 H3。** H3 是逐行读出来的，而它上线时已经修完了；守卫的释放时机由 `app_state` 那 2 条测试守（§0.6）。所以 C4 的价值比我原先的说法**更窄**：它守的是**任务状态机与重试载荷**，不是认领生命周期。把「补了测试」说成「以后这类问题会被抓到」是不诚实的。

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

| 文件 | 测试数（2026-09-14 复核） |
|---|---|
| `cloud_save_service.rs` | 16 |
| `baidu_netdisk_service.rs` | 11 |
| `cloud_manifest_service.rs` | 9 |
| `cloud_account_service.rs` | 8 |
| `baidu_commands.rs` | 8 |
| `cloud_account_commands.rs` | 3 |
| `cloud_save_commands.rs` | **3**（原为 0，见 §0.7） |
| `baidu_config_repository.rs` | 1 |

> 表里的数字是 2026-09-14 复核过的**当前值**，不是审计当时的快照 —— C1、C3、F1、H1、H2 各轮都在加测试，原始表格（12 / 8 / 6 / 8 / 3 / 3 / 1 / 0）已经整体过时。另外「任务终态必须落盘」那条契约的测试在 `task_service.rs`（3 条），这是 C4 价值被摊薄的原因之一。

`cloud_save_commands.rs` 承载上传/恢复任务的编排（`begin_sync` / `finish_sync` / `upload_save_worker` / `restore_save_worker`），是最后一个没有测试的云文件。C2 那条并发缺陷正好落在这层 —— 若这里有测试，加锁行为会被钉住。

> **2026-09-14 修正：这条不再只是「覆盖缺口」。** 为给它排优先级而逐行读过后，发现里面有一条**真缺陷**：认领云端操作后有三处 `?` 早退漏了释放，导致该游戏云同步永久卡死（**H3**，C2 引入的回归，已修，见 §0.6）。
>
> 所以「零测试」在这里不是抽象的覆盖率问题，而是**它掩盖了一个与 C5 同级的后果**。不过要注意：H3 是**逐行读**发现的，不是测试发现的 —— 这一层的正确打开方式是**先读、再补测**。
>
> **2026-09-14 收口**：已补 3 条测试（`begin_sync` / `finish_sync` 的成功与失败路径），接缝用 `8d09f9f` 的做法把 `finish_sync` 的依赖从 `&AppHandle` 降到 `&AppState`。`upload_save_worker` / `restore_save_worker` **有意不测**（理由与代价见 §0.7）。另需说清：这 3 条**抓不到 H3**，守卫的释放时机由 `app_state` 的 2 条测试守。

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

1. **C1 的实际可达伤害范围** —— **已收口；「百度侧行为」仍未验证，但已不再影响我们。**
   原判断：百度网盘服务端对 `..`、`.`、空路径段的解析/归一化策略未知，因此「最坏能删到 `/apps/GameSaver/saves/` 之外的什么」无法在本机判定。需要真实网盘环境或百度接口文档确认。

   **后续追查：找到一个真实可达的未校验出口。** C1 修的是「我们拼出去」的路径，而**从服务端回来的**路径没人管 —— `CloudSaveService::fetch_manifest` 把下载到的 `manifest.json` 反序列化后**不校验任何字段**就返回（`from_slice::<CloudSaveManifest>(&raw)` 之后直接 `return Ok(Some(manifest))`），于是 `versions[].package_path` 是纯粹的远端输入，却原样流进两个出口：

   - **剪枝删除**（唯一会动远端文件的出口）：超出 `keep_limit` 时直接 `client.delete_file(&old_ver.package_path)`；
   - **下载导入**：直接拿它构造 `RemoteFile.path`。

   所以这条的性质变了：**输入侧确定可达**，未知的只剩「百度会怎么解析 `..`」这一个服务端行为。

   **处置：不去猜服务端，改成要求路径可证明地落在本游戏目录内。**
   - 新增 `verified_package_path(game_key, path)`：以 `remote_save_dir(game_key)` 为前缀，判定**复用**已有的 `validate_remote_package_path`（本体包同一条规则），不另写第二份安全检查；
   - 剪枝改为先分组：**验不过的路径绝不删**，且**它的清单条目不静默丢失**（留回清单 —— `keep_limit` 只是软上限，丢掉用户的版本记录才是实打实的损失）；
   - 下载导入在**任何远端 IO 之前**校验，验不过直接报错；
   - 守卫**返回**验证过的路径、调用方必须用它，这样「删掉守卫那一行」会让下游**编译失败**而不是悄无声息。

   **明确没动的地方**：`delete_cloud_version` 的 `remote_pkg` 是构造出来的、已被 C1 校验；`baidu_commands` 的两个删除入口已调 `validate_remote_package_path`；**本体清单的 `package_path` 不抵达任何删除**（只在 `cloud_manifest_service` 里用于与远端列表比对告警），本轮不动；`lib.rs` 里那个是本地 store 的路径且方向相反（作为「保留集」）。

   **仍未验证（保留）：百度服务端对 `..`、`.`、空路径段的归一化行为。** 本机无法判定，需要真实网盘环境或接口文档 —— 但它**已与我们的风险无关**：我们不再把未证明属于本游戏的路径发出去。

2. ~~**`game_key` 能否带 `/` 抵达存档路径**~~ —— **已追到底并修复（E）。**
   原判断：`add_game_commands.rs:30` 在本地添加游戏时**会**拒绝含分隔符的 key；但云端导入路径（`baidu_commands.rs:1591` `derive_game_key(&catalog.game_key)`）只做小写与空白折叠，**不拒绝分隔符**。该 remote catalog 的来源与可信度未追到底，因此这条只算可疑，未计入 C1 的确定部分。

   **追查结果：能抵达，而且我原来的定位偏了。** 真正的推导源不是 `catalog.game_key` 而是 `catalog.display_name` —— 新游戏走 `Game::new_pending(catalog.display_name)`，键由**显示名**推出；而 `validate_catalog` 对 `display_name` 只要求非空（它只对 `catalog.game_key` 要求等于目录段）。`derive_game_key` 不处理分隔符，于是云端一个叫 `Pokemon Red/Blue` 的游戏会存成 `pokemon red/blue`。可达性也没问题：`list_cloud_games:291` 只校验 `catalog.game_key`、不校验 `display_name`，这种游戏照样出现在列表里、照样能点安装。

   **实际后果与预期不同。** C1 落地后这不再是路径逃逸（`is_safe_path_segment` 会拒绝），而是变成一个**功能性死路**：该游戏此后每一个云存档操作都被拒绝，用户永远同步不了存档，且无从修复。C1 把「危险」换成了「不可用」，两边都需要在入口处拦住。

   **同时确认本来就没问题的几处**（避免高估这轮的覆盖面）：`remote_body_dir` 已在两个入口校验；`validate_catalog:772` 已要求 `catalog.game_key` 与目录段精确相等；`cloud_account_service::validate:448` 对导入清单里每个游戏都查 `is_safe_path_segment`，且 `fetch_profile:260` 会调用它 —— 所以**云账号导入路径没有漏洞**，我最初怀疑它是错的；`list_cloud_games:183-185` 会过滤掉目录段不安全的游戏。真正剩下的缺口只有「存进本地 store 的那个键」。

3. **并发上传同一版本是否可能**：自动同步与手动同步若恰好都指向同一 `version_id`，C2 的丢条目不会发生（retain-then-push 对同一 id 幂等）。C2 的成立依赖两条路径取到不同版本，这一点在 UI 上是可能的（手动选历史版本），但我没有实测复现。

   **补充（C2 落地后的状态）**：这条现在已 **moot** —— C2 的互斥不依赖该前提是否成立，只要两个上传互斥，交错就不可能发生。它的剩余价值只是判断「当初那条审计发现是真 bug 还是理论推演」，而不是决定要不要修。另外前提比原文写的更窄：`start_upload_save_version_task` 收的是**显式** `version_id`，自动同步（`launch_service.rs`）传的是刚捕获生成的那个版本，所以只有当用户**同时手动上传另一个（例如历史）版本**时才满足「两个不同 id」。C2 之前上传路径**没有任何互斥**，所以这个窗口是真实存在的 —— 只是没能实测复现。


---

## 6. 建议的处置顺序

| 批次 | 内容 | 性质 |
|---|---|---|
| 第 1 批 | ~~**C1** 三个路径构造函数补 `is_safe_path_segment`，并收敛三份重复校验~~ **已完成（见 §0.1）** | 纯加固 |
| 第 2 批 | ~~**C5** 启动恢复里清理 `.cloud-installing` 暂存目录，并修正「请重启应用」的文案~~ **已完成（见 §0.2）** | 小改，直接消除需要手工干预的卡死 |
| 第 3 批 | ~~**C2** 云同步命令按 `game_uid` 取 `save_operations` 占位~~ **已完成（见 §0.3）** | 小改 |
| 第 4 批 | ~~**C3** 临时文件名加 UUID + RAII 清理~~ **已完成（见 §0.4）** | 小改 |
| 第 5 批 | **C4** 为 `cloud_save_commands.rs` 补编排层测试 | 补验证 |
| 第 6 批 | ~~**H1** 云本体 `version_id` 的本地路径出口：`package_path` 返回 `Result` + 清单入口校验 + 收敛重复判定~~ **已完成（见 §0.5）** | 纯加固，成本低 |
| 第 7 批 | ~~**H3** `cloud_save_commands.rs` 的认领泄漏（C2 回归）改用 RAII 守卫；**H2** `rebuild` 区分「读不到清单」与「清单不存在」~~ **已完成（见 §0.6）**；其中 H2 的修法第一版把「解析不了」也当成「取不下来」，**堵死了修复工具，同日补正**（见 §0.6 补正） | 修自己的回归 + 小改 |
| 第 8 批 | ~~**C4** 为 `cloud_save_commands.rs` 补编排层测试（收在 `begin_sync` / `finish_sync`，workers 一层有意放弃）~~ **已完成（见 §0.7）** | 补验证 |
| 第 9 批 | **§7 的 L1 / L2 / L3** 与一处疑似死代码 | 未处置；判为低价值，见下 |

**优先级说明**：C5 当初排在 C2 之前，是因为它的后果最重且成本极低 —— C2 需要一次竞态才发生，而 C5 一旦发生就**永久**卡住该游戏，且错误信息把用户引向无效操作。C1 虽然级别最高，但它的实际可达伤害取决于百度服务端行为（见 §5 存疑项 1），而补上校验的成本接近于零，所以仍排第一。

**第 7 批为什么排在 C4 前面**：C4 一直被当成「覆盖缺口」，但真要给它排优先级就得先读那 414 行 —— 读下来发现 H3（认领泄漏 → 云同步永久卡死）。**它是 C2 自己带进来的回归，后果与 C5 同类**，所以优先于「补测试」本身。

**C4 的剩余价值已被前几批改变，并在第 8 批收口**：它原本的建议是「补编排层测试，可同时钉住 C2 的加锁行为」。C2 落地后**互斥语义已由 `app_state` 的测试钉住**（含双向排斥与持锁判定一致性），C3 的临时文件生命周期也有了测试，H3 的守卫释放时机现在也有 2 条测试。第 8 批补上了 `begin_sync` / `finish_sync` 的 3 条，接缝用的是 `8d09f9f` 的做法。**收口时刻意收窄**：`upload_save_worker` / `restore_save_worker` 不做（理由见 §0.7）—— 要做得在破坏性最强的几条路径上做四、五层接缝，收益又与 `cloud_save_service` 已有的 16 条重叠。

**顺带一条方法论**：C4 这次的教训是「**先读，再补测**」。H3 是逐行读 414 行读出来的；如果当时直接按「补覆盖率」的思路写测试，很可能写出 20 条绿测试、而 H3 一个都没碰到 —— 因为它躲在三个 `?` 早退的**失败路径**上，而按函数主路径写的测试天然只走 happy path。§0.7 也记了：这 3 条测试**抓不到 H3**。

**§7 剩余三条为什么判为不值得做**：L1 的可达性未证实（子代理与我都没能构造出复现），且后果可由 `repair_cloud_body_manifest` 重建；L2 不是漏洞（缓存 miss 且会自愈）；L3 是语义缺陷、非越权、可达性不清。都记为「已知、不处理」，等出现实际症状再回头。

---

## 7. 云本体侧的新发现

审云本体这一侧（`cloud_manifest_service.rs` 1024 行 / 6 条测试、`baidu_commands.rs` 2087 行 / 8 条测试）时一并发现下面这些。

**状态**：H1 已修（§0.5）；**H2 与 H3 已修（§0.6）**；剩下的 **L1 / L2 / L3** 与一处疑似死代码**判定为低价值、不处理**（理由见 §6 末段）。这样列是为了让「还没做什么」保持明确。

### H3（中高）认领泄漏 → 云同步永久卡死 —— ✅ 已修（§0.6）

不在被审文件里，是读 `cloud_save_commands.rs`（C4）时发现的。**C2 引入的回归**：认领后三处 `?` 早退漏了释放，泄漏的 key 是纯内存、只有重启才清。详见 §0.6。

### H2（中）`rebuild` 把「读失败」当成「清单不存在」并覆盖式回写 —— ✅ 已修（§0.6）

`cloud_manifest_service.rs:469-471` 用 `Self::read(...).ok().flatten()` 读旧清单，读失败（网络错误、格式非法）被静默吞掉，随后按远端列表**整体重写** `manifest.json`（`:539`）。后果：其它设备写下的 `sha256`、`file_count`、`total_bytes` 静默丢失 —— **该版本此后下载不再比对哈希，完整性校验被静默降级**。

修的时候发现「读失败就不覆盖」不能直接照做（会让「修复清单」这个工具本身失效，见 §0.6 的三情况表），所以改成区分三种情况；并补了出口侧兜底 `rebuilt_version_id()`。

**另一半的历史**：「沿用远端清单的 `version_id` 回写、把 H1 的污染洗白」这一半（`:504-509`）在 §0.5 就被入口校验堵住了（清单被拒 → `existing` 为 `None` → 退回本地记录或文件名）。

### L1（低）清单读-改-写没有服务内串行化 —— 不处理

`rebuild`（`:460-541`）是 read → build → 整体 `write_manifest`：文件内无锁，`AppState` 也没有清单级锁。互斥只靠 `reserve_transfer(game_uid)`，而 `delete_remote_body_package` 在本地找不到该 uid 时改用 `format!("remote:{game_key}")`（`baidu_commands.rs:520-524`）—— 两把 key 互不可见，于是「云端专属删除」与「同 key 本地游戏的本体写入」可并发对同一目录读-改-写并丢更新。

判低而非中：`project` 对清单问题只降级，`repair_cloud_body_manifest` 可从远端列表重建；且该重叠需要前端传一个「store 里不存在、但同 key 游戏存在」的 uid，正常 UI 走不到。**未能构造端到端复现。**

### L2（低）`cache_folder_name` 的 `:` 碰撞 —— 不处理

`cloud_manifest_service.rs:142-149` 做的是 `/ \ :` → `_` 再 `trim_matches('_')`，**不是哈希**（早前一份会话笔记说它「哈希了目录名」，那是错的；本报告从未这样写过）。因为所有调用点的 `remote_dir` 都来自已过 `is_safe_path_segment` 的 `remote_body_dir`，**逃逸不成立**：`/`、`\` 的替换只命中固定前缀，空 / `.` / `..` 不可达，Windows 保留名被固定前缀挡住。

残余：`is_safe_path_segment` 不拒 `:`，于是 `"a:b"` 与 `"a_b"` 映射到同一缓存目录（两者都是合法 key）。后果限于缓存串味，且命中时会重新 `validate` 并要求 `game_key` 精确相等，多数情况自愈为 cache miss。**判定为低，不是漏洞。**

### L3（低）用远端文件元数据缓存本地生成的 catalog —— 不处理

`game_commands.rs:105-121`（改显示名时异步触发）把本地的 `catalog_from_game(&game)` 配上网盘列表里的 `fs_id` / `size` / `server_mtime` 存进 `catalog.cache.json`；之后 `read_catalog` 命中缓存（`:394-399`）会返回**本地版本**而不是云端 `game.json`。影响：云游戏列表 / 云安装可能使用本地启动配置而非云端配置（同一 game_key，**非越权**），属语义缺陷。附带记录：缓存有效性判定未含 `RemoteFile.md5`。

### 附带：一处疑似死代码（非安全问题，未确认）

`baidu_commands.rs:111-133` 与 `cover_protocol.rs:98-113` 会读 `cache/<folder>/game.json`、`manifest.json` 来反查 gameKey，但全仓 grep 显示缓存目录只写 `catalog.cache.json` / `manifest.cache.json` / `cover.cache.json` / `cover.jpg` —— 这两段容错分支疑似**永不生效**。未能确认是否存在我未找到的写入者（可能在旧版本或前端）。

### 这一侧判定为干净的部分

同一轮里也逐条核过、**没有**问题的：

- **远端 `package_path` 的所有消费点**（F1 在存档侧的形态）：`project:619-638` 只把它当 HashMap key 与远端列举结果精确匹配，命中只影响展示 / 比对字段；`rebuild:514` 写回的是**远端列表的** `file.path`，不是清单里的 `package_path`；**没有任何** `delete_file` / `upload_file` / `ensure_directory` 以它为输入。
- **`catalog.executable_relative_path`**（只要求非空）：使用前必经 `normalize_relative`（`game_body_package_service.rs:1108-1131`），拒绝对路径、`..`、`RootDir` / `Prefix` —— fail-closed。
- **`catalog.working_directory_relative_path`**：`launch_service.rs:287-299` 用 `safe_join`，非法即启动失败 —— fail-closed。
- **`game_key`（清单与 catalog）**：要求与目录段**精确相等**，`game_uid` 过 `is_valid_game_uid`（仅 alnum / `-` / `_`）。
- **`display_name`**：只要求非空，但出口已收口 —— 安装路径推出的键在 `baidu_commands.rs:1607` 被 `cloud_install_game_key` 换掉（E）；Rust 侧未发现把它当路径段使用的地方（**前端未审计**）。
- **缓存读写**：命中条件含 `fs_id` + `size` + `server_mtime` 全等**且**重新 `validate`；`save_cached_*` 只在校验通过后调用。
- **封面路径**：本地临时名是 UUID，远端输入不参与命名。
- **`cloud_manifest_service.rs` 非测试代码无 `unwrap()` / `expect()` / `panic!`**；`let _ =` 全部是缓存写入与临时文件清理 —— 问题不在「忽略错误」，而在**校验的覆盖面**。
