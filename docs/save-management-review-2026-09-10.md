# 存档管理专项审查（2026-09-10）

审查对象：存档管理链路 —— 本地 CAS 存档版本、保留策略、恢复/回滚、以及存档的云端打包上传与下载导入。

范围文件：

| 文件 | 行数 | 角色 |
| --- | --- | --- |
| `repositories/save_repository.rs` | 1559 | 本地 CAS 主库：提交、恢复、回收 |
| `services/cloud_save_service.rs` | 916 | 存档打包、上传、下载导入、恢复 |
| `commands/save_version_commands.rs` | 524 | 恢复 / 删除 / 清理的对外命令 |
| `commands/save_commands.rs` | 918 | 存档范围与保留数配置 |
| `services/launch_service.rs` | — | 游戏退出后提交版本（最常走的路径） |
| `components/GameDetailPage.vue` | — | 保存版本时间线 + 云端存档抽屉 |

---

## 0. 结论摘要

| 编号 | 严重度 | 一句话 | 位置 |
| --- | --- | --- | --- |
| V1 | **高** ✅ 已修复 | 存档对象回收（GC）发生在持久化**之前**，写盘失败会让磁盘上的版本指向已被删除的对象，变成静默不可恢复 | 3 处，见正文 |
| V2 | 中 ✅ 已修复 | 恢复的前置校验把「已删除文件」的墓碑条目当成活条目，墓碑引用的目录一失效就整场恢复失败 | `save_repository.rs` `build_restore_groups` |
| V3 | 中 ✅ 已修复 | 打包上传的兜底读取不校验内容哈希，却能上传成功 —— 产出一份「看着在、永远还原不了」的云端备份 | `cloud_save_service.rs:130-136` |
| V4 | 中 ✅ 已修复 | 打包时只按 `root_type` 选存档范围，忽略 `root_path`，多范围同类型时会读错目录 | `cloud_save_service.rs:736-746` |
| V5 | 中 ✅ 已修复 | 改保留数时未重算 `latest_save_version_id`，指针悬垂后每次退出都会无条件新建版本 | 4 处，见正文 |
| V6 | 中 ✅ 已修复 | 解压前不校验大小（代码审查报告 P2-2 在存档侧的主路径确认） | `cloud_save_service.rs` `read_zip_entry_bounded` |
| V7 | 低 ✅ 已修复 | 恢复过程在用户存档目录内留下的 `.gamesaver-restore-*` / `.gamesaver-rollback-*` 无启动清理，且会被下一次提交当成存档收进版本 | `save_repository.rs` `sweep_restore_artifacts` + `lib.rs` 启动钩子 |
| V8 | 低 | 版本排序按 `created_at` **字符串**比较，改用 ISO 时间后保留策略会删错版本 | 5 处 |
| V9 | 低 | `wildcard_matches` 不支持中间通配符且静默失效 | `save_repository.rs:1017-1030` |
| V10 | 低 | 恢复会删除「当前存在但不属于目标版本」的受保护文件，确认文案未告知 | `save_repository.rs:448-508` |

高置信度：V1 / V2 / V3 / V4 / V6 / V9（纯静态可判）。
V5 / V7 / V8 / V10 涉及运行时序列，触发条件已在各条正文写明。

**修复进度：V1、V2、V3、V4、V5、V6、V7 已修复（2026-09-10）；剩 V8 / V9 / V10。**

---

## 1. 高优先级

### V1 存档对象回收发生在持久化之前，写盘失败即产生「不可恢复的版本」—— 高 ✅ 已修复（2026-09-10）

**相同的形状出现在三处**，其中第一处是每次游戏退出都会走的主路径：

| 位置 | 剪枝 | 回收 | 持久化 |
| --- | --- | --- | --- |
| `services/launch_service.rs` | `:410-412` | `:413` | `:426` |
| `commands/save_commands.rs`（改保留数） | `:731-733` | `:734` | `:737` |
| `commands/save_version_commands.rs`（恢复前保护） | `:256-258` | `:259` | `:271` |

三处都是同一个模式：

```rust
state.with_store_mut(|candidate| {
    candidate.save_versions.retain(/* 剪掉超出保留数的旧版本 */);
    let _ = SaveRepository::collect_garbage(app, &candidate.save_versions); // ← 先删对象
    ...
    GameRepository::persist(app, candidate)?;                                // ← 后写盘
    Ok(())
})
```

**为什么是 bug**：

- `AppState::with_store_mut`（`app_state.rs:77-89`）的契约是「闭包返回 `Err` 时不提交，内存态保持原样」，并且有专门的测试 `with_store_mut_discards_the_mutation_on_error` 守着这条契约（`app_state.rs:254-267`）。
- `GameRepository::persist`（`game_repository.rs:57-78`）走 `atomic_replace`，**失败即返回 `Err`，磁盘上仍是旧内容**。
- 于是 `persist` 一旦失败：`collect_garbage` 已经按「剪枝后的版本集」把对象文件删了，而磁盘 `store.json` 仍然列着那些被剪掉的版本。

**后果**：重启后这些版本照常出现在「保存版本」列表里，点「恢复」必然失败（`restore_group` 里 `save_repository.rs:464` 的 `sha256_file(&object) != hash` 判定 → 「存档对象校验失败」）。用户看不到任何异常，只是某一天发现几个历史版本"坏了"。

注意触发窗口恰好与剪枝同时发生 —— 也就是**只有真的存在要被回收的版本时，才有东西可丢**。

**同文件已有正确写法可对照**：`delete_versions_task`（`save_version_commands.rs:344-386`）把 `persist` 放在闭包内、**把 GC 放在 `with_store_mut` 返回之后**，用返回值里的版本列表去回收：

```rust
let versions = state.with_store_mut(|candidate| {
    candidate.save_versions.retain(...);
    GameRepository::persist(app, candidate)?;
    Ok(candidate.save_versions.clone())     // ← 把结果带出来
})?;
SaveRepository::collect_garbage(app, &versions)   // ← 落盘成功后才回收
```

