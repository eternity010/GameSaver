# GameSaver 待开发计划：多锚点路径变量扩展

## 背景

当前规则系统已支持：

- `%USERPROFILE%`
- `%GAME_DIR%`

这已经覆盖了两类核心场景：

1. 用户目录下的常见存档
2. 游戏安装目录下的存档

但 `%USERPROFILE%` 仍然过于宽泛。  
例如下面两条规则都能工作：

- `%USERPROFILE%\Documents\My Games\GameA`
- `%USERPROFILE%\AppData\LocalLow\Studio\GameB`

问题在于它们虽然可迁移，但语义表达不够清晰，也不利于后续导入诊断、规则展示和人工校验。

目标是引入一批更具体的路径锚点变量，使规则从“能迁移”进一步提升到“更可读、更稳定、更可诊断”。

---

## 本期目标

在不引入复杂规则 DSL 的前提下，新增一批固定锚点变量：

- `%DOCUMENTS%`
- `%APPDATA%`
- `%LOCALAPPDATA%`
- `%LOCALLOW%`
- `%SAVED_GAMES%`

并建立明确的：

1. 存储归一化优先级
2. 运行时展开逻辑
3. 兼容旧规则的迁移策略
4. 前端展示语义

---

## 非目标

- 不支持用户自定义变量。
- 不支持表达式、条件路径、平台分支 DSL。
- 不在本期实现 ZIP 导入前预检向导。

---

## 设计原则

### 1) 锚点越具体，优先级越高

规则归一化时，优先用更具体、更语义化的锚点，而不是一律兜底到 `%USERPROFILE%`。

建议优先级：

1. `%GAME_DIR%`
2. `%SAVED_GAMES%`
3. `%DOCUMENTS%`
4. `%LOCALLOW%`
5. `%LOCALAPPDATA%`
6. `%APPDATA%`
7. `%USERPROFILE%`

说明：

- `%GAME_DIR%` 最特殊，依赖游戏绑定 EXE，优先级最高。
- `%USERPROFILE%` 作为最后兜底，不抢更具体的语义锚点。

### 2) 同一条路径只落一个锚点

例如：

- `C:\Users\Eternity\Documents\My Games\X`

归一化后应固定为：

- `%DOCUMENTS%\My Games\X`

而不是：

- `%USERPROFILE%\Documents\My Games\X`

### 3) 运行时展开必须可诊断

每个锚点都必须支持：

- 成功展开
- 无法展开时返回明确错误

不要出现静默回退到绝对路径的行为。

---

## 锚点定义建议

### `%DOCUMENTS%`

映射到当前用户文档目录，例如：

- `C:\Users\<User>\Documents`

### `%APPDATA%`

映射到：

- `C:\Users\<User>\AppData\Roaming`

### `%LOCALAPPDATA%`

映射到：

- `C:\Users\<User>\AppData\Local`

### `%LOCALLOW%`

映射到：

- `C:\Users\<User>\AppData\LocalLow`

注意：Windows 没有特别稳定的环境变量直接暴露 `LocalLow`，实现时需要基于用户目录拼接或查询已知目录。

### `%SAVED_GAMES%`

映射到：

- `C:\Users\<User>\Saved Games`

说明：这一类目录在“现代单机游戏”里使用频率很高，值得单独提升语义层级。

---

## 后端实现计划

## Phase 1：路径解析基础

- [ ] 新增锚点常量定义。
- [ ] 新增各锚点对应的系统路径解析函数。
- [ ] 统一抽象为“锚点表”或“候选锚点列表”，避免后续 `if/else` 链无限膨胀。

建议抽象：

- token 名称
- 解析函数
- 归一化优先级

## Phase 2：存储归一化

- [ ] 扩展 `normalize_confirmed_path_for_storage`
- [ ] 按优先级依次尝试各锚点前缀匹配
- [ ] 命中后生成 `TOKEN + 相对路径`
- [ ] 保留非法相对跳转拦截（如 `..`）

## Phase 3：运行时展开

- [ ] 扩展 `expand_confirmed_path_for_runtime`
- [ ] 支持所有新锚点
- [ ] 错误消息区分：
  - `%GAME_DIR%`：需要绑定 EXE
  - 用户目录类锚点：系统目录解析失败

## Phase 4：历史规则迁移

- [ ] `normalize_store` 中按新优先级重写旧规则
- [ ] 用户重新绑定 EXE 时，保留 `%GAME_DIR%` 即时迁移
- [ ] 对原本 `%USERPROFILE%\Documents\...` 一类旧规则进行收敛升级

---

## 前端计划

### 规则管理

- [ ] 在规则页显示路径锚点说明
- [ ] 可选展示路径标签：
  - 用户目录规则
  - 文档目录规则
  - LocalLow 规则
  - 游戏目录规则

### 游戏库 / 预检查

- [ ] 在预检查里展示“规则路径锚点类型”
- [ ] 若锚点可解析失败，给出对应原因

### 导入体验（后续）

- [ ] 导入完成后，区分：
  - 需要绑定 EXE 的 `%GAME_DIR%`
  - 可直接生效的用户目录类锚点

---

## 风险与注意事项

### 风险 A：优先级不稳定导致规则来回变化

- 应对：优先级必须写死并文档化。

### 风险 B：`LocalLow` 解析不稳定

- 应对：统一通过用户目录推导，不依赖不稳定环境变量。

### 风险 C：老规则在升级后 diff 过大

- 应对：接受“规则文本变化”，但确保运行行为不变。

### 风险 D：实现方式退化成半套 DSL

- 应对：只允许固定 token，不开放用户自定义。

---

## 测试矩阵

- [ ] `%USERPROFILE%\Documents\...` 自动升级为 `%DOCUMENTS%\...`
- [ ] `%USERPROFILE%\AppData\Roaming\...` 自动升级为 `%APPDATA%\...`
- [ ] `%USERPROFILE%\AppData\Local\...` 自动升级为 `%LOCALAPPDATA%\...`
- [ ] `%USERPROFILE%\AppData\LocalLow\...` 自动升级为 `%LOCALLOW%\...`
- [ ] `%USERPROFILE%\Saved Games\...` 自动升级为 `%SAVED_GAMES%\...`
- [ ] `%GAME_DIR%` 仍保持优先，不被用户目录锚点抢走
- [ ] 旧规则升级后，备份/恢复行为不变

---

## 推荐实施顺序

建议不要一次性把所有锚点都做完，而是分两批：

### Batch 1

- `%DOCUMENTS%`
- `%APPDATA%`
- `%LOCALAPPDATA%`
- `%LOCALLOW%`
- `%SAVED_GAMES%`

这是最值得做的一批，覆盖绝大多数用户目录型存档。

### Batch 2（未来再看）

- 更多特殊目录
- 平台特定目录
- 账号子目录语义扩展

---

## 验收标准（DoD）

- 规则展示比 `%USERPROFILE%` 粗粒度时代更清晰。
- 老规则在升级后可自动收敛到更具体锚点。
- 备份/恢复/预检查在新锚点下保持行为一致。
- `cargo check` 与前端类型检查通过。

