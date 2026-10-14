//! 层级比例反馈控制器（2026-10，S5）：把"实际被选用的层级分布"拉回目标
//! 7:2:1（cheap/medium/premium）。判定器偏松/偏严或供给倾斜时，由**实际消耗
//! 分布**反向纠偏——与难度分位校准（calibration.rs）互补：一个管输入判定，
//! 一个管输出选择。
//!
//! 机制：滑动窗口（默认 200）统计各价位层占比；超出目标容忍带 → 对该层施加
//! **成本软惩罚**（乘子 >1，随超出幅度增长，封顶）；低于容忍带 → 轻微偏好
//! （乘子 <1）。惩罚作用于评分里的成本因子，不改变硬闸与质量门槛。

use std::collections::VecDeque;
use std::sync::Mutex;

const WINDOW: usize = 200;
const MIN_SAMPLES: usize = 40;

pub struct RatioController {
    window: Mutex<VecDeque<u8>>, // 0=cheap 1=medium 2=premium
    target: [f32; 3],
    band: f32,
}

/// 观测快照（面板/统计）
#[derive(Clone, Copy, Debug, Default)]
pub struct RatioSnapshot {
    pub samples: usize,
    pub cheap: f32,
    pub medium: f32,
    pub premium: f32,
    pub penalties: [f32; 3],
}

impl Default for RatioController {
    fn default() -> Self {
        Self::new([0.7, 0.2, 0.1], 0.05)
    }
}

impl RatioController {
    pub fn new(target: [f32; 3], band: f32) -> Self {
        Self {
            window: Mutex::new(VecDeque::with_capacity(WINDOW)),
            target,
            band: band.clamp(0.0, 0.3),
        }
    }

    pub fn observe(&self, tier: u8) {
        if let Ok(mut w) = self.window.lock() {
            if w.len() >= WINDOW {
                w.pop_front();
            }
            w.push_back(tier.min(2));
        }
    }

    pub fn samples(&self) -> usize {
        self.window.lock().map(|w| w.len()).unwrap_or(0)
    }

    fn shares(&self) -> [f32; 3] {
        let Ok(w) = self.window.lock() else { return [0.0; 3] };
        if w.is_empty() {
            return [0.0; 3];
        }
        let mut c = [0f32; 3];
        for t in w.iter() {
            c[*t as usize] += 1.0;
        }
        let n = w.len() as f32;
        [c[0] / n, c[1] / n, c[2] / n]
    }

    /// 各层成本乘子：超标层 >1（抑制），不足层 <1（偏好），带内 =1
    pub fn penalties(&self) -> [f32; 3] {
        if self.samples() < MIN_SAMPLES {
            return [1.0; 3];
        }
        let s = self.shares();
        let mut out = [1.0f32; 3];
        for i in 0..3 {
            let hi = self.target[i] + self.band;
            let lo = (self.target[i] - self.band).max(0.0);
            out[i] = if s[i] > hi {
                // 超出幅度归一（以"超出目标的比例"衡量），乘子 1.0 → 1.5
                let overshoot = (s[i] - hi) / (1.0 - hi).max(1e-3);
                (1.0 + overshoot * 0.5).clamp(1.0, 1.5)
            } else if s[i] < lo {
                // 不足：轻微偏好（0.9 起，按缺口比例加深，下限 0.85）
                let gap = (lo - s[i]) / lo.max(1e-3);
                (1.0 - gap * 0.15).clamp(0.85, 1.0)
            } else {
                1.0
            };
        }
        out
    }

    pub fn snapshot(&self) -> RatioSnapshot {
        let s = self.shares();
        RatioSnapshot {
            samples: self.samples(),
            cheap: s[0],
            medium: s[1],
            premium: s[2],
            penalties: self.penalties(),
        }
    }

    pub fn target(&self) -> [f32; 3] {
        self.target
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_start_no_penalty() {
        let c = RatioController::default();
        assert_eq!(c.penalties(), [1.0; 3]);
        for _ in 0..10 {
            c.observe(2);
        }
        assert_eq!(c.penalties(), [1.0; 3], "below MIN_SAMPLES → neutral");
    }

    #[test]
    fn overshoot_penalizes_and_undershoot_prefers() {
        let c = RatioController::default();
        // 全部 premium（严重超标）→ premium 乘子升高；cheap/medium 不足 → <1
        for _ in 0..200 {
            c.observe(2);
        }
        let p = c.penalties();
        assert!(p[2] > 1.2, "premium overshoot must be penalized: {p:?}");
        assert!(p[0] < 1.0 && p[1] < 1.0, "cheap/medium undershoot must be preferred: {p:?}");
        assert!(p[2] <= 1.5, "cap: {p:?}");
    }

    #[test]
    fn on_target_no_penalty() {
        let c = RatioController::default();
        // 70/20/10 精确命中 → 全部中性
        for _ in 0..140 {
            c.observe(0);
        }
        for _ in 0..40 {
            c.observe(1);
        }
        for _ in 0..20 {
            c.observe(2);
        }
        assert_eq!(c.penalties(), [1.0; 3]);
    }
}