**修法**：把三处统一成 `delete_versions_task` 的形状 —— 从闭包返回剪枝后的 `save_versions`，持久化成功后再调 `collect_garbage`。

**兼带问题**：`collect_garbage` 是「遍历整个 CAS 目录」的全盘扫描，却被放在持有全局 `store` 锁的闭包里执行，**违反 `with_store_mut` 自己的文档约定**（`app_state.rs:75-76`：「也应避免在其中做长耗时的网络请求或多轮全盘扫描，否则会阻塞其他读写者」）。改成上面的形状后，GC 自然移到锁外，这个问题一并消失。

**修复记录（2026-09-10）**

新增 `SaveRepository::prune_game_save_versions(store, game_uid, keep_versions)`（`repositories/save_repository.rs`，紧邻 `collect_garbage`），把三处逐字重复的剪枝逻辑收进一个纯函数：**只改内存、不碰磁盘**，返回 `Some(剪枝后的存活集)` 或 `None`（无版本被删）。三处调用点统一改为「闭包内剪枝 → 持久化 → 闭包返回成功之后再回收」：

| 位置 | 改法 |
| --- | --- |
| `services/launch_service.rs`（会话提交主路径） | 闭包返回 `(summary, 剪枝存活集)`；`collect_garbage` 移到 `with_store_mut` 返回之后 |
| `commands/save_commands.rs`（改保留数） | 剪枝与持久化收进独立作用域，`collect_garbage` 放在作用域之外 |
| `commands/save_version_commands.rs`（恢复前保护） | 闭包返回剪枝存活集，仅在 `Ok` 分支落盘后才回收 |

顺带修掉两个同源问题：

1. `launch_service` 中「游戏记录不存在」提前 `return Err` 的路径，此前也会先执行 GC 删掉对象，现已不会（对象删除一律发生在落盘成功之后）。
2. 三处 GC 都不再在持有 `store` 锁期间做 CAS 全盘扫描 —— 交给纯函数后，回收点自然落在锁外。

回归测试：`save_repository.rs` 新增 2 条纯函数测试（剪枝只保留最新 N 条且不动其他游戏；未超限/`keep=0` 时必须返回 `None` 以免触发无谓全盘扫描），`save_commands.rs` 原有剪枝测试改为直接调用真实函数（此前是在测试里重抄一遍排序）。`cargo test --lib` **183 passed / 0 failed**；`cargo fmt` 干净；`cargo clippy` 保持 **lib 30 / lib test 34**，新增代码零告警。

> **覆盖度说明**：单测环境**无法**构造 `AppHandle`（会让测试二进制因 GUI 依赖启动失败），而 `persist` / `collect_garbage` 都依赖它 —— 因此「先持久化、后回收」这个**顺序**本身没有被自动化测试直接覆盖，测试覆盖的是抽出的纯函数契约（存活集正确、无谓调用被抑制）。顺序的正确性由代码结构保证：GC 调用点物理上位于 `with_store_mut` / 守卫块之外。

---

## 2. 中优先级

### V2 恢复被「已删除文件」的墓碑条目阻断 —— 中 ✅ 已修复（2026-09-10）

`build_restore_groups` 对版本里的**每一条** entry 都执行 scope 解析与 root 校验：

```rust
for entry in &version.files {
    let relative = validate_relative(&entry.relative_path)?;
    let scope = find_scope_for_entry(profile, entry, &relative)?;   // 修复前
    let root = scope_root(game, scope);
    if root.exists() && !root.is_dir() { return Err(...); }
    if !root.exists() && !can_create_missing_restore_root(scope.root_type) {
        return Err(format!("存档范围不存在，请先创建或重新选择：{}", root.display()));
    }
    ...
    if !entry.deleted { /* 只有非 deleted 才进入恢复列表 */ }
}
```

但 `deleted: true` 的条目只用来表达「这个文件在那个版本里不存在」，它**不需要 root 存在、也不需要 scope 能解析**。修复前却对它一视同仁。

**触发**：某个 tombstone 引用的自定义目录（`custom` / `managed_game`）后来被删除，或该 scope 被用户从 profile 里移走。此时：

- root 不存在 → 报「存档范围不存在，请先创建或重新选择」；
- scope 已移除 → `find_scope_for_entry` 报「保存版本中的文件不属于当前存档范围」。

两种情况下**整场恢复直接失败**，而报错指向的路径与用户真正想恢复的存档内容毫无关系 —— 用户无从修复，因为那个目录本来就不该存在。

**修法**：`deleted` 条目跳过 root/scope 前置校验。

**修复记录（2026-09-10）** —— `save_repository.rs`：

- `build_restore_groups` 的循环开头新增墓碑分支：`if entry.deleted { ...; continue; }`，墓碑不再进入 `validate_relative` / `find_scope_for_entry` / root 存在性校验，也不会因缺少 `object_hash` 而报「保存版本缺少对象」。
- 但墓碑**不能一刀切跳过**。它有一个真实的、必须保留的职责：让「曾经存有它的那个范围」也参与本轮恢复，否则该范围里当前多余的受保护文件不会被清掉 —— 而「游戏把存档目录整个清空」产生的恰恰是一个**只剩墓碑**的版本，恢复到那一版就等于把存档清空。一刀切跳过会让这种版本直接撞上「保存版本没有可恢复的存档文件」。因此新增 `deleted_entry_restore_root`：能安全定位到**现存目录**时登记该范围（复用新抽出的 `register_restore_group`，顺带把原来内联的建组逻辑收成一个函数），定位不到才跳过。
- `deleted_entry_restore_root` 的取态是**保守**的：只在「条目记录的 `root_path` 与某个范围精确吻合」时才认领（老条目没记录 `root_path` 时，仅当同类型范围唯一才认领）；目标路径不存在或不是目录时放弃；任何不确定都返回 `None` 而非报错。**删除是有破坏性的动作，宁可漏清也绝不猜** —— 这与读取侧 `find_scope_for_read` 的宽松取向有意相反（读旧文件无害，删错目录是数据损失）。
- 抽出的 `register_restore_group` 同时服务于活条目与墓碑：分组的 `protected_paths` 取自**所有**根目录落在同一物理路径上的范围，与「哪个条目触发登记」无关，这正是墓碑不必做 scope 校验的依据。

