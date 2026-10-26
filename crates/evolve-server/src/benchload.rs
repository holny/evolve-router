//! ⑧ 网关负载基准（TokenRouter 借鉴项 ①，arXiv:2610.12242 §5 评价范式）：
//! 并发扫描 → 吞吐 / 单用户速度 / TTFT 三条曲线。
//!
//! 论文最可借鉴的不是 token 路由本身，而是它的评价方式：只报平均延迟说服力
//! 有限，"吞吐–并发"与"单用户速度–并发"的权衡曲线（SLO 工作点）才是系统
//! 级证据。本工具对**运行中的网关**打 OpenAI 兼容请求（model=auto 走完整
//! 路由链），每个并发档位产出一行指标，落 result.csv + summary.json 并打印
//! 对比表。与隐藏子命令 `mock-upstream` 组合可在无真实上游时做端到端自检。

use anyhow::Context as _;
use futures::StreamExt;
use serde::Serialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 单个并发档位的汇总指标（CSV / JSON 同构）
#[derive(Debug, Clone, Serialize)]
pub struct LevelMetrics {
    pub concurrency: u32,
    pub requests: u32,
    pub ok: u32,
    pub failed: u32,
    /// 聚合吞吐（输出 token / 秒）：Σ out_tokens / 档位墙钟时间
    pub throughput_tok_s: f64,
    /// 单用户速度：每请求 out_tokens/(该请求总耗时) 的均值（token/秒/请求）
    pub user_speed_mean_tok_s: f64,
    pub ttft_mean_ms: f64,
    pub ttft_p50_ms: f64,
    pub ttft_p95_ms: f64,
    pub total_ms_mean: f64,
    pub out_tokens_total: u64,
    pub wall_s: f64,
    /// 质量级联触发次数（响应头 x-ev-quality-cascade: 1）
    pub cascades: u32,
    /// 兜底链换手次数（响应头 x-ev-fallback-from 出现）
    pub fallbacks: u32,
    /// 单用户速度是否达到 SLO（--slo 未设置时为 None）
    pub slo_ok: Option<bool>,
}

impl LevelMetrics {
    pub const CSV_HEADER: &'static str = "concurrency,requests,ok,failed,throughput_tok_s,\
user_speed_mean_tok_s,ttft_mean_ms,ttft_p50_ms,ttft_p95_ms,total_ms_mean,out_tokens_total,\
wall_s,cascades,fallbacks,slo_ok";

    pub fn csv_row(&self) -> String {
        let slo = match self.slo_ok {
            Some(true) => "pass",
            Some(false) => "miss",
            None => "-",
        };
        format!(
            "{},{},{},{},{:.2},{:.2},{:.0},{:.0},{:.0},{:.0},{},{:.2},{},{},{}",
            self.concurrency,
            self.requests,
            self.ok,
            self.failed,
            self.throughput_tok_s,
            self.user_speed_mean_tok_s,
            self.ttft_mean_ms,
            self.ttft_p50_ms,
            self.ttft_p95_ms,
            self.total_ms_mean,
            self.out_tokens_total,
            self.wall_s,
            self.cascades,
            self.fallbacks,
            slo,
        )
    }
}

/// 最近秩分位（nearest-rank）：p∈[0,1]，返回第 ⌈p·n⌉ 小的值；空序列返回 None
pub fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let idx = ((p * sorted.len() as f64).ceil() as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    Some(sorted[idx])
}

/// 单次请求的观测（失败请求只保 status/total，不参与速度与 TTFT 统计）。
/// 耗时用 f64 毫秒（as_secs_f64 换算）——本地/极速上游下 as_millis 取整
/// 会把总耗时打成 0，单用户速度随之全 0（实测 mock 上游并发 4 复现）
#[derive(Debug, Default, Clone)]
struct ReqOutcome {
    ok: bool,
    #[allow(dead_code)]
    status: u16,
    ttft_ms: Option<f64>,
    total_ms: f64,
    out_tokens: u64,
    cascade: bool,
    fallback: bool,
}

/// SSE 流累积器：从 data 帧提取 model（首个）、usage（末个）、正文长度，
/// 并侦测 error 帧（网关截断注入帧与上游错误帧都带顶层 error——HTTP 状态
/// 已发出无法更改，error 帧是客户端侧唯一的失败信号）
#[derive(Debug, Default)]
struct SseAcc {
    model: Option<String>,
    completion_tokens: Option<u64>,
    content_chars: u64,
    saw_error: bool,
    /// byte 级行缓冲：只在完整行边界解码——多字节 UTF-8（如 CJK 正文）跨
    /// chunk 拆分时逐 chunk lossy 解码会把 1 个字符膨胀成 2 个 U+FFFD，
    /// /4 估算随之虚高（同 stream.rs 的 BUG-6 修复）
    byte_buf: Vec<u8>,
}

