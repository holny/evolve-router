//! Provider 信息聚合模块（2026-10，用户裁决：官方信息集中一处，按 baseURL/hostname 区分）。
//!
//! 职责：把"该 provider 的官方信息"聚合为单点查询，避免散落各处：
//! - 配额窗口额度（5h/周/月）、模型抵扣系数与价格 → 复用 [`crate::plans`] 注册表（不重复建设）
//! - 官方配额查询端点 → 复用 [`crate::plans::quota_api_for`]
//! - 模型列表 → 由 discovery 远程拉取（/v1/models）后进 catalog，此处只给文档入口
//! - **峰谷时段与折扣**（本模块新增）：影响有效成本与错峰调度
//! - 官方文档链接 + **核对日期**（reviewed_at）：`review_due()` 超过 STALE_DAYS
//!   即提示重新核对官方文档——价格/折扣/额度是会变的，必须定期更新
//!
//! 数据来源约定：`source` 标记 `official-docs`（已按官方文档核对）或
//! `to-verify`（待核对，值为保守假设）。更新流程：打开 `docs_url` 核对 →
//! 改本表 → 更新 `reviewed_at`。

use crate::plans::{self, QuotaApi};

/// 核对有效期：超过则提示复核（价格/折扣/额度会变）
pub const STALE_DAYS: i64 = 30;

/// 峰谷时段（官方文档口径）
#[derive(Clone, Copy, Debug)]
pub struct PeakWindow {
    /// 是否仅工作日（周一~周五；中国法定节假日官方按非高峰处理，此处未建节假日表）
    pub weekdays_only: bool,
    /// 高峰时段（本地时区，小时，半开区间 [start, end)）
    pub ranges: &'static [(u8, u8)],
    /// 时区偏移（分钟）——智谱/DeepSeek/火山均 UTC+8
    pub tz_offset_min: i32,
    /// 非高峰折扣系数（0.5 = 半价）
    pub off_peak_factor: f32,
}

/// Provider 官方信息聚合
#[derive(Clone, Copy, Debug)]
pub struct ProviderInfo {
    /// hostname 匹配（后缀匹配）
    pub host: &'static str,
    /// 计划注册表键（复用 plans::plan_key_for）
    pub plan_key: Option<&'static str>,
    /// 官方配额查询端点族（复用 plans::quota_api_for）
    pub quota_api: Option<QuotaApi>,
    /// 峰谷时段（None = 无峰谷定价）
    pub peak: Option<PeakWindow>,
    /// 官方文档（配额/价格/额度核对入口）
    pub docs_url: &'static str,
    /// 核对日期（YYYY-MM-DD）
    pub reviewed_at: &'static str,
    /// 数据来源：official-docs | to-verify
    pub source: &'static str,
    /// 备注（额度口径、活动价等）
    pub note: &'static str,
}