回归测试（`save_repository.rs` 新增 3 条）：墓碑引用已删除的自定义目录（同类型范围仍在）不得阻断恢复且只登记真实存在的根；墓碑所属范围类型已从 profile 移除时同理；**只剩墓碑但目录仍在**时必须登记该范围、`entries` 为空（钉住「不能一刀切跳过」这个边界）。测试里把范围收窄到 `confirmed_files`、并清空 `include_directories` —— `SaveScope::new_manual` 默认的 `["."]` 会让范围匹配一切，从而掩盖这条回归。

**变异验证**：① 让墓碑改回旧的严格校验 → 恰好前两条失败、第三条通过；② 让墓碑一刀切 `continue` → 恰好第三条失败、前两条通过。两次变异的失败集合互不重叠，说明「跳过校验」与「保留清理职责」各有一条测试守着。

**同一家族的问题（本次有意未改）**：`find_scope_for_entry` 对非删除条目也是硬失败。用户通过「更改存档目录」把范围改窄之后，旧版本里落在范围外的文件会让该版本从此无法恢复 —— 而 UI 上这些版本照常列出、按钮照常可点（`GameDetailPage.vue`），没有任何「此版本不可恢复」的标记。**没有一并放宽**，因为这是「要写到哪去」的判定：放宽等于允许恢复写入用户已明确移出保护范围（或加了排除规则）的目录，风险方向与墓碑不同。合理的做法是保留严格校验、在 UI 上把不可恢复的版本标出来，属独立改动。

### V3 打包上传的兜底读取不校验哈希，云端包会自校验失败 —— 中 ✅ 已修复（2026-09-10）

`package_save_version`（`cloud_save_service.rs:96-159`）为每个非删除条目取字节：

```rust
let bytes = if let Some(hash) = &entry.object_hash {
    SaveRepository::read_object(app, hash)
        .or_else(|_| read_file_from_scope(game, profile, entry))?   // :131-133
} else {
    read_file_from_scope(game, profile, entry)?                     // :135
};
```

`read_object` 自带 SHA-256 校验；而**回退分支 `read_file_from_scope` 读的是磁盘上的活文件，没有校验读到的字节是否等于 `entry.object_hash`**。

同时包内 meta 写的是原样克隆的版本（`:115-121` `version: version.clone()`），也就是**旧的哈希**。而下载侧的校验正是拿 meta 里的哈希去比对数据项：

- `validate_downloaded_save_meta`（`:623-655`）从 `meta.version.files` 构建 `relative → (hash, size)`；
- `download_and_import_save_version`（`:317-323`）逐项 `sha256_hex(&data) != *expected_hash` 即报错。

**后果**：只要 CAS 对象缺失或损坏过（而磁盘上的存档文件已经变化），上传会**成功**、云端清单会**列出该版本**，但还原时包内数据与声明哈希对不上 → 校验失败。这是一份「看着存在、永远还原不了」的备份 —— 恰恰是备份最不该有的失败形态。

**修法**：回退读取后立即校验 `sha256(bytes) == entry.object_hash`，不一致就直接报错（宁可上传失败，也不要留下一份假备份）；或者承认要按磁盘现状打包，那就同步把 meta 里该条 entry 的 `object_hash` / `size` 改写成实际读到的值。

**修复记录（2026-09-10）** —— 采用前者（宁可失败）：

`package_save_version` 里内联的取值分支抽成 `read_entry_bytes` + `resolve_entry_bytes` + `ensure_bytes_match_hash` 三个函数（`services/cloud_save_service.rs`，紧邻 `read_file_from_scope`）：

```rust
let Some(hash) = hash else { return read_fallback(); };
match object {
    Some(Ok(bytes)) => Ok(bytes),                     // CAS 命中，read_object 已自带校验
    Some(Err(object_error)) => {
        let bytes = read_fallback().map_err(...)?;
        ensure_bytes_match_hash(&bytes, hash, relative_path)?;   // ← V3 的修复点
        Ok(bytes)
    }
    None => read_fallback(),
}
```

把「回退必须复算哈希」这条规则落在一个**纯函数**上，是因为对象读取本身依赖 `AppHandle`（单测里构造不出来）。这样测试可以完整驱动「对象缺失 → 回退 → 内容漂移 → 必须报错」这条路径，而不只是测一个孤立的比较函数。错误文案同时带上了期望哈希与实际哈希，便于用户定位是哪个文件漂移。

### V4 打包时只按 `root_type` 选范围，忽略 `root_path` —— 中 ✅ 已修复（2026-09-10）

`read_file_from_scope`（`cloud_save_service.rs:736-750`）：

```rust
let scope = profile
    .scopes
    .iter()
    .find(|s| s.root_type == entry.root_type)      // ← 只比 root_type，取第一个
    .ok_or_else(|| format!("未找到匹配的作用域：{:?}", entry.root_type))?;
let root = crate::repositories::save_repository::scope_root(game, scope);
let file_path = root.join(&entry.relative_path);
```

同一个 `root_type` 下存在多个 scope 是**常态**——恢复侧为此专门实现了三级消歧（`find_scope_for_entry`，`save_repository.rs:694-740`：精确物理路径 → `root_type` + 相对路径 → 尾段路径匹配），打包侧却退化成「按 `root_type` 取第一个」。

