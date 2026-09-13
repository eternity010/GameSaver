//! 测试专用的共享夹具支持（`#[cfg(test)]`，不进产物）。
//!
//! 目前只有一个成员：[`TempWorkspace`] —— 带 `Drop` 清理的测试临时目录。三处夹具用它：
//! `save_repository`、`save_learning_service`、`launch_service`。
//!
//! 为什么需要它：这些夹具都要一棵**真实目录树**，而且前两处刻意不能建在 `%TEMP%` 下 ——
//! 那里正是 `is_noise_path` 明确挡掉的噪音目录（片段表里有 `\appdata\local\temp\`），拿它当
//! 范围根会让候选过滤把测试文件全判成噪音，测到的就不是策略而是噪音规则了。于是它们在
//! **crate 工作目录**下建树，再在测试最后一行 `remove_dir_all`。
//!
//! 那个「最后一行」就是漏洞：`assert!` 失败会 panic，之后的清理代码根本不执行。于是一次失败的
//! 测试就永久留下一棵目录树 —— 实测一次 P5 迭代就在 `src-tauri/` 下攒了 **18 个**目录，
//! `git status` 里全是 `??`，还得靠人肉分辨哪些是泄漏、哪些是有用的夹具产物；
//! `launch_service` 更彻底，它只有 `create_dir_all`、没有清理，在 `%TEMP%` 下积了 **547 个**。
//!
//! `Drop` 在 unwind 时照常运行，所以把清理挂在 `Drop` 上，失败路径与成功路径就**共用同一条**
//! 清理逻辑：测试无论怎么结束，目录都会走。这一点有实测：故意写一个 panic 的探针测试，
//! 测试报告 FAILED，而它建的目录没有留下。

use std::path::{Path, PathBuf};

/// 一个随作用域自动清理的测试临时目录。
///
/// 用法与 `PathBuf` 基本一致（`Deref<Target = Path>`，所以 `root.join(..)`、`&root` 都能直接用）；
/// 需要真正的 `PathBuf` 时用 `root.to_path_buf()`。
///
/// **不要**再把显式 `remove_dir_all` 写回测试末尾 —— 清理已经归 `Drop` 管，重复一遍只是噪音。
pub(crate) struct TempWorkspace {
    path: PathBuf,
}

impl TempWorkspace {
    /// 在 crate 工作目录下建一个 `gamesaver-{label}-{uuid}` 目录。
    ///
    /// `label` 是给**失败现场**看的：留下目录的场合，光有一个 uuid 根本认不出是哪个测试、
    /// 在测哪条规则。所以调用方一律传测试语义的名字（`p5-filter`、`r5-asset-dir`…）。
    pub(crate) fn new(label: &str) -> Self {
        let path = std::env::current_dir()
            .expect("resolve test working directory")
            .join(format!("gamesaver-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create test workspace");
        Self { path }
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        // 刻意忽略错误：`Drop` 里不能 panic（unwind 期间再 panic 直接 abort），而且清理失败
        // （文件被别的进程占住之类）不该把一次本来通过的测试变成失败。清理漏掉的场合，
        // 目录名里的 label 就是找回现场的路标。
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

impl std::ops::Deref for TempWorkspace {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempWorkspace {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Debug for TempWorkspace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 断言失败时打印的是**路径**，不是结构体名 —— 定位现场要的就是路径。
        self.path.fmt(formatter)
    }
}
