# 存档识别误报审计（2026-09-15）

**触发**：给《Ancient Cultivatrix》建档时，审阅阶段（「确认存档保护范围」）出现 3 个候选范围，
其中只有第 3 个是正确的。问题：**能从这次误报里学到什么识别提升方法？**

**结论（先说结果）**：两个误报各有**一条独立的、定位到行的**成因，都不是"启发式不够聪明"，
而是两处判定**过于宽松**：

| # | 候选范围 | 判定 | 成因 |
|---|---|---|---|
| ① | `C:\Users\eternity\AppData\LocalLow\5 shrimps\Ancient Cultivatrix` | **误报** | 扩展名白名单单独放行（`db`/`sqlite`/`vdf`） |
| ② | `C:\Users\Public\Documents\Steam\RUNE\1903340` | **误报** | 模拟器 appid 目录被当成「命名存档容器」，目录内**任何**文件都成候选 |
| ③ | `<游戏根目录>\Ancient Cultivatrix_Data\save` | 正确 | — |

本次会话：`game_uid=564f9729-8f6f-4f01-9bd9-c6b6f461b597`。

---

## 1. 证据链

### 1.1 日志（`%APPDATA%\com.gamesaver.next\logs\gamesaver.log`，逐字）

```
892  开始只读推断存档初稿：game_uid=564f9729-8f6f-4f01-9bd9-c6b6f461b597
893  只读推断完成：game_uid=564f9729-… 候选范围=1 置信度=65
894  存档学习扫描范围：game_uid=564f9729-… roots=1 paths=E:\GameSaverGames\games\564f9729-…
895  存档学习容器兜底：未找到常见存档容器
896  [ERROR] 取消存档识别时停止 ETW 失败：…（与本议题无关的既有错误）
897  开始确认存档保护：game_uid=564f9729-… scopes=1 capture_mode=Some("etw")
898  存档保护范围校验完成：game_uid=564f9729-… scopes=1
```

**关键**：只读预览只给出 **1** 个候选，学习阶段的扫描根也只有 **1** 个（托管本体）；
而审阅界面显示 **3** 个 ⇒ **多出来的两个是 ETW 阶段追加的**，不是预览产生的。
（预览那 1 个候选的具体路径日志未记录；预览按机制只扫托管本体，因此看不到 ①②。）

### 1.2 磁盘现状（2026-09-15 00:10~00:13 写入）

| 范围 | 文件 | 大小 |
|---|---|---|
| ① LocalLow | `archiveList.sqlite` | 12,288 B |
| ① LocalLow | `b0476da758b34407915359dc20f8050a.sqlite` | 69,632 B |
| ① LocalLow | `Player.log` | 3,121 B（**已被 `\player.log` 规则正确排除**） |
| ② Rune | `achievements.ini` | **32 B** |
| ② Rune | `leaderboards.ini` | **32 B** |
| ③ 正确 | `_settingData.json` | 347 B |

①的两个库是「资源归档索引」与「安装级配置库」，启动时重写；②的两个 32 字节 ini 是
**RUNE（Steam 模拟器）的成就/排行榜状态**，由注入游戏进程的 DLL 写入 —— 所以 ETW 看到的
"游戏进程写了它" 对 ①②③ **全部成立**，写入证据本身无法区分。

### 1.3 机械复现（临时探针入 `save_learning_service.rs` 测试模块，跑完已删）

```
=== 误报 2：模拟器 appid 目录 ===
is_save_container_directory(C:\Users\Public\Documents\Steam\RUNE\1903340) = true
achievements.ini         etw_candidate=true  save_candidate=true  noise=false
leaderboards.ini         etw_candidate=true  save_candidate=true  noise=false
steam_emu.ini            etw_candidate=true  save_candidate=true  noise=false
remote\slot1.sav         etw_candidate=true  save_candidate=true  noise=false   ← 正当用例，必须保持 true

=== 误报 1：只靠扩展名通过的库/配置文件 ===
archivelist.sqlite                           etw_candidate=true  save_candidate=true
b0476da758b34407915359dc20f8050a.sqlite      etw_candidate=true  save_candidate=true
settings.db                                  etw_candidate=true  save_candidate=true
telemetry.vdf                                etw_candidate=true  save_candidate=true
save.db                                      etw_candidate=true  save_candidate=true   ← 正当用例，必须保持 true
```