/// 注册表：hostname → 官方信息（新增 provider 在此追加一行）
pub const PROVIDERS: &[ProviderInfo] = &[
    ProviderInfo {
        host: "api.deepseek.com",
        plan_key: None,
        quota_api: Some(QuotaApi::DeepseekBalance),
        peak: Some(PeakWindow {
            weekdays_only: true,
            ranges: &[(9, 12), (14, 18)],
            tz_offset_min: 8 * 60,
            off_peak_factor: 0.5,
        }),
        docs_url: "https://api-docs.deepseek.com/zh-cn/quick_start/pricing",
        reviewed_at: "2026-10-14",
        source: "official-docs",
        note: "按量计费；缓存命中输入价约为未命中 1/50；高峰=周一~五 9-12/14-18 (UTC+8)，其余半价；输出 flash 4 / pro 13.5（CNY，空闲档）",
    },
    ProviderInfo {
        host: "open.bigmodel.cn",
        plan_key: Some("zhipu-coding"),
        quota_api: Some(QuotaApi::ZhipuMonitor),
        peak: Some(PeakWindow {
            weekdays_only: true,
            ranges: &[(14, 18)],
            tz_offset_min: 8 * 60,
            off_peak_factor: 0.5,
        }),
        docs_url: "https://docs.bigmodel.cn/cn/coding-plan/overview",
        reviewed_at: "2026-10-14",
        source: "official-docs",
        note: "Coding Plan：5h 动态刷新 + 周额度（Lite 2k/10k · Pro 12k/60k · Max 28k/140k）；抵扣系数 GLM-5.3 6.9/1.7/24、Flash 2.3/0.56/8；非高峰 50% 抵扣（高峰=周一~五 14-18 UTC+8）",
    },
    ProviderInfo {
        host: "api.z.ai",
        plan_key: Some("zai-devpack"),
        quota_api: Some(QuotaApi::ZaiMonitor),
        peak: Some(PeakWindow {
            weekdays_only: true,
            ranges: &[(14, 18)],
            tz_offset_min: 8 * 60,
            off_peak_factor: 0.5,
        }),
        docs_url: "https://docs.z.ai/devpack/overview",
        reviewed_at: "2026-10-14",
        source: "official-docs",
        note: "DevPack：5h/周积分（Lite 2k/10k · Pro 12k/60k · Max 28k/140k）；系数 GLM-5.3 6.9/1.7/24、Flash 2.3/0.56/8；非高峰 50% 抵扣（高峰=周一~五 14-18 新加坡时间 UTC+8）；$18/月起",
    },
    ProviderInfo {
        host: "api.minimax.cn",
        plan_key: Some("minimax-coding"),
        quota_api: Some(QuotaApi::MinimaxRemains),
        peak: None,
        docs_url: "https://platform.minimaxi.com/document/Coding%20Plan",
        reviewed_at: "2026-10-14",
        source: "web-verified",
        note: "M Plan：官方 remains 端点（5h/周 百分比）；额度=5h 固定窗 + 周窗双滚动；**未见公开峰谷折扣**（活动期首月 5 折，2026-10-01~14）",
    },
    ProviderInfo {
        host: "ark.cn-beijing.volces.com",
        plan_key: Some("volces-coding"),
        quota_api: None,
        peak: None,
        docs_url: "https://docs.volcengine.com/docs/ark/coding-plan-personal-plan-overview",
        reviewed_at: "2026-10-14",
        source: "web-verified",
        note: "方舟 Coding/Agent Plan：**无长期峰谷规则**（官方仅活动期折扣：指定模型抵扣系数最低 2.5 折，2026-06~11）；在线推理常规 API 另有分时段定价（非套餐口径）；AFP 抵扣系数见 plans 注册表",
    },
    ProviderInfo {
        host: "opencode.ai",
        plan_key: Some("opencode-go"),
        quota_api: Some(QuotaApi::OpencodeZen),
        peak: None,
        docs_url: "https://opencode.ai/docs/go/",
        reviewed_at: "2026-10-10",
        source: "to-verify",
        note: "Zen Go：月度美元额度（rolling/weekly/monthly 百分比端点）；未见峰谷定价——待核对",
    },
];

/// 官方价目表条目（按 1M tokens 计；provider 币种；空闲档价格——高峰按 peak_factor 折算）
#[derive(Clone, Copy, Debug)]
pub struct PriceSheet {
    /// 模型 id 片段匹配（小写包含）
    pub model_match: &'static str,
    /// 缓存未命中输入价
    pub in_miss: f32,
    /// 缓存命中输入价
    pub in_hit: f32,
    /// 输出价
    pub out: f32,
    pub currency: &'static str,
}

/// 官方价目表（2026-10 核对；S1 成本模型的数据源）。价格以空闲档列出，
/// 高峰档由 peak_factor 折算（DeepSeek 高峰 = 空闲 ×2）。
pub const PRICES: &[(&str, &[PriceSheet])] = &[
    ("api.deepseek.com", &[
        PriceSheet { model_match: "flash", in_miss: 1.0, in_hit: 0.02, out: 4.0, currency: "CNY" },
        PriceSheet { model_match: "pro", in_miss: 4.5, in_hit: 0.15, out: 13.5, currency: "CNY" },
    ]),
    // 智谱/opencode/火山为套餐制——成本口径为配额积分/美元（见 plans::model_rates），
    // 不在此列（避免与订阅口径重复）；按量 provider 后续在此追加
];

/// 价格查询：hostname + 模型 id（小写包含匹配）→ 官方价目
pub fn price_for(base_url: &str, model_id: &str) -> Option<&'static PriceSheet> {
    let info = info_for(base_url)?;
    let m = model_id.to_lowercase();
    for (host, sheets) in PRICES {
        if info.host == *host {
            if let Some(s) = sheets.iter().find(|s| m.contains(s.model_match)) {
                return Some(s);
            }
        }
    }
    None
}