**后果**：`Documents\StudioA\GameX` 与 `Documents\StudioB\GameY` 两个 scope 都是 `documents` 时，如果条目本属 B 而 A 排在前面，读到的就是 `Documents\StudioA\GameX\<相对路径>` —— 一个不相干的文件。配合 V3 的无校验兜底，这个错文件会被静默打进包并上传。

**修法**：复用 `find_scope_for_entry` 的消歧逻辑（它对 entry 的 `root_path` 已有完整处理），或至少先按 `root_path` 精确匹配、失败再退回 `root_type`。

**修复记录（2026-09-10）**

消歧的核心抽成 `pick_scope_by_root_path(candidates, entry)`（`repositories/save_repository.rs`）：候选唯一时直接返回，多个时**先按 `root_path` 精确匹配、再按路径尾段匹配**，都无法唯一确定就返回 `None`（调用方报错，绝不猜一个）。

- **恢复侧** `find_scope_for_entry` 原来的 tier-3 内联消歧改为调用它 —— 顺带修好一个此前会误报的角落：两个范围尾段同名（如 `…\StudioA\GameX` 与 `…\StudioB\GameX`）而条目 `root_path` 精确指向其一，旧代码只看尾段 → 判为歧义报错；现在先做精确匹配，能正确解析。
- **打包侧** 新增 `find_scope_for_read(profile, entry)`，`read_file_from_scope` 改用它，不再 `find(|s| s.root_type == entry.root_type)` 取第一个。

`find_scope_for_read` 与恢复侧的 `find_scope_for_entry` **有意不同**：它**不**要求条目仍落在当前启用、未被排除的范围内 —— 用户收窄存档范围或改动存档目录后，旧版本里落在范围外的文件本应仍可读取并上传（它们就来自这台机器）。因此它只按 `root_type` 取候选、再用 `root_path` 消歧；候选唯一时行为与旧代码完全一致（无回归），只有「多个同类型范围且无法消歧」才报错 —— 而这恰好是旧代码会静默读错文件的情形。

**回归测试**：新增 5 条（`save_repository.rs` 3 条 + `cloud_save_service.rs` 2 条）—— 同类型多范围时按 `root_path` 选中正确范围；条目缺 `root_path` 时必须报错而非猜一个；范围收窄后打包侧仍可读、恢复侧仍拒绝（用一对断言把这个**有意的不一致**钉住）；对象缺失时回退内容漂移必须报错；哈希比较忽略大小写。`cargo test --lib` **188 passed / 0 failed**（183 → 188）；`cargo fmt --check` 干净；`cargo clippy` 保持 **lib 30 / lib test 34**，新增代码零告警。

**变异验证**：把两处修复分别改回旧行为后重跑 —— ① 去掉回退前的 `ensure_bytes_match_hash` 调用 → `resolve_entry_bytes_verifies_the_fallback_against_the_recorded_hash` 失败；② 把 `find_scope_for_read` 改回「取第一个候选」→ `find_scope_for_read_picks_the_scope_matching_the_entry_root_path` 与 `…refuses_to_guess_between_same_type_scopes` 精确失败。**恰好这 3 条失败，其余全绿**，说明测试确实钉在修复点上。

> **覆盖度说明**：与 V1 同样的限制 —— 单测无法构造 `AppHandle`，所以 `read_entry_bytes` 里「真的去调 `read_object`／真的去读盘」这一步没有端到端覆盖，覆盖的是它背后可注入的决策函数 `resolve_entry_bytes`；`read_file_from_scope` 的 scope 选择则通过 `find_scope_for_read` 被完全覆盖（纯函数，不依赖 `AppHandle`）。

### V5 改保留数时 `latest_save_version_id` 会悬垂 —— 中 ✅ 已修复（2026-09-10）

`update_save_profile_keep_versions`（`save_commands.rs:686-740`）剪枝旧版本时**只 retain，不重算** `game.latest_save_version_id`：

```rust
candidate.save_versions.retain(|v| !(v.game_uid == game_uid && to_remove.contains(&v.version_id)));  // :731-733
let _ = SaveRepository::collect_garbage(&app, &candidate.save_versions);                              // :734
GameRepository::persist(&app, &candidate)?;                                                            // :737
```

对照 `delete_versions_task`（`save_version_commands.rs:353-368`）——那里是**显式重算**的：

```rust
let latest = candidate.save_versions.iter()
    .filter(|v| v.game_uid == game_uid)
    .max_by(|l, r| l.created_at.cmp(&r.created_at).then(l.version_id.cmp(&r.version_id)))
    .map(|v| v.version_id.clone());
game.latest_save_version_id = latest;
```

**具体触发序列**（两步，都走 UI）：

1. 用户在版本时间线里点「恢复」某个**旧**版本 → `latest_save_version_id` 被指向那个旧版本（`save_version_commands.rs:313`；云端还原路径同理会写 `cloud_save_service.rs:384`）。
   注意剪枝排序用的是 `created_at`，被恢复的旧版本 `created_at` 最小，排在最末。
2. 用户把「保留」下拉改成 1（`GameDetailPage.vue:1224` → `changeKeepVersions`）→ 剪枝恰好把刚恢复的那个版本删掉，而 `latest_save_version_id` 仍指着它。

**后果**：`load_version_context`（`save_version_commands.rs:427-436`）解析出的 `latest` 为 `None` → `commit(app, game, profile, latest=None, ...)` 失去「与最新版本比对」的能力 → **此后每次游戏退出都会无条件新建一个版本**，即使存档一个字节都没变。这直接违反设计文档的「没有变化时不创建新版本」（`docs/gamesaver-new-architecture.md:317`），并加速版本膨胀与后续回收。

`cloud_save_service.rs:696-713` 的 `protect_current_save_version` 也依赖同一个指针来取 `latest`，同样受影响。

**修法**：把 `delete_versions_task` 里那段重算抽成共用函数，在 `update_save_profile_keep_versions` 剪枝后一并调用。

**修复记录（2026-09-10）**