探针已删除，测试数回到基线（320 passed / 2 ignored）。

---

## 2. 成因（定位到行）

### 成因 A：`db` / `sqlite` / `vdf` 与 `sav` 同级，扩展名可单独定案

`src-tauri/src/domain/save_candidate.rs:16-18`

```rust
pub(crate) const SAVE_EXTENSIONS: [&str; 10] = [
    "sav", "save", "db", "sqlite", "es3", "sl2", "ess", "savegame", "state", "vdf",
];
```

`is_save_candidate`（`:223-275`）的收尾是：

```rust
has_save_container || has_save_path_hint || has_file_hint
    || SAVE_EXTENSIONS.contains(&ext_lower.as_str())     // ← 最后一条单独为真即通过
```

`archiveList.sqlite` / `<uuid>.sqlite` 的**文件名**不含任何线索（`NAME_HINTS` 里没有），
路径里没有 `savedata`/`profiles`/… 段，父目录 `Ancient Cultivatrix` 也不是命名容器
⇒ 三者全假，**完全靠扩展名进来**。

`db`/`sqlite` 恰恰是**缓存、索引、遥测、设置**最常用的格式（`vdf` 更是 Steam 自己的配置格式）；
把 `sav`/`save`/`es3` 这类"专属存档扩展名"和它们并列放在同一条无条件白名单里，是这次 ① 的根因。

### 成因 B：模拟器 appid 目录 = 命名容器 ⇒ 目录内一切皆候选

`src-tauri/src/domain/save_candidate.rs:161-170`

```rust
let norm = normalize_path(path);
if norm.contains(r"\steam\") || norm.contains(r"\rune\")
    || norm.contains(r"\codex\") || norm.contains(r"\onlinefix\")
{
    if name.chars().all(|c| c.is_ascii_digit()) && name.len() >= 2 {
        return true;            // ← 整个 <模拟器>\<appid> 目录都成了"存档容器"
    }
}
```

于是 `path_has_save_container_ancestor` 对 `…\RUNE\1903340\achievements.ini` 返回真
⇒ `has_save_container = true` ⇒ 该文件无条件成为候选。

这条规则**本意是对的**：模拟器把存档放在 `<模拟器>\<appid>\remote\…`，必须能认出来。
现有测试只钉住了 Steam 原生形态（`…\Steam\userdata\123456\2456740`，见
`save_learning_service.rs:3617`），**`\rune\`/`\codex\`/`\onlinefix\` 三个分支没有任何测试** ——
而它们的粒度错误：模拟器的 appid 目录里既有真存档（`remote/`），也有模拟器自身状态
（`achievements.ini`、`leaderboards.ini`、`steam_emu.ini`）。

### 附带成因 C：证据分级对"文件质量"不敏感 ⇒ 三个范围都显示「高可信」

`src-tauri/src/services/save_learning_service.rs:1832-1838`

```rust
if (has_write && (has_close || has_rename)) || has_rename {
    return (SaveCandidateEvidenceLevel::Strong,
            "ETW 已确认写入并完成关闭或重命名。".to_string(),
            if has_rename { 92 } else { 88 });
}
```

只要范围内**任意**文件发生「写入 + 关闭/重命名」就是 Strong，不看文件数、大小、
是否落在命名容器、名字是否有线索 ⇒ **32 字节的模拟器配置也拿"高可信"**，
用户在审阅界面没有可用的取舍线索（三张卡片全一样）。

更糟的是 `calculate_learning_confidence`（`:1868-1871`）：

```rust
let has_named_save_container = scope_drafts.iter()
    .any(|draft| is_save_container_directory(Path::new(&draft.scope.root_path)));