/// 有效单价估算（1M tokens 口径）：按给定缓存命中率与输出比例折算，
/// 再乘峰谷系数（非高峰更低）。用于跨 model 的等价成本比较（S1 消费方）。
pub fn effective_price_per_mtok(
    base_url: &str,
    model_id: &str,
    hit_rate: f32,
    out_ratio: f32,
    epoch_ms: u64,
) -> Option<f32> {
    let s = price_for(base_url, model_id)?;
    let h = hit_rate.clamp(0.0, 1.0);
    let in_eff = s.in_miss * (1.0 - h) + s.in_hit * h;
    let base = in_eff + s.out * out_ratio.max(0.0);
    Some(base * peak_factor(base_url, epoch_ms))
}

/// 按 baseURL 找官方信息（hostname 后缀匹配；无匹配返回 None——例如自建端点）
pub fn info_for(base_url: &str) -> Option<&'static ProviderInfo> {
    let host = base_url
        .split("://")
        .nth(1)
        .unwrap_or(base_url)
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_lowercase();
    PROVIDERS
        .iter()
        .find(|p| host == p.host || host.ends_with(&format!(".{}", p.host)))
}

/// 峰谷折扣系数：非高峰返回 off_peak_factor（如 0.5），高峰/无峰谷返回 1.0。
/// `epoch_ms` 为 UTC 毫秒（测试可注入固定时刻）。
pub fn peak_factor(host_or_url: &str, epoch_ms: u64) -> f32 {
    let Some(info) = info_for(host_or_url) else { return 1.0 };
    let Some(peak) = info.peak else { return 1.0 };
    // UTC → provider 本地时区
    let local_min = (epoch_ms / 60_000) as i64 + peak.tz_offset_min as i64;
    let day_min = ((local_min % (24 * 60)) + 24 * 60) % (24 * 60);
    // 星期：1970-01-01 为周四 → weekday index（0=周一）
    let days = local_min.div_euclid(24 * 60);
    let weekday = ((days + 3) % 7) as u8; // 0=周一 … 6=周日
    if peak.weekdays_only && weekday >= 5 {
        return peak.off_peak_factor; // 周末全天非高峰
    }
    let hour = (day_min / 60) as u8;
    let in_peak = peak.ranges.iter().any(|(s, e)| hour >= *s && hour < *e);
    if in_peak { 1.0 } else { peak.off_peak_factor }
}

/// 便捷包装：当前时刻（秒级足够）
pub fn peak_factor_now(base_url: &str) -> f32 {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    peak_factor(base_url, now_ms)
}

/// 待复核清单：reviewed_at 超过 STALE_DAYS 的 provider（启动/巡检日志提示）
pub fn review_due(now_epoch_ms: u64) -> Vec<&'static ProviderInfo> {
    let now_days = (now_epoch_ms / 86_400_000) as i64;
    PROVIDERS
        .iter()
        .filter(|p| {
            let days = days_from_ymd(p.reviewed_at).unwrap_or(now_days);
            now_days - days > STALE_DAYS
        })
        .collect()
}