把「指针必须能解析」做成 `AppStore` 自己的数据不变量，而不是在每个调用点各写一遍：

| 位置 | 改动 |
| --- | --- |
| `domain/store.rs` | 新增 `AppStore::repair_latest_save_version_id`（附私有 `newest_save_version_id`） |
| `repositories/save_repository.rs` | `prune_game_save_versions` 在 retain 之后调用修复 —— 唯一的剪枝入口，自此调用方不必记得修 |
| `commands/save_version_commands.rs` | `delete_versions_task` 的内联「重算为最新一版」改为调用同一方法 |
| `domain/store.rs` | `normalize` 在裁掉无效版本记录后顺带修一遍，坏状态不会被载入运行期 |

**规则是「修悬垂、不夺权」，而不是报告原修法暗示的「无条件改算成最新一版」**：这个指针是 `commit` 判断「存档有没有变化」的比对基线（`save_repository.rs` 用它做逐文件哈希复用与缺失文件的墓碑判定），而**恢复旧版本之后它会有意停在那个旧版本上** —— 此时本地内容就等于旧版本，下一次退出理当比对出「无变化」。无条件改写会让这次比对必然发现差异，白多出一个版本，恰好又退回本条要治的「版本库无意义膨胀」。所以只有当指针为空、或指向的版本已不存在时才回退到 `created_at` 最新的一版。

**一处顺序坑（第一版就踩了）**：修复最初写在 retain **之前**。可 retain 恰恰可能删掉指针指向的那一版 —— 那正是本条的触发序列，等于没修。现改为 retain 之后统一收尾；并且**未发生剪枝时也执行修复**，让旧数据里已经存在的悬垂指针能借下一次剪枝自愈。

**回归测试**：`domain/store.rs` 新增 5 条（仍能解析的旧版本指针不被改写 / 悬垂时回退到 `created_at` 最大者（列表乱序）/ 一版不剩时清空 / 不误改其他游戏 / `normalize` 裁掉版本后修复），`save_repository.rs` 新增 1 条（剪枝删掉指针指向的版本后当场修复）。`cargo test --lib` **194 passed / 0 failed**；`cargo fmt --check` 干净；`cargo clippy` 保持 **lib 30 / lib test 34**。

**变异验证**：① 去掉「仍能解析则保留」的早返回（退化成无条件改写）→ 只有 `repair_keeps_a_pointer_that_still_resolves_even_when_it_is_not_the_newest` 失败；② 把 `prune_game_save_versions` 里的修复改成「仅当未剪枝时执行」→ 只有 `prune_repairs_a_latest_pointer_it_orphans` 失败。**两处各只打掉对应那一条，其余全绿**，还原后已复跑确认干净。

### V6 解压前不校验大小（P2-2 在存档主路径上的确认） —— 中 ✅ 已修复（2026-09-10）

已登记在 `docs/code-review-2026-09-10.md` 的 P2-2，这里确认它落在**存档导入的主路径**上，而不只是理论边界。

`download_and_import_save_version`（`cloud_save_service.rs:304-326`）：

```rust
let mut data = Vec::new();
item.read_to_end(&mut data).map_err(...)?;      // :317-319  先整项读进内存
let hash = sha256_hex(&data);
if data.len() as u64 != *expected_size || hash != *expected_hash {   // :321  后校验大小
    return Err(...);
}
```

`expected_entries` 在循环之前就已构建完成（`:297`），`relative → (hash, size)` 里带着期望大小；zip 条目头部本身也带 `item.size()`。**读取之前就能拦截**，不必先分配内存。

**修复记录（2026-09-10）**：抽出 `read_zip_entry_bounded(item, declared_size, expected_size, max_size, label)`，读取前先比大小、读取量再用 `Read::take` 钉死。三处要点：

- **两道与读取量有关的闸门，缺一不可。** ①`declared_size`（zip 头部）与 `expected_size`（版本清单）不一致就拒绝 —— 拦经典解压炸弹，**一个字节都不读**；②`declared_size > max_size` 是**与清单无关**的硬上限，因为 `declared_size` 本身也来自压缩包。③实际读取用 `take(declared_size + 1)`：即便头部谎报，读取量也不会超过声明值（多读的那 1 字节专门用来判定「超出」，随即报错），而不是「读完再发现超了」。
- **为什么还需要与清单无关的硬上限**：`expected_size` 来自压缩包内的 `meta.json`。而远端清单在「由网盘文件列表重建」这条路径上 `package_sha256` 是 `None`（`cloud_save_service.rs:450`），此时第 265 行的哈希校验被跳过、`meta.json` **没有可信锚点** —— 只有硬上限能挡住伪造的巨物。上限取 1 GiB（正常收集侧默认 `max_file_bytes = 10 MiB`，两者相差百倍），`meta.json` 单独取 32 MiB。
- **同一个缺陷也修在 `meta.json` 上**：它原本同样是裸 `read_to_end`，而它自己的大小同样由压缩包声明。

回归测试 5 条（`cloud_save_service.rs`）：大小不一致拒绝且 `read_calls == 0`（证明校验发生在读取之前）、超硬上限拒绝且 `read_calls == 0`、**头部谎报时读取量被截断**（断言实际读取位置 ≤ 声明值 + 1）、正常读取返回精确字节、`meta.json` 路径只受上限约束。

**变异验证（三次，失败集合互不重叠）**：① 撤掉「与清单比对大小」→ 恰好第一条失败；② 撤掉硬上限 → 恰好第二条失败；③ 撤掉 `take` → 恰好「头部谎报」那条失败，且报错正是「读取量必须被截断在声明大小附近，实际读了 6 字节」（首次写测试时只断言了错误类型，变异验证证明那样**拦不住**第 ③ 条，遂补上读取量断言）。

### V7 恢复产物残留在用户存档目录里，且会被下一次提交当成存档收进版本 —— 低 ✅ 已修复（2026-09-10）

`restore_group` 把两个工作目录建在**用户真实存档根目录内**（`save_repository.rs:445-447`）：

