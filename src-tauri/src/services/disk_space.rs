//! 磁盘可用空间探测与预检。
//!
//! 原先三处各写了一份 `GetDiskFreeSpaceExW` 包装（`add_game_service`、`game_body_update_service`、
//! `library_commands`），三份探测、两种失败语义；而最需要预检的本体打包反而一条检查都没有
//! （代码审查 P2-1：打包要额外写出一份与游戏同体积的 ZIP，磁盘满时 7z 会把 `.tmp` 写坏再报错）。
//!
//! 这里收口成一份：**探测只负责探测**（失败返回 `Err`，绝不伪装成 `Ok(0)`），判定交给纯函数，
//! 由调用方按自己的语义决定「探测失败即拒绝」还是「探测失败即跳过」。

use std::path::Path;

/// 预检额外要求的余量：压缩器自身的临时开销，以及预检完成到写入完成之间的并发增长。
pub const SPACE_HEADROOM_BYTES: u64 = 128 * 1024 * 1024;

/// 探测 `target` 所在卷的可用字节数。
///
/// `target` 可以尚不存在 —— 会先向上找到最近的存在祖先（打包缓存目录就是这么用的：预检发生在
/// 目录创建之前）。无法确认时返回 `Err`，调用方**不要**把它当成 0 或当成"空间充足"。
///
/// 唯一的例外是非 Windows：本项目只支持 Windows，那里直接返回 `u64::MAX`（等于不预检），
/// 以沿用各调用点原来「不因无法探测而阻断操作」的行为。
pub fn available_space(target: &Path) -> Result<u64, String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

        let mut probe = target.to_path_buf();
        while !probe.exists() {
            if !probe.pop() {
                return Err("无法确认游戏库磁盘可用空间".to_string());
            }
        }
        let wide = probe
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut available = 0u64;
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut available,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err("无法确认游戏库磁盘可用空间".to_string());
        }
        Ok(available)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = target;
        // 非 Windows 不做预检：沿用本项目原来的行为（不因无法探测而阻断操作）。
        Ok(u64::MAX)
    }
}

/// 探测 + 判定；探测失败即拒绝。用于「宁可不做、也不能写坏」的路径（添加游戏、本体更新、打包）。
pub fn ensure_available_space(target: &Path, required_bytes: u64) -> Result<(), String> {
    ensure_with_probe(&available_space, target, required_bytes)
}

/// 同上，但允许注入探测 —— 单测要构造「磁盘快满了」和「探不到盘」这两种分支。
pub(crate) fn ensure_with_probe(
    probe: &impl Fn(&Path) -> Result<u64, String>,
    target: &Path,
    required_bytes: u64,
) -> Result<(), String> {
    let available = probe(target)?;
    ensure_capacity(available, required_bytes)
}

/// 纯判定：可用空间是否够 `required_bytes` 加上余量。
fn ensure_capacity(available: u64, required_bytes: u64) -> Result<(), String> {
    let required = required_bytes.saturating_add(SPACE_HEADROOM_BYTES);
    if available < required {
        return Err(format!(
            "游戏库磁盘空间不足，需要至少 {} MB，可用 {} MB",
            required / 1024 / 1024,
            available / 1024 / 1024
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ensure_with_probe, SPACE_HEADROOM_BYTES};
    use std::path::Path;

    const GIB: u64 = 1024 * 1024 * 1024;

    /// 固定探测目标，单测只关心判定；真实探测目标由调用方给。
    fn check(probe: &impl Fn(&Path) -> Result<u64, String>, required: u64) -> Result<(), String> {
        ensure_with_probe(probe, Path::new("C:\\"), required)
    }

    #[test]
    fn headroom_is_added_to_the_requirement() {
        // 可用恰好等于「需求 + 余量」：放行。
        let probe = |_: &Path| Ok(GIB);
        assert!(check(&probe, GIB - SPACE_HEADROOM_BYTES).is_ok());
        // 少一个字节：拒绝，且错误里报的是加上余量后的数字。
        let error = check(&probe, GIB - SPACE_HEADROOM_BYTES + 1).expect_err("差一个字节必须拒绝");
        assert!(error.contains("磁盘空间不足"), "实际错误：{error}");
        assert!(error.contains("1024 MB"), "应当报含余量的需求：{error}");
    }

    #[test]
    fn probe_failure_is_propagated_instead_of_counting_as_free_space() {
        let probe = |_: &Path| Err("探不到盘".to_string());
        let error = check(&probe, 0).expect_err("探测失败必须拒绝，不能当成空间充足");
        assert!(error.contains("探不到盘"), "实际错误：{error}");
    }

    #[test]
    fn requirement_does_not_wrap_around_when_adding_headroom() {
        // 需求接近 u64::MAX 时加上余量会溢出；溢出若绕回小数字，就会把「不够」判成「够」。
        let probe = |_: &Path| Ok(200 * 1024 * 1024);
        let error = check(&probe, u64::MAX - 1).expect_err("溢出不得绕回成放行");
        assert!(error.contains("磁盘空间不足"), "实际错误：{error}");
    }
}
