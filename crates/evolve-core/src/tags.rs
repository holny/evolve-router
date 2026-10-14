//! 模型标签（2026-10，用户裁决）：**能力标签** + **价钱档位标签**，
//! 默认自动推导、用户可在 Web 覆盖。
//!
//! - 能力：basic(简单) / standard(一般) / powerful(牛逼)——来自目录 tiers
//!   （跨家族可比，已含 benchmarck 覆盖与飞轮修正）
//! - 价位：cheap / medium / premium（便宜/中等/贵）——按**同方案家族内相对位置**（p34/p67 分位）
//!   判定，而非绝对价——订阅积分制与按量美元制口径不同，绝对价不可比
//!
//! 标签消费方：价格层硬闸（难度→允许价位）、近 100 请求分布统计、面板展示。

use crate::types::ModelRecord;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
    Basic,
    Standard,
    Powerful,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PriceTier {
    Cheap,
    /// 中价档（原名 standard——与能力标签同名易混淆，2026-10 改名 medium）
    Medium,
    Premium,
}

impl Capability {
    pub fn label(&self) -> &'static str {
        match self {
            Capability::Basic => "basic",
            Capability::Standard => "standard",
            Capability::Powerful => "powerful",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "basic" | "simple" | "简单" => Some(Capability::Basic),
            "standard" | "一般" => Some(Capability::Standard),
            "powerful" | "pro" | "牛逼" => Some(Capability::Powerful),
            _ => None,
        }
    }
}

impl PriceTier {
    pub fn label(&self) -> &'static str {
        match self {
            PriceTier::Cheap => "cheap",
            PriceTier::Medium => "medium",
            PriceTier::Premium => "premium",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "cheap" | "low" | "便宜" => Some(PriceTier::Cheap),
            // "standard" 为历史值（兼容已存的 overrides）
            "standard" | "medium" | "mid" | "一般" | "中等" => Some(PriceTier::Medium),
            "premium" | "high" | "贵" => Some(PriceTier::Premium),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelTags {
    pub capability: Capability,
    pub price_tier: PriceTier,
}

/// 能力默认推导：取目录 tiers 的最大值（跨家族可比）
pub fn default_capability(m: &ModelRecord) -> Capability {
    let t = m
        .tiers
        .coding
        .max(m.tiers.reasoning)
        .max(m.tiers.agentic)
        .max(m.tiers.vision * 0.9);
    if t < 0.55 {
        Capability::Basic
    } else if t < 0.8 {
        Capability::Standard
    } else {
        Capability::Powerful
    }
}

/// 价位默认推导：`rank_pct` = 该模型在同方案家族内的成本相对位置 [0,1]
/// （0 = 最便宜）。无家族信息时按绝对有效单价（元/1M）粗分。
pub fn default_price_tier(rank_pct: Option<f32>, abs_price_per_mtok: Option<f32>) -> PriceTier {
    if let Some(p) = rank_pct {
        return if p < 0.34 {
            PriceTier::Cheap
        } else if p < 0.67 {
            PriceTier::Medium
        } else {
            PriceTier::Premium
        };
    }
    match abs_price_per_mtok {
        Some(v) if v < 2.0 => PriceTier::Cheap,
        Some(v) if v < 8.0 => PriceTier::Medium,
        Some(_) => PriceTier::Premium,
        None => PriceTier::Medium, // 无成本信息：中性
    }
}

/// 组合默认标签
pub fn default_tags(m: &ModelRecord, rank_pct: Option<f32>, abs_price: Option<f32>) -> ModelTags {
    ModelTags {
        capability: default_capability(m),
        price_tier: default_price_tier(rank_pct, abs_price),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Source, Tiers};

    fn mk(coding: f32, reasoning: f32) -> ModelRecord {
        ModelRecord {
            id: "p/m".into(),
            provider: "p".into(),
            base_url: "https://example.com/v1".into(),
            upstream_model: "m".into(),
            tiers: Tiers { coding, reasoning, agentic: coding, vision: 0.0 },
            source: Source::User,
            ..Default::default()
        }
    }

    #[test]
    fn capability_from_tiers() {
        assert_eq!(default_capability(&mk(0.45, 0.5)), Capability::Basic);
        assert_eq!(default_capability(&mk(0.7, 0.65)), Capability::Standard);
        assert_eq!(default_capability(&mk(0.95, 0.9)), Capability::Powerful);
        // 便宜层但能力跨家族强：flash 型 tiers 高 → 判 powerful（不因名字降级）
        assert_eq!(default_capability(&mk(0.9, 0.85)), Capability::Powerful);
    }

    #[test]
    fn price_tier_relative_and_absolute() {
        assert_eq!(default_price_tier(Some(0.1), None), PriceTier::Cheap);
        assert_eq!(default_price_tier(Some(0.5), None), PriceTier::Medium);
        assert_eq!(default_price_tier(Some(0.9), None), PriceTier::Premium);
        assert_eq!(default_price_tier(None, Some(1.2)), PriceTier::Cheap);
        assert_eq!(default_price_tier(None, Some(5.0)), PriceTier::Medium);
        assert_eq!(default_price_tier(None, Some(13.5)), PriceTier::Premium);
        assert_eq!(default_price_tier(None, None), PriceTier::Medium);
        // 历史值兼容
        assert_eq!(PriceTier::parse("standard"), Some(PriceTier::Medium));
    }

    #[test]
    fn label_roundtrip() {
        for c in ["basic", "standard", "powerful", "简单", "牛逼"] {
            assert!(Capability::parse(c).is_some(), "{c}");
        }
        for p in ["cheap", "medium", "mid", "standard", "premium", "便宜", "贵"] {
            assert!(PriceTier::parse(p).is_some(), "{p}");
        }
    }
}