```rust
let staging  = group.root.join(format!(".gamesaver-restore-{restore_id}"));
let rollback = group.root.join(format!(".gamesaver-rollback-{restore_id}"));
```

它们的清理只发生在 `cleanup_restore_artifacts` / `finalize_restore` / `rollback_restore` 三条正常路径上。全项目 grep 确认：**没有任何启动清理**（`.gamesaver-restore-*` / `.gamesaver-rollback-*` 只在上面两行出现）。

**后果**（恢复过程中进程被强杀 / 断电）：

1. `.gamesaver-restore-<id>\` 留在存档目录里，里面是目标版本的完整副本；`.gamesaver-rollback-<id>\` 里是用户恢复前的存档。
2. 下一次游戏退出时 `collect_profile_files`（`save_repository.rs:859-894`）会 `WalkDir` 扫描 `include_directories`，**把这些残留目录当成正常存档文件**收进新版本。它们不匹配任何 `exclude_patterns`（默认只有 `*.tmp` / `*.log` 等），也不是被排除的目录名。
3. 于是垃圾进了本地版本、随后被上传到云端；将来恢复该版本时，这些 `.gamesaver-restore-*` 路径会被**重新写回**存档目录，并且每崩一次多一份。

这与项目自身的标准（`store_file` 的 `.tmp-*` / `.bak-*` 崩溃窗口兜底）不一致 —— 存档侧同样存在崩溃窗口，却没有兜底。

**修法**（按优先级）：

1. 最低限度：在收集阶段拒绝 `.gamesaver-restore-` / `.gamesaver-rollback-` 前缀，保证垃圾不会进版本；
2. 更完整：启动时扫描各 scope 根目录，发现残留的 rollback 目录则执行一次回滚恢复（这正是「恢复中途崩溃」应有的语义），再清理残留。

**修复记录（2026-09-10）** —— 实做时对上面第 1、2 条各有一处修正：

- **第 1 条不能挂在 `is_excluded` 上**。`is_excluded` 除收集之外，还被 `scope_matches_entry_exact` / `scope_matches_entry_loose` 用来判「条目属于哪个范围」。挂上去会让**修复前就已经把产物收进去的历史版本**在恢复时找不到范围，整场报「保存版本中的文件不属于当前存档范围」—— 用新 bug 换旧 bug，正是 V2 那个失败模式，只是换了个诱因（已用变异验证：撤掉 `build_restore_groups` 里的跳过，该测试就精确复现了这条报错）。因此改为独立的 `path_is_restore_artifact`，只接在两个真正的入口：`add_candidate`（收进版本的唯一漏斗）与 `is_protected_file`（决定「恢复时先把哪些文件搬走」，不接的话残留只是换个位置呆着）。
- **匹配写成「前缀 + 32 位十六进制」，不是裸前缀**。用户真有一个叫 `.gamesaver-restore-notes.txt` 的存档时，裸前缀会把它一起当垃圾排除，等于替用户丢文件。判据取**任一路径分量**而非首段：范围根可以嵌套，内层范围的产物在外层看来是二级目录。
- **第 2 条的关键是「先归位、再清理」，不能写成「发现残留就删」**。崩溃若落在「把现有存档搬进 rollback」之后，用户存档目录里那些位置是**空的**，真身躺在 rollback 里 —— 直接删就是把用户的存档删了。（三个调用点都在 `restore()` 之前先 `commit()` 保护了当前存档，所以严格说不算永久丢数据，但用户得自己意识到并手动恢复那一版，等于把崩溃窗口转嫁给他。）实现上归位**全部成功才返回 `Ok`**，只要有一个文件没归位就保留回滚副本不删，下次启动继续重试 —— 宁可留着垃圾，也不能弄丢用户自己写的文件。
- **归位语义在第 ④ 段（文件装好、只差 finalize）会「放弃这次恢复」**，这是有意的且自洽：`latest_save_version_id` 是 `commit` 的比对基线而非「最新一版」（见 V5），本地内容与它不一致只会让下一次 `commit` 记出一个新版本。
- **安全闸门**：只处理范围根的**直接子目录**且名字严格匹配；`symlink_metadata` 确认是**真目录**且不是重解析点 —— `FileType::is_symlink` 在 Windows 上是否覆盖目录联接依赖 std 实现细节，所以额外查一次 `FILE_ATTRIBUTE_REPARSE_POINT`，两道都过才敢动 `remove_dir_all`（目录联接能指向别处，顺着删就删到范围外了）；归位路径一律过 `safe_join`。失败只写日志、不阻断启动，与 `GameBodyUpdateService::recover_pending_updates` 一致。
- **不动历史数据**：已经在库里的「被污染版本」不做迁移，只在读取侧「不再采集 + 恢复时不回写」。无法判断当初那些文件是否真是残留，改库的风险大于收益，让它随剪枝自然淘汰。
- **接在 `lib.rs` setup 里 `recover_pending_updates` 附近**，靠 `store.save_profiles[].scopes[].root_path` 拿到全部范围根（去重、跳过不存在的），只做**顶层** `read_dir` 不递归。

回归测试（`save_repository.rs` 新增 7 条）：产物名必须带 32 位十六进制 id（含嵌套分量、以及「同名用户文件不得被误判」）；`collect_profile_files` 不收产物（借助 `new_manual` 默认的 `include_directories = ["."]` 让递归扫描真正扫到 staging）；只剩 staging 时直接删且用户存档不动；**有 rollback 时先归位再清理**（含嵌套路径与「半成品被原档覆盖」）；**归位失败时回滚副本必须保留**（把归位目标做成同名目录制造失败）；历史污染版本恢复时跳过产物条目。

**变异验证（三次，失败集合互不重叠）**：① 撤掉 `add_candidate` 的剔除 → 恰好收集测试失败，收集结果多出 `.gamesaver-restore-<id>/slot1.sav`；② 让清扫跳过归位直接删 → 恰好「先归位」与「归位失败要保留」两条失败，而「只删 staging」「同名不误删」仍通过；③ 撤掉 `build_restore_groups` 的跳过 → 恰好历史污染那条失败，且报错正是「保存版本中的文件不属于当前存档范围」。三条各守一角，说明「不进版本」「归位优先且失败即保留」「历史版本不炸」都被独立钉住了。

---

## 3. 低优先级 / 整洁性

### V8 版本排序按 `created_at` 字符串比较 —— 低

以下 5 处都用字符串比较决定版本新旧（进而决定谁被保留、谁被回收）：

| 位置 | 用途 |
| --- | --- |
| `commands/save_commands.rs:720` | 改保留数时选待删版本 |
| `commands/save_version_commands.rs:245` | 恢复前保护时剪枝 |
| `commands/save_version_commands.rs:475` | 清理命令选待删版本 |
| `services/launch_service.rs:399` | 退出提交时剪枝 |
| `services/cloud_save_service.rs:214` | 云端清单排序 |

目前 `now_iso()`（`save_repository.rs:1142-1147`、`save_commands.rs:858-863` 等多份拷贝）实际返回的是 **10 位 unix 秒字符串**，等长所以字符串比较碰巧等于数值比较。

**但这很脆**：函数名叫 `now_iso`、字段叫 `created_at`、设计文档也按时间语义描述它。一旦有人把它改成真正的 ISO 8601（或带毫秒、或位数变化），字符串比较会与时间顺序脱钩 —— 而这些代码正是用来决定**删除哪个版本**的，删错方向就是「删掉最新的、留下最旧的」。

**修法**：要么明确落成定长数字串并加注释锁死（含跨版本兼容说明），要么解析成 `u64` 再比较。

### V9 `wildcard_matches` 不支持中间通配符，且静默失效 —— 低

`save_repository.rs:1017-1030`：

```rust
if pattern == "*" { return true; }
if let Some(suffix) = pattern.strip_prefix("*") { return value.ends_with(suffix); }
if let Some(prefix) = pattern.strip_suffix("*") { return value.starts_with(prefix); }
value == pattern
```

只支持「全匹配 / `*后缀` / `前缀*`」三种形态。`slot*.sav` 这类**中间通配**会直接落到 `value == pattern` 分支 —— 与字面量 `"slot*.sav"` 比较，**永远不匹配**。

默认排除项（`domain/save_profile.rs:51-53`）全是 `*.ext` 形态，所以现状没暴露。但存档规则编辑器允许用户自定义 pattern，用户写下 `slot*.sav` 会以为排除了、实际一个都没排除 —— 而排除项失效的方向对存档保护是「多备份」而非「少备份」，所以不会丢数据，只会让版本比预期大。

**修法**：要么实现完整的通配匹配，要么在保存规则时对不支持的形态给出明确错误，别让它静默失效。

### V10 恢复会删除「当前存在但不属于目标版本」的受保护文件 —— 低

`restore_group`（`save_repository.rs:448-508`）：

```rust
let touched = group.protected_paths.union(&target_paths).cloned().collect();   // :453-457
// 1. 把 touched 中所有当前存在的路径 rename 进 rollback 目录                    // :483-495
// 2. 只把 target_paths 里的条目装回去                                          // :496-508
// 3. finalize_restore 删掉整个 rollback 目录                                    // :610-613
```

即：**恢复 = 让受保护范围精确回到该版本的状态**，当前多出来的受保护文件会被删除（而不是保留）。

这个语义本身可以接受 —— 恢复是「回到快照」而不是「合并」。问题在于**它没有被说明**：前端确认文案是「恢复前会先保护当前存档，确定恢复这个版本吗？」（`GameDetailPage.vue:575`），既没提「当前多出的存档文件会被删除」，恢复完成后 `restore` 返回的 `RestoreReceipt` 也没把删除清单暴露到任务摘要里（`save_version_commands.rs:325` 只回 `versionId` 与 `createdAt`）。

用户的真实场景：游戏有 3 个存档槽，恢复到只有 2 个槽的版本 → 第 3 个槽被静默删除。虽然它在 rollback 目录里存在过，但 `finalize_restore` 会把它一并删掉。

**修法**：确认文案里讲清楚该版本包含哪些存档、会移除哪些当前文件；或在任务摘要里报告删除数量。

### 其它整洁性问题

- **`latest_save_version_id` 的设置方式脆弱**：`game_body_commands.rs:958-964` 用 `candidate.save_versions.last().filter(|v| v.game_uid == ...)` 来取「刚刚保护的那个版本」。`save_versions` 是跨游戏的全局数组，`.last()` + uid 过滤拿到的是「该游戏最后被追加的一条」，而不是「最新的那条」。当前因为追加顺序 = 创建顺序所以等价，但它表达的不是作者的意图，改法应是直接用被 push 的那个 `protected.version_id`（该函数里 `protected` 变量就在作用域内，只是被 move 了）。
- **`now_iso()` 有 4 份拷贝**：`save_repository.rs:1142`、`save_commands.rs:858`、`cloud_save_service.rs:780`、`launch_service.rs`，实现完全一致。这已在代码审查报告 P3「重复实现」中登记，此处只是补充存档侧的相关位置。
- **`read_file_from_scope` 缺少对 `entry.relative_path` 的越界校验**：直接把 `relative_path` join 到 root 上（`:747`），没有走 `validate_relative` / `safe_join`。目前 `relative_path` 由 `collect_profile_files` 产生（已经过 `strip_prefix(root)` 保证在 root 内），所以现状安全；但它是全项目唯一一处不经 `safe_join` 就拼存档路径的地方，与 `save_repository.rs` 里其他所有写入路径的处理方式不一致，值得统一。

---

## 4. 检查中确认「做对了」的部分

避免只报问题，以下几条经核查是正确的：

- **锁序无环，不存在死锁**。`commit` / `restore` / `collect_garbage` 只持有 `REPOSITORY_LOCK`（GC 另加 `PENDING_OBJECTS`），全程不碰 `store` 锁；而三处 GC 调用点都是「先持 `store` 锁 → 再取 `REPOSITORY_LOCK`」。全项目没有「持 `REPOSITORY_LOCK` 再去取 `store` 锁」的路径，因此不会交叉持锁。V1 的修法（把 GC 移到锁外）也继续保持这个方向。
- **待提交对象的保护窗口配对完整**。`protect_pending_objects` 只在 `commit` 成功产出版本时登记（`save_repository.rs:120`），四处 `commit` 调用点的 `release_pending_objects` 都逐一核对过：`save_version_commands.rs:274-283`、`launch_service.rs:441-450`、`cloud_save_service.rs:719-733` 都在成功与失败两条路径上释放；`game_body_commands.rs` 的 6 处释放覆盖了取消、写 journal 失败、swap 失败、persist 失败与正常收尾（该函数用原始 store 锁 + `rollback_update` 辅助函数，结构比其它三处复杂，但释放点齐全）。
- **CAS 写入是幂等且拒绝覆盖的**。`write_object_locked`（`save_repository.rs:286-315`）在目标已存在时比对长度，长度不同直接报错而**不覆盖** —— 同一哈希对应同一内容，这个判断是可靠的；临时文件 + `sync_all` + rename 的写法也正确。
- **恢复的对象在物化前逐个校验**。`restore_group:462-471` 对每个对象先 `sha256_file(&object) != hash` 再复制，不是「先拷后验」。
- **恢复失败会整体回滚**。`restore`（`:151-174`）在某一组失败时，对已完成的组逆序 `rollback_group`，并把回滚失败信息追加进错误（`append_rollback_errors`），不会留下「一半新一半旧」的静默状态。
- **`deleted` 条目的归属判定是必要的**。`entry_belongs_to_profile`（`:983-994`）在提交时用当前 profile 判断旧条目是否还该保留为 tombstone —— 范围外的旧条目被丢弃而不是记成删除，方向正确（否则改窄范围会导致下次提交误删用户文件）。
- **恢复前先保护当前存档的顺序正确**。本地恢复（`save_version_commands.rs:221-230`）、云端还原（`cloud_save_service.rs:366-371`）、本体更新（`game_body_commands.rs:815-834` 在 swap 之前）三条路径都是「先 commit 当前存档 → 再覆盖」，符合设计文档「不得无提示覆盖本地存档」。

---

## 5. 建议处理顺序

| 顺序 | 事项 | 理由 |
| --- | --- | --- |
| 1 | **V1** 三处 GC 移到持久化之后 | ✅ 已完成（2026-09-10）：改动小（照抄 `delete_versions_task` 已有形状），消除「静默不可恢复版本」，顺带解掉锁内全盘扫描 |
| 2 | **V3 + V4** 打包路径补哈希校验、修 scope 选择 | ✅ 已完成（2026-09-10）：两者叠加才会产生「错内容被静默上传」，是云端备份可信度的地基 |
| 3 | **V5** 剪枝后重算 `latest_save_version_id` | ✅ 已完成（2026-09-10）：做成 `AppStore` 的数据不变量并嵌在唯一的剪枝入口上，顺带覆盖 `delete_versions_task` 与载入时的 `normalize` |
| 4 | **V2** 墓碑条目跳过 root/scope 前置校验 | ✅ 已完成（2026-09-10）：影响「恢复到底能不能用」，且报错信息误导用户；保留墓碑「让该范围参与清理」的职责，避免「恢复到空」被挡掉 |
| 5 | **V7** 残留产物排除 + 启动兜底 | ✅ 已完成（2026-09-10）：两道防线 —— 收集侧切断「污染→上传云端」的链条，启动清扫按盘上痕迹「先归位、再清理」。实做时修正了报告原方案的两处：排除**不挂** `is_excluded`（会让历史污染版本不可恢复），清扫**不能**写成「发现残留就删」（备份阶段崩溃时 rollback 里是用户自己的存档） |
| 6 | **V6** 解压前拦大小 | ✅ 已完成（2026-09-10）：`read_zip_entry_bounded` 把「校验」挪到「读取」之前，并用 `take` 钉死读取量；顺带修掉 `meta.json` 上的同一缺陷 |
| 7 | V8 / V9 / V10 | 整洁性与文案：`created_at` 排序鲁棒性、`wildcard_matches` 静默失效、恢复删除不告知 |

---

## 附：本次审查使用的验证手段

```bash
# 提交 / 回收 / 释放三者的全部调用点（用于核对配对与写序）
grep -rn "SaveRepository::commit" src-tauri/src
grep -rn "release_pending_objects" src-tauri/src
grep -rn "collect_garbage" src-tauri/src

# latest_save_version_id 的全部读写点（用于找悬垂引用）
grep -rn "latest_save_version_id" src-tauri/src

# 确认恢复产物没有启动清理（仅两处出现，均为构造处）
grep -rn "gamesaver-restore\|gamesaver-rollback" src-tauri/src

# 确认默认排除项形态（全部为 *.ext，未暴露 V9 的中间通配问题）
grep -rn "DEFAULT_EXCLUDE_PATTERNS" -A 3 src-tauri/src/domain/save_profile.rs

# 确认 with_store_mut 的失败语义与持久化的原子性
#   app_state.rs:77-89 + 测试 with_store_mut_discards_the_mutation_on_error
#   game_repository.rs:57-78（atomic_replace）
```

> 说明：本报告基于静态代码阅读 + 上述实际执行的验证命令。所有行号来自当前工作区（含未提交改动）的文件快照。V5 / V7 / V8 / V10 涉及运行时序列，判断依据已在正文逐条写明触发条件；V1 / V2 / V3 / V4 / V6 / V9 为纯静态可判。
