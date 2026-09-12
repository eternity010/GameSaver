//! 纯路径规范化工具。
//!
//! 之所以放在领域层：`normalize_path` 与 `strip_verbatim_prefix` 同时被服务层与仓储层
//! 需要，而仓储层不能反向依赖服务层；两者本身又都是纯字符串处理、不碰 IO。
//!
//! 此前 `strip_verbatim_prefix` 在服务层（`save_learning_service.rs`）与仓储层
//! （`save_repository.rs`）各有一份**逐字节相同**的实现，本次下沉合并成一份
//! （存档识别审查 R2b 第 1 步）。

use std::path::{Path, PathBuf};

/// 去掉 Windows 的 `\\?\` / `\\?\UNC\` 前缀。
///
/// `canonicalize` 在 Windows 上会返回带 `\\?\` 的 verbatim 路径，它不能直接参与字符串
/// 比较，也不能与用户输入的路径拼在一起。
pub(crate) fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{}", rest))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

/// 归一化路径：去 verbatim 前缀、`/` 统一成 `\`、去尾部 `\`、转小写。
///
/// 刻意只折叠 ASCII 大小写（`to_ascii_lowercase`）：非 ASCII 字节原样保留，
/// 避免中文/日文目录名在不同机器上被折叠成不同形态。
pub(crate) fn normalize_path(path: &Path) -> String {
    let clean = strip_verbatim_prefix(path);
    clean
        .to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}
