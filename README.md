# GameSaver

GameSaver 是一个基于 `Tauri + Vue` 的游戏存档工具，当前主流程是：

1. 学习存档目录并生成规则
2. 在规则管理中维护路径/启停/导入导出
3. 在游戏库中按游戏一键启动（默认自动备份）

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

- 当前默认模式：自动备份启动（已开放）。
- 沙盒模式（Sandboxie）：
  - 当前状态：开发中，已从主界面入口隐藏，避免误导用户。
  - 计划目标：稳定沙盒启动 + 一键回收沙盒写入。
- 注入模式（CreateFileW 重定向）：
  - 当前状态：开发中，已从主界面入口隐藏，避免误导用户。
  - 计划目标：完善命中率与兼容性后再重新开放入口。