impl SseAcc {
    fn push_chunk(&mut self, chunk: &[u8]) {
        self.byte_buf.extend_from_slice(chunk);
        while let Some(pos) = self.byte_buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = self.byte_buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes);
            self.process_line(line.trim_end());
        }
    }

    fn process_line(&mut self, line: &str) {
        let Some(data) = line.strip_prefix("data:") else { return };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else { return };
        if v.get("error").is_some() {
            self.saw_error = true;
        }
        if self.model.is_none()
            && let Some(m) = v.get("model").and_then(|m| m.as_str())
        {
            self.model = Some(m.to_string());
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            // OpenAI 形状优先，anthropic 跨协议形状兜底（与 stream.rs 同式）
            let c = u
                .get("completion_tokens")
                .and_then(|x| x.as_u64())
                .or_else(|| u.get("output_tokens").and_then(|x| x.as_u64()));
            if c.is_some() {
                self.completion_tokens = c;
            }
        }
        if let Some(t) = v
            .pointer("/choices/0/delta/content")
            .and_then(|c| c.as_str())
        {
            self.content_chars += t.chars().count() as u64;
        }
    }

    /// 输出 token：usage 优先；缺 usage 时按内容长度 /4 估算（不把 0 当 0）
    fn out_tokens(&self) -> u64 {
        self.completion_tokens
            .unwrap_or(self.content_chars / 4)
    }
}

/// 扫描参数（CLI `bench-load` 子命令的解析结果）
#[derive(Debug, Clone)]
pub struct BenchOpts {
    pub url: String,
    pub model: String,
    pub concurrency: Vec<u32>,
    pub requests: u32,
    pub max_tokens: u32,
    pub stream: bool,
    pub slo: f64,
    pub prompts: Arc<Vec<String>>,
    pub out: PathBuf,
    pub timeout_s: u64,
}

pub fn default_prompts() -> Vec<String> {
    vec![
        "Refactor this Rust function to return Result instead of panicking: fn get(i: usize) -> u8 { VEC[i] }".into(),
        "Explain the difference between optimistic and pessimistic locking in two sentences.".into(),
        "Write a SQL query: top 5 customers by total order amount in the last 30 days.".into(),
        "Find the bug: for i in 0..v.len() { if v[i] == target { v.remove(i); } }".into(),
        "Summarize the trade-offs of a hash map versus a B-tree index for range queries.".into(),
        "Design a rate limiter for 100 requests/minute per user; describe the data structures.".into(),
        "What is 17 * 23? Show the intermediate steps.".into(),
        "Rewrite this sentence to be concise: 'We are in receipt of your communication dated the 3rd.'".into(),
    ]
}

fn endpoint(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    if trimmed.ends_with("/chat/completions") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1/chat/completions")
    }
}