/// "YYYY-MM-DD" → 距 1970 的天数（civil date 算法，无外部依赖）
fn days_from_ymd(s: &str) -> Option<i64> {
    let mut it = s.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    // Howard Hinnant's days_from_civil
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// 汇总视图：plan 关键信息 + 峰谷 + 复核状态（面板/调试用）
pub fn summary(base_url: &str) -> Option<String> {
    let info = info_for(base_url)?;
    let plan = plans::plan_for(base_url);
    Some(format!(
        "{} | plan={} | quota_api={:?} | peak={} | reviewed={} ({}) | docs={}",
        info.host,
        plan.map(|p| p.key).unwrap_or("-"),
        info.quota_api,
        info.peak
            .map(|p| format!("{:?} x{:.1}", p.ranges, p.off_peak_factor))
            .unwrap_or_else(|| "none".into()),
        info.reviewed_at,
        info.source,
        info.docs_url
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epoch(y: i64, mo: i64, d: i64, h: i64, mi: i64) -> u64 {
        let days = days_from_ymd(&format!("{y:04}-{mo:02}-{d:02}")).unwrap();
        ((days * 24 + h) * 60 + mi) as u64 * 60_000
    }

    #[test]
    fn deepseek_peak_windows() {
        // 2026-10-14 是周三。UTC 01:30 = 北京 09:30 → 高峰（9-12）
        assert_eq!(peak_factor("https://api.deepseek.com", epoch(2026, 10, 14, 1, 30)), 1.0);
        // 北京 13:00 → 非高峰（12-14 空隙）→ 0.5
        assert_eq!(peak_factor("https://api.deepseek.com", epoch(2026, 10, 14, 5, 0)), 0.5);
        // 北京 15:00 → 高峰（14-18）
        assert_eq!(peak_factor("https://api.deepseek.com", epoch(2026, 10, 14, 7, 0)), 1.0);
        // 周六全天非高峰：2026-10-17 是周六，北京 10:00
        assert_eq!(peak_factor("https://api.deepseek.com", epoch(2026, 10, 17, 2, 0)), 0.5);
    }

    #[test]
    fn zhipu_and_zai_peak_windows() {
        // 智谱高峰仅 14-18（北京）→ 北京 10:00 非高峰
        assert_eq!(peak_factor("https://open.bigmodel.cn/api/coding/paas/v4", epoch(2026, 10, 14, 2, 0)), 0.5);
        // 北京 15:00 → 高峰
        assert_eq!(peak_factor("https://open.bigmodel.cn/api/coding/paas/v4", epoch(2026, 10, 14, 7, 0)), 1.0);
        // z.ai 同构
        assert_eq!(peak_factor("https://api.z.ai/api/coding/paas/v4", epoch(2026, 10, 14, 7, 0)), 1.0);
    }

    #[test]
    fn no_peak_providers_are_flat() {
        assert_eq!(peak_factor("https://opencode.ai/zen/go/v1", epoch(2026, 10, 14, 7, 0)), 1.0);
        assert_eq!(peak_factor("https://ark.cn-beijing.volces.com/api/coding/v3", epoch(2026, 10, 14, 7, 0)), 1.0);
        // 自建端点：无信息 → 平峰
        assert_eq!(peak_factor("http://127.0.0.1:9999/v1", epoch(2026, 10, 14, 7, 0)), 1.0);
    }

    #[test]
    fn price_sheets_and_effective_price() {
        // 空闲档（北京 13:00 = UTC 05:00 → 非高峰）
        let off = epoch(2026, 10, 14, 5, 0);
        // 命中率高时有效输入价趋近命中价
        let hi_hit = effective_price_per_mtok("https://api.deepseek.com", "deepseek-flash", 0.95, 0.3, off).unwrap();
        let lo_hit = effective_price_per_mtok("https://api.deepseek.com", "deepseek-flash", 0.0, 0.3, off).unwrap();
        assert!(hi_hit < lo_hit, "cache hit lowers cost: {hi_hit} < {lo_hit}");
        // flash 比 pro 便宜
        let pro = effective_price_per_mtok("https://api.deepseek.com", "deepseek-v4-pro", 0.5, 0.3, off).unwrap();
        assert!(hi_hit < pro);
        // 高峰档（北京 10:00）是空闲档两倍
        let peak = epoch(2026, 10, 14, 2, 0);
        let p_peak = effective_price_per_mtok("https://api.deepseek.com", "deepseek-flash", 0.5, 0.3, peak).unwrap();
        let p_off = effective_price_per_mtok("https://api.deepseek.com", "deepseek-flash", 0.5, 0.3, off).unwrap();
        assert!((p_peak / p_off - 2.0).abs() < 0.01, "peak doubles: {p_peak}/{p_off}");
        // 套餐制 provider 无按量价目（成本走 plans 配额口径）
        assert!(price_for("https://open.bigmodel.cn/api/coding/paas/v4", "glm-5.3").is_none());
    }

    #[test]
    fn info_lookup_by_host_and_review_due() {
        assert!(info_for("https://api.deepseek.com/v1").is_some());
        assert!(info_for("https://open.bigmodel.cn/api/coding/paas/v4").is_some());
        assert!(info_for("https://unknown.example.com/v1").is_none());
        // 复核清单：把 now 推到核对日期 31 天后 → 至少 deepseek 在列
        let later = epoch(2026, 11, 20, 0, 0);
        let due = review_due(later);
        assert!(due.iter().any(|p| p.host == "api.deepseek.com"));
        // 刚核对完（同日）→ 不在列
        let due_now = review_due(epoch(2026, 10, 14, 0, 0));
        assert!(due_now.iter().all(|p| p.reviewed_at != "2026-10-14"));
    }
}
