# 本地测试与安装包构建

## 日常开发

```powershell
npm run tauri dev
```

适合界面和常规功能开发，无需反复生成、安装安装包。开发模式不自动请求管理员权限；需要验证 ETW 时使用管理员终端启动。

## 快速测试安装包

```powershell
npm run build:installer:test
```

保持 release 模式、内嵌前端和当前管理员启动清单，关闭 LTO，使用 16 个代码生成单元、二级优化及增量编译，仅封装 NSIS EXE。覆盖参数只对子进程生效，不修改正式 release 配置。

独立缓存和产物目录：

```text
src-tauri/target/test-installer/release/bundle/nsis/
```

首次执行需要编译依赖并建立缓存，仍可能耗时较长；后续使用同一命令重复构建可复用增量缓存。独立目录会额外占用构建缓存空间。不要日常清空 target 或执行 cargo clean；切换正式和测试构建时两套缓存互不替换。实际提速需以本机后续构建记录为准。

可不执行编译，只检查命令和覆盖参数：

```powershell
npm run build:installer:test -- --dry-run
```

测试包用于功能验证，不用于最终性能基准。修改过的 Rust 代码、依赖和内嵌前端仍需要重新编译，增量缓存不代表完全免编译。

## 正式发布安装包

```powershell
npm run tauri -- build
```

沿用 `Cargo.toml` 中 `lto = true`、`codegen-units = 1`、`opt-level = 3` 的正式配置，并按 Tauri 配置生成 EXE 和 MSI。

```text
src-tauri/target/release/bundle/nsis/
src-tauri/target/release/bundle/msi/
```

2026-11-16 的 1.0.32 正式包构建中，Rust release 编译用时 6 分 18 秒，Vite 打包用时 347 毫秒；主要瓶颈是 Rust 发布编译，少生成一种安装格式只能节省末尾封装时间。