async fn one_request(
    client: &reqwest::Client,
    endpoint: &str,
    model: &str,
    max_tokens: u32,
    stream: bool,
    prompt: &str,
) -> ReqOutcome {
    let mut body = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": max_tokens,
    });
    if stream {
        body["stream"] = json!(true);
        // 请求上游在末帧带 usage（OpenAI stream_options；不识别的 provider
        // 会忽略或走网关翻译，缺 usage 时有估算兜底；个别严格 provider 会
        // 400 拒绝——此时用 --no-stream）
        body["stream_options"] = json!({"include_usage": true});
    }
    let started = Instant::now();
    let resp = match client.post(endpoint).json(&body).send().await {
        Ok(r) => r,
        Err(_) => {
            return ReqOutcome { total_ms: ms(started), ..Default::default() }
        }
    };
    let cascade = resp
        .headers()
        .get("x-ev-quality-cascade")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim() == "1")
        .unwrap_or(false);
    let fallback = resp.headers().contains_key("x-ev-fallback-from");
    let status = resp.status().as_u16();
    let mut acc = SseAcc::default();
    let mut ttft: Option<f64> = None;
    let ok_base = status == 200;
    let parse_result = if stream {
        let mut stream = resp.bytes_stream();
        let mut first_err: Option<String> = None;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(b) => {
                    if ttft.is_none() {
                        ttft = Some(ms(started));
                    }
                    acc.push_chunk(&b);
                }
                Err(e) => {
                    first_err = Some(e.to_string());
                    break;
                }
            }
        }
        ok_base && first_err.is_none()
    } else {
        match resp.bytes().await {
            Ok(b) => {
                if ttft.is_none() {
                    ttft = Some(ms(started));
                }
                acc.push_chunk(&b);
                // 非流式是整段 JSON，不是 SSE——直接按 JSON 解析
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) {
                    if v.get("error").is_some() {
                        acc.saw_error = true;
                    }
                    acc.model = v
                        .get("model")
                        .and_then(|m| m.as_str())
                        .map(String::from)
                        .or(acc.model.take());
                    if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                        acc.completion_tokens = u
                            .get("completion_tokens")
                            .and_then(|x| x.as_u64())
                            .or_else(|| u.get("output_tokens").and_then(|x| x.as_u64()))
                            .or(acc.completion_tokens.take());
                    }
                    acc.content_chars = v
                        .pointer("/choices/0/message/content")
                        .and_then(|c| c.as_str())
                        .map(|c| c.chars().count() as u64)
                        .unwrap_or(acc.content_chars);
                }
                ok_base
            }
            Err(_) => false,
        }
    };
    // 失败判据：HTTP 状态、流中断、error 帧（截断注入帧/上游错误帧——HTTP 200
    // 已发出时这是唯一信号）三者在案；再以"有无内容或 usage"兜底
    let ok = parse_result && !acc.saw_error && (acc.out_tokens() > 0 || acc.model.is_some());
    ReqOutcome {
        ok,
        status,
        ttft_ms: ttft,
        total_ms: ms(started),
        out_tokens: if ok { acc.out_tokens() } else { 0 },
        cascade,
        fallback,
    }
}

/// 亚毫秒精度耗时（毫秒，f64）：as_millis 取整会把极速请求打成 0
fn ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

async fn run_level(client: &reqwest::Client, opts: &BenchOpts, concurrency: u32) -> LevelMetrics {
    let total = opts.requests;
    // 实际并发不能超过请求总数——按有效值上报，避免 CSV 里出现"并发 16 实为 8"
    let effective = concurrency.min(total.max(1));
    let next = Arc::new(AtomicU32::new(0));
    let endpoint = endpoint(&opts.url);
    let started = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..effective {
        let client = client.clone();
        let endpoint = endpoint.clone();
        let next = next.clone();
        let prompts = opts.prompts.clone();
        let (model, max_tokens, stream) = (opts.model.clone(), opts.max_tokens, opts.stream);
        handles.push(tokio::spawn(async move {
            let mut outs: Vec<ReqOutcome> = Vec::new();
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= total {
                    break;
                }
                let prompt = prompts[(i as usize) % prompts.len()].clone();
                outs.push(one_request(&client, &endpoint, &model, max_tokens, stream, &prompt).await);
            }
            outs
        }));
    }
    let mut all: Vec<ReqOutcome> = Vec::new();
    for h in handles {
        // worker panic 必须显式失败：静默吞掉会让 ok+failed < requests 且无迹象
        all.extend(h.await.expect("bench worker panicked"));
    }
    let wall_s = started.elapsed().as_secs_f64();
    let ok_rows: Vec<&ReqOutcome> = all.iter().filter(|r| r.ok).collect();
    let ok = ok_rows.len() as u32;
    let mut ttfts: Vec<f64> = ok_rows.iter().filter_map(|r| r.ttft_ms).collect();
    ttfts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let speeds: Vec<f64> = ok_rows
        .iter()
        .filter(|r| r.total_ms > 0.0)
        .map(|r| r.out_tokens as f64 / (r.total_ms / 1000.0))
        .collect();
    let out_tokens_total: u64 = ok_rows.iter().map(|r| r.out_tokens).sum();
    let mean = |v: &[f64]| if v.is_empty() { 0.0 } else { v.iter().sum::<f64>() / v.len() as f64 };
    LevelMetrics {
        concurrency: effective,
        requests: total,
        ok,
        failed: all.len() as u32 - ok,
        throughput_tok_s: if wall_s > 0.0 { out_tokens_total as f64 / wall_s } else { 0.0 },
        user_speed_mean_tok_s: mean(&speeds),
        ttft_mean_ms: ttfts.iter().sum::<f64>() / ttfts.len().max(1) as f64,
        ttft_p50_ms: percentile(&ttfts, 0.50).unwrap_or(0.0),
        ttft_p95_ms: percentile(&ttfts, 0.95).unwrap_or(0.0),
        total_ms_mean: all.iter().map(|r| r.total_ms).sum::<f64>() / all.len().max(1) as f64,
        out_tokens_total,
        wall_s,
        cascades: all.iter().filter(|r| r.cascade).count() as u32,
        fallbacks: all.iter().filter(|r| r.fallback).count() as u32,
        slo_ok: if opts.slo > 0.0 {
            Some(mean(&speeds) >= opts.slo)
        } else {
            None
        },
    }
}

