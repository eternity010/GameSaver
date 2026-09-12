//! 单个路径段的安全判定。
//!
//! 「路径段」指会被拼进某个根目录之下、只占一层的名字：游戏 UID、远程目录名、
//! 封面协议 URL 里的标识符等。这类值一旦含目录分隔符或 `..`，`Path::join` 就会
//! 把它当作跨目录移动，于是「拼在根目录之下」这个前提直接失效，读写落到根之外。
//!
//! 之所以把它放在领域层：判定是纯逻辑、不碰 IO，而调用方分处命令层与协议层，
//! 收在这里才不会让两侧各写一份强度不一的检查。

/// 判定一段文本可以安全地作为**单个**路径段拼接。
///
/// 拒绝空串、`.`、`..`、含任一目录分隔符（`/`、`\`）或控制字符。
/// 刻意不做字符白名单：游戏名允许空格、中文、括号，限制字符集会误伤合法值。
pub fn is_safe_path_segment(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
        && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::is_safe_path_segment;

    #[test]
    fn accepts_real_directory_names() {
        for value in [
            "409fedc3-aee4-4e6d-a584-f9ef01fa9b5b",
            "monster black market",
            "肉遊びver1.0.7",
            "[g20240404]black market",
            "  spaced name  ",
        ] {
            assert!(is_safe_path_segment(value), "rejected safe name: {value:?}");
        }
    }

    #[test]
    fn rejects_anything_that_moves_the_path() {
        for value in [
            "",
            "   ",
            ".",
            "..",
            "../escape",
            "..\\escape",
            "a/b",
            "a\\b",
            "/absolute",
            "trailing/",
            "game\nname",
            "game\0name",
        ] {
            assert!(
                !is_safe_path_segment(value),
                "accepted unsafe segment: {value:?}"
            );
        }
    }
}
