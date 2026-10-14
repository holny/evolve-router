//! 难度经验校准层（2026-10）：判定器（heuristic / 决策模型）给出的 raw
//! difficulty 存在系统性偏移——实测 heuristic 口径下 96% 请求落在 L3，
//! 导致贵模型被滥用。本层用**滚动窗口的经验分位**把 raw 值单调映射到目标
//! 分布（默认 70% L1 / 20% L2 / 10% L3），使实际分布收敛到预期形状。
//!
//! 设计要点：
//! - 单调保序：只重塑分布形状，不改变"谁比谁难"的相对次序；
//! - 冷启动直通：样本不足（< MIN_SAMPLES）时返回原值，避免早期抖动；
//! - 目标固定 7:2:1（当前无配置项；如需可调，走 `Engine::set_difficulty_calibration`
//!   由 server 侧读取配置后调用——评审 #7：此前注释声称有 [policy] 开关，实际没有）。

use std::collections::VecDeque;
use std::sync::Mutex;

/// 目标分档边界（与评分口径一致）：L1 ≤ 0.8 < L2 ≤ 2.0 < L3
pub const L1_MAX: f32 = 0.8;
pub const L2_MAX: f32 = 2.0;

const WINDOW: usize = 512;
const MIN_SAMPLES: usize = 64;

pub struct DifficultyCalibrator {
    ring: Mutex<VecDeque<f32>>,
    /// 校准后值（同窗口）——用于观测实际分布是否收敛目标
    calibrated: Mutex<VecDeque<f32>>,
    /// [share_l1, share_l2, share_l3]，和为 1
    target: [f32; 3],
}

/// 分布快照（观测用）：样本数 + 原始/校准后的三档占比
#[derive(Clone, Copy, Debug, Default)]
pub struct DifficultySnapshot {
    pub samples: usize,
    pub raw_l1: f32,
    pub raw_l2: f32,
    pub raw_l3: f32,
    pub cal_l1: f32,
    pub cal_l2: f32,
    pub cal_l3: f32,
}

impl Default for DifficultyCalibrator {
    fn default() -> Self {
        Self::new([0.7, 0.2, 0.1])
    }
}

impl DifficultyCalibrator {
    pub fn new(target: [f32; 3]) -> Self {
        Self {
            ring: Mutex::new(VecDeque::with_capacity(WINDOW)),
            calibrated: Mutex::new(VecDeque::with_capacity(WINDOW)),
            target,
        }
    }

    /// 目标分布 [l1,l2,l3]（观测/展示用）
    pub fn target(&self) -> [f32; 3] {
        self.target
    }

    fn push(ring: &Mutex<VecDeque<f32>>, v: f32) {
        if let Ok(mut r) = ring.lock() {
            if r.len() >= WINDOW {
                r.pop_front();
            }
            r.push_back(v);
        }
    }

    fn shares(ring: &Mutex<VecDeque<f32>>) -> (f32, f32, f32) {
        let Ok(r) = ring.lock() else { return (0.0, 0.0, 0.0) };
        if r.is_empty() { return (0.0, 0.0, 0.0); }
        let (mut l1, mut l2, mut l3) = (0.0f32, 0.0f32, 0.0f32);
        for v in r.iter() {
            if *v <= L1_MAX { l1 += 1.0 } else if *v <= L2_MAX { l2 += 1.0 } else { l3 += 1.0 }
        }
        let n = r.len() as f32;
        (l1 / n, l2 / n, l3 / n)
    }

    /// 分布快照：raw vs calibrated 三档占比（面板/统计观测 7:2:1 收敛）
    pub fn snapshot(&self) -> DifficultySnapshot {
        let samples = self.samples();
        let (raw_l1, raw_l2, raw_l3) = Self::shares(&self.ring);
        let (cal_l1, cal_l2, cal_l3) = Self::shares(&self.calibrated);
        DifficultySnapshot { samples, raw_l1, raw_l2, raw_l3, cal_l1, cal_l2, cal_l3 }
    }

    /// 记录原始判定值（每次 decide 调用一次，供分位统计）
    pub fn observe(&self, raw: f32) {
        if let Ok(mut r) = self.ring.lock() {
            if r.len() >= WINDOW {
                r.pop_front();
            }
            r.push_back(raw);
        }
    }

    pub fn samples(&self) -> usize {
        self.ring.lock().map(|r| r.len()).unwrap_or(0)
    }

    /// raw difficulty → 校准后 difficulty（单调保序的分布重塑）。
    /// 用**分位秩**（fractional rank）映射而非值阈值——对重复值稳健：
    /// 并列值按半份额分摊，使实际份额精确逼近目标（值阈值法在并列簇上会跳档）。
    pub fn calibrate(&self, raw: f32) -> f32 {
        let Ok(r) = self.ring.lock() else { return raw };
        if r.len() < MIN_SAMPLES {
            return raw;
        }
        let mut sorted: Vec<f32> = r.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = sorted.len() as f32;
        let below = sorted.partition_point(|v| *v < raw) as f32;
        let eq = sorted.partition_point(|v| *v <= raw) as f32 - below;
        let rank = ((below + 0.5 * eq) / n).clamp(0.0, 1.0);

        let t1 = self.target[0].max(1e-4);
        let t2 = self.target[1].max(1e-4);
        let t3 = (1.0 - t1 - t2).max(1e-4);
        let cal = if rank <= t1 {
            rank / t1 * L1_MAX
        } else if rank <= t1 + t2 {
            L1_MAX + (rank - t1) / t2 * (L2_MAX - L1_MAX)
        } else {
            L2_MAX + (rank - t1 - t2) / t3 * (3.0 - L2_MAX)
        };
        Self::push(&self.calibrated, cal);
        cal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_start_passthrough() {
        let c = DifficultyCalibrator::default();
        assert_eq!(c.calibrate(2.9), 2.9);
        for i in 0..10 { c.observe(1.0 + i as f32 * 0.1); }
        assert_eq!(c.calibrate(2.5), 2.5);
    }

    #[test]
    fn reshapes_biased_distribution_to_target() {
        let c = DifficultyCalibrator::default();
        // 模拟当前真实分布：96% 高位（2.2~3.0），少量低位
        for i in 0..600 {
            let raw = if i % 25 == 0 { 0.6 } else { 2.2 + (i % 9) as f32 * 0.1 };
            c.observe(raw);
        }
        let mut l1 = 0; let mut l2 = 0; let mut l3 = 0;
        for i in 0..600 {
            let raw = if i % 25 == 0 { 0.6 } else { 2.2 + (i % 9) as f32 * 0.1 };
            let cal = c.calibrate(raw);
            if cal <= L1_MAX { l1 += 1 } else if cal <= L2_MAX { l2 += 1 } else { l3 += 1 }
        }
        let n = 600.0;
        let (s1, s2, s3) = (l1 as f32 / n, l2 as f32 / n, l3 as f32 / n);
        // 目标 70/20/10（±6pp 容忍，样本含重复值离散误差）
        assert!((s1 - 0.70).abs() < 0.06, "l1 share={s1}");
        assert!((s2 - 0.20).abs() < 0.06, "l2 share={s2}");
        assert!((s3 - 0.10).abs() < 0.06, "l3 share={s3}");
    }

    #[test]
    fn monotonic_order_preserved() {
        let c = DifficultyCalibrator::default();
        for i in 0..200 { c.observe(i as f32 / 100.0); }
        let a = c.calibrate(0.5);
        let b = c.calibrate(1.0);
        let d = c.calibrate(1.5);
        assert!(a <= b && b <= d, "growing: {a} {b} {d}");
    }
}