fn print_table(rows: &[LevelMetrics]) {
    println!(
        "\n{:<6} {:<6} {:<6} {:<13} {:<13} {:<10} {:<8} {:<8} {:<6} {:<6} SLO",
        "并发", "请求", "成功", "吞吐(tok/s)", "单用户(tok/s)", "TTFT均值", "p50", "p95",
        "级联", "兜底"
    );
    for r in rows {
        let slo = match r.slo_ok {
            Some(true) => "pass".to_string(),
            Some(false) => "MISS".to_string(),
            None => "-".to_string(),
        };
        println!(
            "{:<6} {:<6} {:<6} {:<13.2} {:<13.2} {:<10.0} {:<8.0} {:<8.0} {:<6} {:<6} {}",
            r.concurrency, r.requests, r.ok, r.throughput_tok_s, r.user_speed_mean_tok_s,
            r.ttft_mean_ms, r.ttft_p50_ms, r.ttft_p95_ms, r.cascades, r.fallbacks, slo
        );
    }
    println!(
        "\n吞吐–单用户速度权衡（TokenRouter Fig.1b 范式）：并发越高吞吐越高、\
单用户速度越低；--slo 标出满足单用户速度要求的最大并发档。"
    );
}

/// 入口：构建 tokio runtime 后执行扫描（CLI 调用）
pub fn run(opts: BenchOpts) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(run_async(opts))
}

