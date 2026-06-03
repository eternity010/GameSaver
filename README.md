# GameSaver

GameSaver 是一个基于 `Tauri + Vue` 的游戏存档工具，当前主流程是：

1. 学习存档目录并生成规则
2. 在规则管理中维护路径/启停/导入导出
3. 在游戏库中按游戏一键启动（默认自动备份）

## 当前后端入口状态

- Active entrypoint: `src-tauri/src/lib.rs`（模块化后端）。
- Legacy reference only: `src-tauri/src/lib_legacy.rs`（仅归档参考，不再新增功能）。
- 当前支持启动模式：`backup`、`backup_direct`。
- `inject` / `sandbox` 已取消，不在现行流程中。

## 环境要求（Windows）

- Node.js 20+
- Rust stable
- Visual Studio Build Tools（含 C++ 工具链）

## 本地开发

```bash
npm install
npm run tauri dev
```

## 本地构建

```bash
npm run build
cargo check --manifest-path .\src-tauri\Cargo.toml
```

## 本地打包

仅打 exe 安装包（NSIS）：

```bash
npx tauri build --bundles nsis
```

输出路径：

`src-tauri/target/release/bundle/nsis/GameSaver_*.exe`

> 说明：MSI 打包依赖 WiX，偶发会受本机安全软件/文件锁影响。  
> CI 工作流已包含 MSI 构建（best-effort）。

## GitHub 发布（从 0 开始）

1. 在 GitHub 创建空仓库（例如 `GameSaver`）。
2. 在本地项目根目录执行：

```bash
git init
git add .
git commit -m "chore: initial release setup"
git branch -M main
git remote add origin <你的仓库地址>
git push -u origin main
```

3. 打发布标签并推送：

```bash
git tag v0.1.0
git push origin v0.1.0
```

4. GitHub Actions 会自动构建安装包并创建/更新 Release。

## 自动发布工作流

已配置：`.github/workflows/release.yml`

- 触发条件：
  - push tag：`v*`（如 `v0.1.0`）
  - 手动触发：`workflow_dispatch`
- 产物：
  - `NSIS .exe`（必做）
  - `MSI`（best-effort，失败不阻塞 exe 发布）

## 开发计划（UX 优先）

- 当前默认模式：自动备份启动（已开放，可靠初版）。
- 备份版本管理：
  - 默认每个游戏保留最近 10 个备份版本。
  - 支持查看备份版本时间线、回滚到指定版本、撤销本次恢复。
  - 启动前会检查本地存档与最近备份的状态，给出直接启动/恢复后启动/人工确认建议。
- 设置页：
  - 已新增顶层“设置”页。
  - 当前只保留 `backupRoot` 作为正式用户数据路径。
  - 支持浏览选择目录、打开当前目录、仅保存路径、复制后切换的备份目录迁移。
  - 迁移成功后才更新配置，默认保留旧目录，不自动删除旧数据。
- 迁移包：
  - 规则和备份仍按当前 `backupRoot` 导出/导入。
  - 用户可先在设置页迁移备份目录，再导出或导入迁移包。
- 前端结构：
  - 已将学习存档页、规则管理页、游戏库页拆成页面组件。
  - 游戏库左侧卡片和右侧详情已组件化。
  - 当前 UI 正在压缩默认可见信息：游戏库、设置页、学习结果页优先展示高频状态与主操作，低频诊断信息折叠展示。
- 已取消开发计划：
  - 沙盒模式（Sandboxie）已取消，不在现行流程中。
  - 注入模式（CreateFileW 重定向）已取消，不在现行流程中。
  - `managedSaveRoot` 已从现行产品链路中移除，仅历史参考代码中可能仍有遗留描述。

## 当前开发进度

- 版本状态：当前最新正式版本为 `v0.1.24`。
- 后端状态：
  - `src-tauri/src/lib.rs` 是当前模块化入口。
  - `src-tauri/src/lib_legacy.rs` 仅作为历史参考，不再承接新功能。
  - 当前核心模块包括学习、规则、游戏库、启动器、备份、迁移、设置、运行时状态。
- 前端状态：
  - 当前工作重点是降低信息密度，而不是增加新功能入口。
  - 已完成游戏库详情页第一轮压缩：顶部摘要、同步依据折叠、备份空间管理默认折叠。
  - 已完成设置页压缩：单一备份目录面板，默认路径折叠展示。
  - 已完成学习结果页压缩：候选路径默认展示推荐理由，得分和文件变化数折叠到“查看依据”。
- 本轮已完成：
  - 学习结果页新增“代表性变更文件”依据展示，默认直出前三项，其余折叠。
  - 规则管理页列表改为摘要优先，编辑区按需展开。
  - 游戏库列表与详情区域的重叠问题已修复。
# Version Release Checklist (Important)

To avoid "new tag but old installer version" issues, keep these in sync before every release:

- `package.json` -> `version`
- `src-tauri/Cargo.toml` -> `package.version`
- `src-tauri/tauri.conf.json` -> `version`

Recommended release flow:

1. Update the three versions above to the same value (for example `0.1.16`).
2. Validate locally:
   - `npm run build`
   - `cargo check --manifest-path .\src-tauri\Cargo.toml`
3. Push `main` first, then create/push tag:
   - `git push origin main`
   - `git tag vX.Y.Z`
   - `git push origin vX.Y.Z`
4. Verify tag target is the latest commit:
   - `git show --no-patch --oneline vX.Y.Z`
   - `git ls-remote --tags origin vX.Y.Z`

If GitHub Release shows a new tag but assets still have an old version name (for example `GameSaver_0.1.14_...`), usually:

- The tag points to an old commit.
- `src-tauri/tauri.conf.json` version was not updated.