let container_bonus = if has_named_save_container { 5 } else { 0 };
```

`…\RUNE\1903340` 命中了容器判定 ⇒ **误报反而给整个学习结果 +5 分**（截图里的 95/100
就有这 5 分）。这是评分方向上的错误激励。

---

## 3. 提升方法（按性价比排序）

### 方法 1：把「库/配置类扩展名」从无条件白名单里摘出来（治 A）

- 拆成两组：专属存档扩展名（`sav`/`save`/`es3`/`sl2`/`ess`/`savegame`/`state`）保持无条件通过；
  通用库/配置扩展名（`db`/`sqlite`/`vdf`）**要求额外条件**——文件名命中 `NAME_HINTS`，
  或落在命名容器 / 存档语义路径段下。
- 效果：本次 ① 的两个库被拒；`save.db`、`profile.sqlite` 仍然通过（复现表已覆盖这两个边界）。
- 代价：极少数"文件名毫无线索的 `.sqlite` 真存档"会漏 —— 但漏收比误收安全（漏了用户可手动加目录，
  误收会把索引库当存档备份、恢复时覆盖配置）；且 ETW 证据仍在事务摘要里可见。
- 影响面：`is_save_candidate` 是**收集侧与恢复侧共用的同一道门**
  （`repositories/save_repository.rs:1428-1447` 的 `scope_admits_directory_file`，
  被 `collect_profile_files` 与 `collect_protected_paths` 共用），
  收紧语义不会造成"删除用户没备份过的文件"，是安全方向。

### 方法 2：模拟器状态文件进噪音名单，或收紧模拟器分支粒度（治 B）

两个可选做法，**建议同时做**：

1. 在 `should_ignore_event_path`（`save_candidate.rs:43-121`，它已经是"文件名级后缀 + 路径片段"表）
   加：`achievements.ini`、`leaderboards.ini`、`steam_emu.ini`、`steam_appid.txt`、
   `\steam_settings\`。这一处判定**同时被 ETW 解析与快照扫描使用**，两侧口径天然一致。
2. 把模拟器分支从"目录名是纯数字"收紧为"目录名是纯数字 **且** 其下存在 `remote`/`save*`/`profile*`
   子目录"——即只有真的住着存档的 appid 目录才算容器。

- 回归护栏：必须补上 `remote\slot1.sav`（模拟器真存档）仍为 true 的用例。

### 方法 3：证据分级纳入「文件质量」（治 C，体验层）

- 范围内只有 1~2 个文件、且都在 1 KB 以下、且无命名容器时，降级为 `Review`，
  文案改为「疑似配置/索引文件，请确认是否属于存档」。
- 这样 ② 那张卡片在界面上就是"待确认"，用户一眼分辨；同时避免误报给整体分数加分。

### 方法 4：把这次的三个真实路径钉成回归测试（护栏，最该先做）

现有判定其实**有测试**（`save_learning_service.rs:2168-2361`、`:3611-3619`），但**没有一个用例**
覆盖"模拟器 appid 目录"和"LocalLow 里的索引库"这两类本次踩到的形态。建议把这些真实路径原样写进用例：

```
false: C:\Users\…\AppData\LocalLow\5 shrimps\Ancient Cultivatrix\archiveList.sqlite
false: C:\Users\…\AppData\LocalLow\5 shrimps\Ancient Cultivatrix\b0476da7….sqlite
false: C:\Users\Public\Documents\Steam\RUNE\1903340\achievements.ini
false: C:\Users\Public\Documents\Steam\RUNE\1903340\leaderboards.ini
true : C:\Users\Public\Documents\Steam\RUNE\1903340\remote\slot1.sav
true : <任意>\save\_settingData.json
```

### 不建议的改法（及原因）

- **不要**把 `AppData\LocalLow\*` 一律降权：大量 Unity 游戏的正经存档就在 LocalLow
  （`%USERPROFILE%\AppData\LocalLow\<厂商>\<产品>` 是 Unity 的标准数据目录）。
- **不要**按文件大小全局过滤：③ 的正确存档只有 347 字节。
- **不要**把模拟器目录整个排除：`remote/` 下就是真存档。

---

## 4. 状态

| 项 | 状态 |
|---|---|
| 成因 A（扩展名白名单过宽） | **已修复**：`db`/`sqlite`/`vdf` 从 `SAVE_EXTENSIONS` 摘出（`domain/save_candidate.rs`） |
| 成因 B（模拟器状态文件） | **已修复**：`achievements.ini`/`leaderboards.ini`/`steam_emu.ini`/`steam_appid.txt`/`steam_settings\` 进 `should_ignore_event_path` 噪音名单 |
| 成因 C（证据分级 / 容器加分） | 已定位，**未修改**（体验层，待单独决定） |
| 方法 4（回归测试） | **已落地**：`save_learning_service.rs::real_world_index_and_emulator_state_files_are_not_save_candidates` |

### 4.1 与初版建议的偏差（方法 2 的第 2 条）

初版建议"把模拟器分支收紧为『其下存在 `remote`/`save*`/`profile*` 子目录』"。
落地时**没有**采用这条：`is_save_container_directory` 的契约是"只看路径字符串、
不碰 IO，也不看文件内容"（`save_candidate.rs:9`），而"其下是否存在某子目录"必须读盘；
该函数还会被用在**尚不存在**的路径上（新建范围、恢复目标路径），读盘判定会随目录状态抖动。
实际采用的是按**文件名**把模拟器状态文件列入噪音——它精确覆盖了本次两个误报，
而 `remote`/`save*` 这类真存档子目录本来就由 `SAVE_DIRECTORY_HINTS` 认定，不受影响。

### 4.2 安全性论证（为什么收紧不会造成误删）

`is_save_candidate` 是收集侧（`collect_profile_files`）与恢复侧（`collect_protected_paths`）
共用的同一道门（`repositories/save_repository.rs:1428-1447` `scope_admits_directory_file`）。
收紧后"受保护文件"集合变小，而恢复只删除"在受保护集合里、却不在备份里"的多余文件
（`collect_protected_paths:1050-1051` 注释所指的方向）⇒ 只会少删，不会多删；
显式确认过的文件不受影响（`scope_admits_directory_file` 第一层直接放行）。

### 4.3 门禁与验证

| 门禁 | 结果 |
|---|---|
| `cargo fmt --all --check` | exit 0 |
| `cargo test --lib` | **321 passed / 0 failed / 2 ignored**（基线 320，新增 1 条回归测试） |
| `cargo clippy --lib --all-targets` | 0 error；`(lib) generated 27 warnings` / `(lib test) generated 32 warnings (27 duplicates)`，与改动前留档 `.perf/gates2.txt` **逐字一致** ⇒ 未引入新 warning |

新增的回归测试同时就是"改动生效"的机械证明：两个误报路径现在
`is_etw_candidate` / `is_save_candidate` **均为 false**；对照组
（`save.db`、`profile.sqlite`、`SaveData\blob.sqlite`、`remote\slot1.sav`、
`…\save\_settingData.json`）**仍为 true**。

**审计依据**：`logs\gamesaver.log:892-898`（本次会话）；
`domain/save_candidate.rs:16-18,43-121,145-172,190-213,223-275`（判定权威）；
`services/save_learning_service.rs:1013-1121`（ETW 文件→扫描根）、`:1801-1885`（证据分级与总置信度）、
`:3611-3619`（现有容器测试）；`repositories/save_repository.rs:1443-1446`（收集侧共用同一道门）；
磁盘实测（2026-09-15 00:10~00:13 写入的三个目录）；临时探针机械复现（已删）。