/// 扫描全部并发档位，打印对比表并落盘 result.csv + summary.json
pub async fn run_async(opts: BenchOpts) -> anyhow::Result<()> {
    anyhow::ensure!(!opts.concurrency.is_empty(), "--concurrency 至少一个档位");
    anyhow::ensure!(opts.requests > 0, "--requests 至少 1");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(opts.timeout_s))
        .build()
        .context("构建 HTTP 客户端失败")?;
    let mut rows = Vec::new();
    for c in &opts.concurrency {
        println!("▶ 并发 {c}：{} 个请求 …", opts.requests);
        rows.push(run_level(&client, &opts, *c).await);
    }
    print_table(&rows);

    std::fs::create_dir_all(&opts.out).context("创建输出目录失败")?;
    let csv = {
        let mut s = String::from(LevelMetrics::CSV_HEADER);
        s.push('\n');
        for r in &rows {
            s.push_str(&r.csv_row());
            s.push('\n');
        }
        s
    };
    let csv_path = opts.out.join("result.csv");
    std::fs::write(&csv_path, csv).context("写 result.csv 失败")?;
    let summary = json!({
        "url": opts.url,
        "model": opts.model,
        "stream": opts.stream,
        "max_tokens": opts.max_tokens,
        "slo_tok_s": opts.slo,
        "levels": rows,
    });
    let json_path = opts.out.join("summary.json");
    std::fs::write(&json_path, serde_json::to_vec_pretty(&summary)?)?;
    println!("已写入 {} 与 {}", csv_path.display(), json_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_nearest_rank() {
        let v: Vec<f64> = (1..=100).map(|x| x as f64).collect();
        assert_eq!(percentile(&v, 0.5), Some(50.0));
        assert_eq!(percentile(&v, 0.95), Some(95.0));
        assert_eq!(percentile(&v, 0.0), Some(1.0));
        assert_eq!(percentile(&[], 0.5), None);
        assert_eq!(percentile(&[7.0], 0.99), Some(7.0));
        // 亚毫秒值不被吞掉（极速上游场景）
        assert_eq!(percentile(&[0.2, 0.4], 0.5), Some(0.2));
    }

    #[test]
    fn sse_acc_parses_model_usage_content() {
        let mut acc = SseAcc::default();
        acc.push_chunk(b"data: {\"model\":\"glm-5.3\",\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n");
        acc.push_chunk(b"data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}],\"usage\":{\"completion_tokens\":4}}\n\n");
        acc.push_chunk(b"data: [DONE]\n\n");
        assert_eq!(acc.model.as_deref(), Some("glm-5.3"));
        assert_eq!(acc.completion_tokens, Some(4));
        assert_eq!(acc.content_chars, 5);
        assert_eq!(acc.out_tokens(), 4);
    }

    #[test]
    fn sse_acc_falls_back_to_estimate_without_usage() {
        let mut acc = SseAcc::default();
        acc.push_chunk(b"data: {\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"abcd\"}}]}\n\n");
        assert_eq!(acc.completion_tokens, None);
        assert_eq!(acc.out_tokens(), 1, "无 usage：4 字符 / 4 = 1 估算");
    }

    /// 多字节 UTF-8 跨 chunk 拆分不得膨胀字符数（stream.rs BUG-6 同款修复）：
    /// 逐 chunk lossy 解码会把 1 个拆断的字符算成 2 个 U+FFFD
    #[test]
    fn sse_multibyte_split_across_chunks_survives() {
        let full = "data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\n\n";
        let bytes = full.as_bytes();
        let split = full.find('你').unwrap() + 1; // 劈在「你」三字节中间
        let mut acc = SseAcc::default();
        acc.push_chunk(&bytes[..split]);
        acc.push_chunk(&bytes[split..]);
        assert_eq!(acc.content_chars, 2, "跨 chunk 的多字节字符不应膨胀或丢失");
    }

    /// error 帧（网关截断注入帧/上游错误帧）必须置失败标记——
    /// HTTP 200 已发出时这是客户端侧唯一的失败信号
    #[test]
    fn sse_error_frame_flagged() {
        let mut acc = SseAcc::default();
        acc.push_chunk(b"data: {\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n");
        acc.push_chunk(b"data: {\"error\":{\"message\":\"upstream stream truncated\"}}\n\ndata: [DONE]\n\n");
        assert!(acc.saw_error, "顶层 error 帧应置 saw_error");
        assert_eq!(acc.content_chars, 2);
    }

    #[test]
    fn csv_row_is_stable() {
        let m = LevelMetrics {
            concurrency: 4,
            requests: 8,
            ok: 7,
            failed: 1,
            throughput_tok_s: 120.5,
            user_speed_mean_tok_s: 30.1,
            ttft_mean_ms: 310.0,
            ttft_p50_ms: 300.0,
            ttft_p95_ms: 480.0,
            total_ms_mean: 900.0,
            out_tokens_total: 964,
            wall_s: 8.0,
            cascades: 1,
            fallbacks: 2,
            slo_ok: Some(false),
        };
        assert_eq!(
            m.csv_row(),
            "4,8,7,1,120.50,30.10,310,300,480,900,964,8.00,1,2,miss"
        );
    }

    #[test]
    fn endpoint_normalization() {
        assert_eq!(
            endpoint("http://127.0.0.1:8787"),
            "http://127.0.0.1:8787/v1/chat/completions"
        );
        assert_eq!(
            endpoint("http://h/v1/chat/completions/"),
            "http://h/v1/chat/completions"
        );
    }

    /// 端到端自检：临时 mock 上游（与 mock-upstream 同形状）跑两个并发档
    #[tokio::test]
    async fn sweep_against_mock_upstream() {
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(|body: axum::body::Bytes| async move {
                let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                let model = v["model"].as_str().unwrap_or("unknown").to_string();
                let sse = format!(
                    "data: {{\"model\":\"{model}\",\"choices\":[{{\"delta\":{{\"content\":\"hello from mock\"}}}}]}}\n\n\
                     data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":4}}}}\n\n\
                     data: [DONE]\n\n"
                );
                axum::http::Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(axum::body::Body::from(sse))
                    .unwrap()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let opts = BenchOpts {
            url: format!("http://{addr}"),
            model: "auto".into(),
            concurrency: vec![1, 2],
            requests: 4,
            max_tokens: 32,
            stream: true,
            slo: 0.0,
            prompts: Arc::new(vec!["ping".into()]),
            out: std::env::temp_dir().join(format!("evo-bench-test-{}", std::process::id())),
            timeout_s: 10,
        };
        run_async(opts).await.expect("sweep should succeed");
        let csv = std::fs::read_to_string(
            std::env::temp_dir().join(format!("evo-bench-test-{}/result.csv", std::process::id())),
        )
        .unwrap();
        // 两个档位各 4 个请求全部成功，吞吐为正
        let rows: Vec<&str> = csv.lines().skip(1).collect();
        assert_eq!(rows.len(), 2);
        for r in rows {
            let cols: Vec<&str> = r.split(',').collect();
            assert_eq!(cols[2], "4", "ok 列应为 4: {r}");
            assert!(cols[4].parse::<f64>().unwrap() > 0.0, "吞吐应为正: {r}");
        }
        server.abort();
    }
}
