//! `created_at` 字段的比较语义。
//!
//! 项目里所有 `created_at` 都落盘成「以秒为单位的 unix 时间戳」的十进制字符串
//! （各处的 `now_iso()`）。因此**字符串比较恰好等于数值比较** —— 但这只是巧合，
//! 没有任何东西保证它：字段叫 `created_at`、生产函数叫 `now_iso`，都暗示着时间语义，
//! 一旦有人把它改成真正的 ISO 8601、加上毫秒、或只是位数变化，字符串比较就会与时间
//! 顺序脱钩。而这些比较正在决定**删掉哪个版本**（保留策略、清理命令、剪枝、本体版本
//! 归档），搞反方向就是「删掉最新的、留下最旧的」。
//!
//! 所以凡是按 `created_at` 排序，一律走这里，不要直接 `String::cmp`。

use std::cmp::Ordering;

/// 按时间先后比较两个 `created_at` 值。
///
/// 两边都能解析成数字就按数值比；否则退回字符串比较 —— 真正的 ISO 8601 恰好是按
/// 字典序单调的，所以两条路都不会把时间顺序搞反。混用两种格式（只可能来自数据损坏）
/// 时顺序没有语义，但仍是确定的。
pub fn compare_created_at(left: &str, right: &str) -> Ordering {
    match (left.parse::<u64>(), right.parse::<u64>()) {
        (Ok(left), Ok(right)) => left.cmp(&right),
        _ => left.cmp(right),
    }
}

/// 同 [`compare_created_at`]，但字段本身可能缺失（云端的包元数据用 `Option<String>`）。
///
/// `None` 排在 `Some` 之前，与 `Option::cmp` 原本的次序一致 —— 不打乱既有排序结果，
/// 只把「有时间戳的那些」换成按时间而不是按字典序比。
pub fn compare_optional_created_at(left: Option<&str>, right: Option<&str>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => compare_created_at(left, right),
        (left, right) => left.is_some().cmp(&right.is_some()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    #[test]
    fn compares_numeric_timestamps_by_value_not_by_text() {
        // 字符串比较会认为 "9" > "10"（首个字符 '9' > '1'），数值比较才是对的。
        // 这条测试就是「必须解析成数字」的守门人。
        assert_eq!(compare_created_at("9", "10"), Ordering::Less);
        assert_eq!(compare_created_at("100", "99"), Ordering::Greater);
        assert_eq!(
            compare_created_at("1788357696", "1788357696"),
            Ordering::Equal
        );
        // 位数不同也不受影响（字符串比较在这里同样会错）。
        assert_eq!(
            compare_created_at("9999999999", "10000000000"),
            Ordering::Less
        );
    }

    #[test]
    fn compares_iso_timestamps_lexicographically() {
        assert_eq!(
            compare_created_at("2026-09-10T12:00:00Z", "2026-09-10T13:00:00Z"),
            Ordering::Less
        );
        assert_eq!(
            compare_created_at("2026-09-11T00:00:00Z", "2026-09-10T23:59:59Z"),
            Ordering::Greater
        );
    }

    #[test]
    fn missing_timestamps_sort_before_present_ones() {
        // 与 `Option::cmp` 一致：None < Some，不能把「没有时间戳」当成最新。
        assert_eq!(
            compare_optional_created_at(None, Some("10")),
            Ordering::Less
        );
        assert_eq!(
            compare_optional_created_at(Some("10"), None),
            Ordering::Greater
        );
        assert_eq!(compare_optional_created_at(None, None), Ordering::Equal);
        // 两边都有则回到按数值比较（字符串比较会认为 "9" > "10"）。
        assert_eq!(
            compare_optional_created_at(Some("9"), Some("10")),
            Ordering::Less
        );
    }

    #[test]
    fn mixed_formats_still_produce_a_deterministic_order() {
        // 只可能来自数据损坏；不保证语义，但必须是全序、不能 panic 或返回随机结果。
        let verdict = compare_created_at("1788357696", "2026-09-10T12:00:00Z");
        assert_eq!(verdict, "1788357696".cmp("2026-09-10T12:00:00Z"));
        assert_eq!(
            verdict,
            compare_created_at("1788357696", "2026-09-10T12:00:00Z")
        );
    }
}
