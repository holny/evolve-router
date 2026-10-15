//! 阈值与难度分档的**单一事实源**。
//!
//! 这些常量此前散落在 engine（价位层硬闸 / 粘性档位闸 / 探索门控 / 视觉硬约束）、
//! scoring（高价值加成）与服务端 decision_tags（标签与 fit）里各写一份，改一处不会
//! 报警、另一处静默漂移（2026-10-15 review 指出）。现在统一从这里取。
//!
//! 客户端（Swift 面板 / dashboard.html）不引 Rust 常量：它们只渲染服务端算好的
//! 派生标签与门控布尔值（见 `decision_tags.rs`），所以这里只需保证服务端内部一致。

/// 难度 → 价位层的分档边界：≤0.8 cheap / ≤2.0 medium / >2.0 premium
pub const DIFF_MEDIUM: f32 = 0.8;
pub const DIFF_HARD: f32 = 2.0;

/// 请求特征阈值（与路由闸门同值）
pub const VISION: f32 = 0.5;
pub const STAKES_HIGH: f32 = 0.6;

/// 难度 → 价位层索引（0=cheap / 1=medium / 2=premium）。
/// engine 的价格层硬闸、粘性档位闸、decision_tags 的难度标签与 fit 都用它
pub fn tier_index(difficulty: f32) -> usize {
    if difficulty <= DIFF_MEDIUM {
        0
    } else if difficulty <= DIFF_HARD {
        1
    } else {
        2
    }
}

/// 难度档位标签（面板/Web 展示词；取值集合也由契约测试锁定）
pub fn difficulty_label(difficulty: f32) -> &'static str {
    ["easy", "medium", "hard"][tier_index(difficulty)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bands_are_shared_and_ordered() {
        assert_eq!(tier_index(0.0), 0);
        assert_eq!(tier_index(DIFF_MEDIUM), 0);
        assert_eq!(tier_index(DIFF_MEDIUM + 0.01), 1);
        assert_eq!(tier_index(DIFF_HARD), 1);
        assert_eq!(tier_index(DIFF_HARD + 0.01), 2);
        assert_eq!(difficulty_label(0.5), "easy");
        assert_eq!(difficulty_label(1.5), "medium");
        assert_eq!(difficulty_label(3.0), "hard");
    }
}
