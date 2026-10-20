//! Provider 用量的进程内增量索引（轻量与快速的核心路径，2026-10-19 实证驱动）。
//!
//! 事故形态：`/api/providers` 被面板 3s 轮询，原实现在 5s TTL 过期时把事件文件
//! 尾部（轮转阈值 32MB，cap 64MB）整读成 String 再逐行 serde 解析——且同一
//! 轮询里算两个窗口（24h tokens + 5h credits）等于扫两遍。网关物理内存峰值
//! 841MB、常态 RSS 145–165MB，malloc 大块不还页。
//!
//! 事件文件只追加、每行 `writeln!` 完整落盘（见 evolve-memory events.rs）→
//! 增量索引 O(新增字节)：首扫一次回补 24h，此后每次 tick 只读上次 offset 之后的
//! 新行；内存只留 24h 内 chat 事件的精简记录（ts/model/status/tokens），求和
//! 是纯内存遍历。文件轮转（rename → 新文件从 0 起）由 `len < offset` 探出并
//! 重建——新文件通常很小，重扫便宜。

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{LazyLock, Mutex};

/// 最近一次 chat 请求结果（`/api/live` 下发 → 托盘图标失败态；取代托盘
/// 自行轮询 `/api/events` 拉 60 条重 JSON 的老路径）
#[derive(Clone, Debug, PartialEq)]
pub struct LastChat {
    pub decision_id: String,
    pub status: i64,
    pub ts: i64,
}

/// 一条 chat 事件的精简投影：只留求和/展示要用的字段
#[derive(Clone, Debug)]
struct Rec {
    ts: i64,
    model: String,
    pt: f64,
    ct: f64,
    cached: f64,
}

struct Index {
    /// 事件文件内已消费到的**绝对字节偏移**（总是行边界：上一完整行的 '\n' 之后）
    offset: u64,
    /// 首扫完成（24h 回补已入 records）
    warm: bool,
    /// 首扫时的文件 inode——轮转（rename → 新文件）即使新文件长度恰好追平旧
    /// offset 也能被探出（纯长度比较在同长文件场景会漏）
    ino: u64,
    records: VecDeque<Rec>,
    last_chat: Option<LastChat>,
}

static IDX: LazyLock<Mutex<Index>> = LazyLock::new(|| {
    Mutex::new(Index {
        offset: 0,
        warm: false,
        ino: 0,
        records: VecDeque::new(),
        last_chat: None,
    })
});

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 事件尾部读取（带返回内容在文件内的**绝对起始偏移**）——增量索引的坐标锚。
/// 只读最后 `max_bytes`；首行可能残缺（截断点）→ 丢弃到第一个换行并右移起点。
fn read_tail_span(path: &Path, max_bytes: u64) -> Option<(u64, Vec<u8>)> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(max_bytes);
    if start > 0 {
        f.seek(SeekFrom::Start(start)).ok()?;
    }
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    if start > 0 {
        return match buf.iter().position(|&b| b == b'\n') {
            // 首个完整行行首的绝对偏移
            Some(i) => {
                let abs = start + i as u64 + 1;
                buf.drain(..=i);
                Some((abs, buf))
            }
            // 截断窗口里一行都不完整 → 空
            None => Some((start + buf.len() as u64, Vec::new())),
        };
    }
    Some((0, buf))
}

/// 自 `offset` 起读到 EOF（增量路径专用；量级 = 自上次以来的新增字节）
fn read_from(path: &Path, offset: u64) -> Option<Vec<u8>> {
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// 解析单行事件 → Rec（只收 chat；无 ts/无 chosen 的跳过）
fn parse_rec(line: &str) -> Option<Rec> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("kind").and_then(|x| x.as_str()) != Some("chat") {
        return None;
    }
    let ts = v
        .get("ts")
        .and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64)))?;
    let model = v.get("chosen").and_then(|x| x.as_str())?.to_string();
    let num = |k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
    Some(Rec {
        ts,
        model,
        pt: num("prompt_tokens"),
        ct: num("completion_tokens"),
        cached: num("cached_tokens"),
    })
}

fn parse_last_chat(line: &str) -> Option<LastChat> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("kind").and_then(|x| x.as_str()) != Some("chat") {
        return None;
    }
    let ts = v
        .get("ts")
        .and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64)))?;
    Some(LastChat {
        decision_id: v
            .get("decision_id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        status: v
            .get("status")
            .and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64)))
            .unwrap_or(0),
        ts,
    })
}

/// 消费一段字节缓冲：只吃**完整行**（结尾无 '\n' 的残行留给下一轮，写入侧
/// writeln! 单 syscall 追加 → 残行必然尚未写完）。字节偏移在原始字节域推进，
/// UTF-8 修复只作用于**已截好的完整行段**，不会漂移坐标。
/// 返回消费到的绝对偏移（= start + 完整行总字节数）。
fn consume(idx: &mut Index, start: u64, buf: &[u8], now_ms: u64) -> u64 {
    let complete_end = match buf.iter().rposition(|&b| b == b'\n') {
        Some(i) => i + 1,
        None => return start,
    };
    let text = String::from_utf8_lossy(&buf[..complete_end]);
    let cutoff = now_ms as i64 - 24 * 3600 * 1000 - 60_000; // 略放宽防边界抖动
    for line in text.lines() {
        if let Some(rec) = parse_rec(line)
            && rec.ts >= cutoff
        {
            idx.records.push_back(rec);
        }
        if let Some(lc) = parse_last_chat(line)
            && idx.last_chat.as_ref().is_none_or(|c| lc.ts >= c.ts)
        {
            idx.last_chat = Some(lc);
        }
    }
    while idx.records.front().is_some_and(|r| r.ts < cutoff) {
        idx.records.pop_front();
    }
    start + complete_end as u64
}

/// 首扫：24h 回补。窗口 2MB 起按需翻倍（cap 64MB 兜底；轮转前 ~32MB 文件
/// 一两次即可覆盖），最老行早于 cutoff 即停——与被删的 read_tail_until 同语义。
fn warmup(idx: &mut Index, path: &Path, len: u64, now_ms: u64) {
    let cutoff = now_ms as i64 - 24 * 3600 * 1000;
    let mut size: u64 = 2 * 1024 * 1024;
    loop {
        let Some((start_abs, buf)) = read_tail_span(path, size) else {
            return;
        };
        let whole_file = size >= len;
        let oldest_ts = String::from_utf8_lossy(&buf)
            .lines()
            .next()
            .and_then(parse_rec)
            .map(|r| r.ts)
            .unwrap_or(0);
        if whole_file || (oldest_ts > 0 && oldest_ts < cutoff) || size >= 64 * 1024 * 1024 {
            let consumed = consume(idx, start_abs, &buf, now_ms);
            idx.offset = idx.offset.max(consumed);
            idx.warm = true;
            return;
        }
        size = (size * 2).min(64 * 1024 * 1024);
    }
}

fn ingest(path: &Path, now_ms: u64) {
    let mut idx = IDX.lock().unwrap_or_else(|p| p.into_inner());
    let Some(md) = std::fs::metadata(path).ok() else {
        return;
    };
    let len = md.len();
    #[cfg(unix)]
    let ino = {
        use std::os::unix::fs::MetadataExt;
        md.ino()
    };
    #[cfg(not(unix))]
    let ino = len;
    // 轮转（inode 变了）或截断（文件比已消费偏移还短）→ 全量重建
    // （新文件小，重扫便宜）
    if idx.warm && (idx.ino != ino || len < idx.offset) {
        idx.offset = 0;
        idx.warm = false;
        idx.records.clear();
        idx.last_chat = None;
    }
    if !idx.warm {
        warmup(&mut idx, path, len, now_ms);
        idx.ino = ino;
        return;
    }
    if len == idx.offset {
        return;
    }
    let Some(buf) = read_from(path, idx.offset) else {
        return;
    };
    let from = idx.offset;
    let consumed = consume(&mut idx, from, &buf, now_ms);
    idx.offset = idx.offset.max(consumed);
}

/// 增量扫描入口（阻塞；量级 = 新增字节）。`/api/live`（1s）与 `/api/providers`
/// （3s）轮询自然驱动——面板不开也由 /api/live 的 1s tick 保新鲜。
pub fn refresh(events: &evolve_memory::events::EventLog) {
    let Some(path) = events.path() else { return };
    ingest(path, now_ms());
}

/// 只读访问器（无 IO）：最近一次 chat 结果
pub fn last_chat() -> Option<LastChat> {
    IDX.lock()
        .unwrap_or_else(|p| p.into_inner())
        .last_chat
        .clone()
}

/// Provider 三联聚合（无 IO，纯内存遍历）：
/// `(24h tokens, 各 provider 最近使用 ts, 5h credits)`——口径与被替换的
/// `tokens_24h_by_provider` / `usage_5h_by_provider` 完全一致：
/// - model → provider 走目录快照；目录外模型不计
/// - credits 仅 5h 窗口内、且模型仍在目录（plan_credits_used 需要 base_url）
#[allow(clippy::type_complexity)]
pub fn provider_usage(
    snap: &[evolve_core::types::ModelRecord],
    now_ms: u64,
) -> (
    HashMap<String, u64>,
    HashMap<String, i64>,
    HashMap<String, f64>,
) {
    let idx = IDX.lock().unwrap_or_else(|p| p.into_inner());
    let prov_of_model: HashMap<&str, &str> = snap
        .iter()
        .map(|m| (m.id.as_str(), m.provider.as_str()))
        .collect();
    let rec_of_model: HashMap<&str, &evolve_core::types::ModelRecord> =
        snap.iter().map(|m| (m.id.as_str(), m)).collect();
    let cutoff24 = now_ms as i64 - 24 * 3600 * 1000;
    let cutoff5 = now_ms as i64 - 5 * 3600 * 1000;
    let mut tok24: HashMap<String, u64> = HashMap::new();
    let mut last_used: HashMap<String, i64> = HashMap::new();
    let mut credits5: HashMap<String, f64> = HashMap::new();
    for r in &idx.records {
        if r.ts < cutoff24 {
            continue;
        }
        let Some(&prov) = prov_of_model.get(r.model.as_str()) else {
            continue;
        };
        *tok24.entry(prov.to_string()).or_insert(0) += (r.pt + r.ct) as u64;
        let e = last_used.entry(prov.to_string()).or_insert(0);
        if r.ts > *e {
            *e = r.ts;
        }
        if r.ts >= cutoff5
            && let Some(mm) = rec_of_model.get(r.model.as_str())
        {
            *credits5.entry(prov.to_string()).or_insert(0.0) +=
                evolve_core::plans::plan_credits_used(&mm.base_url, &r.model, r.pt, r.cached, r.ct);
        }
    }
    (tok24, last_used, credits5)
}

#[cfg(test)]
mod tests {
    use super::*;
    use evolve_core::types::ModelRecord;

    /// 全局 IDX 共享 → 测试必须串行（并行会互相 reset/投喂）
    static TEST_GUARD: Mutex<()> = Mutex::new(());

    fn temp_events() -> (std::path::PathBuf, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("events.jsonl");
        (path, dir)
    }

    fn reset() {
        let mut idx = IDX.lock().unwrap();
        idx.offset = 0;
        idx.warm = false;
        idx.ino = 0;
        idx.records.clear();
        idx.last_chat = None;
    }

    fn ev(ts: i64, model: &str, status: i64) -> String {
        serde_json::json!({
            "kind": "chat", "ts": ts, "chosen": model, "status": status,
            "decision_id": format!("d{ts}"),
            "prompt_tokens": 100, "completion_tokens": 50, "cached_tokens": 20,
        })
        .to_string()
    }

    fn snap(prov: &str) -> Vec<ModelRecord> {
        vec![ModelRecord {
            id: format!("{prov}/m1"),
            provider: prov.to_string(),
            base_url: "https://api.example".into(),
            ..Default::default()
        }]
    }

    fn append(path: &std::path::Path, s: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        write!(f, "{s}").unwrap();
    }

    #[test]
    fn warmup_then_incremental_counts_once() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        let (path, _dir) = temp_events();
        let now = 1_700_000_000_000u64;
        let n = now as i64;
        append(
            &path,
            &format!(
                "{}\n{}\n",
                ev(n - 1000, "p/m1", 200),
                ev(n - 500, "p/m1", 500)
            ),
        );
        reset();
        ingest(&path, now);
        let (tok, last_used, credits) = provider_usage(&snap("p"), now);
        assert_eq!(tok["p"], 300);
        assert_eq!(last_used["p"], n - 500);
        assert!(credits.contains_key("p"));
        // 追加一条 → 只补新增，旧的不重复计
        append(&path, &format!("{}\n", ev(n - 100, "p/m1", 200)));
        ingest(&path, now);
        let (tok, ..) = provider_usage(&snap("p"), now);
        assert_eq!(tok["p"], 450);
        // last_chat 跟到最新一条
        let lc = last_chat().unwrap();
        assert_eq!(lc.ts, n - 100);
        assert_eq!(lc.status, 200);
    }

    #[test]
    fn non_chat_and_foreign_models_excluded() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        let (path, _dir) = temp_events();
        let now = 1_700_000_000_000u64;
        let n = now as i64;
        append(
            &path,
            &format!(
                "{}\n{}\n{}\n",
                ev(n - 1000, "other/m9", 200),
                serde_json::json!({"kind": "reload", "ts": now - 900}).to_string(),
                ev(n - 800, "p/m1", 200)
            ),
        );
        reset();
        ingest(&path, now);
        let (tok, last_used, credits) = provider_usage(&snap("p"), now);
        assert_eq!(tok["p"], 150);
        assert_eq!(last_used.len(), 1);
        assert!(credits.contains_key("p"));
    }

    #[test]
    fn torn_tail_line_waited_for() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        let (path, _dir) = temp_events();
        let now = 1_700_000_000_000u64;
        let n = now as i64;
        append(&path, &format!("{}\n", ev(n - 1000, "p/m1", 200)));
        reset();
        ingest(&path, now);
        // 写半行（无 '\n'）→ 不消费；补全后一次性计入
        let full = ev(n - 100, "p/m1", 200);
        let mid = full.find(",\"status\"").expect("split anchor");
        let (head, tail) = full.split_at(mid);
        append(&path, head);
        ingest(&path, now);
        let (tok, ..) = provider_usage(&snap("p"), now);
        assert_eq!(tok["p"], 150, "残行不计");
        append(&path, &format!("{tail}\n"));
        ingest(&path, now);
        let (tok, ..) = provider_usage(&snap("p"), now);
        assert_eq!(tok["p"], 300, "补全后计一次且仅一次");
    }

    #[test]
    fn rotation_resets_and_rebuilds() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        let (path, _dir) = temp_events();
        let now = 1_700_000_000_000u64;
        let n = now as i64;
        append(&path, &format!("{}\n", ev(n - 1000, "p/m1", 200)));
        reset();
        ingest(&path, now);
        assert!(IDX.lock().unwrap().warm);
        // 轮转：旧文件改名，新文件从 0 起
        std::fs::rename(&path, path.with_extension("jsonl.1")).unwrap();
        append(&path, &format!("{}\n", ev(n - 50, "p/m1", 200)));
        ingest(&path, now);
        let (tok, last_used, ..) = provider_usage(&snap("p"), now);
        assert_eq!(tok["p"], 150, "重建后只算新文件");
        assert_eq!(last_used["p"], n - 50);
    }

    #[test]
    fn prune_drops_records_older_than_24h() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        let (path, _dir) = temp_events();
        let now = 1_700_000_000_000u64;
        let n = now as i64;
        append(
            &path,
            &format!(
                "{}\n{}\n",
                ev(n - 25 * 3600 * 1000, "p/m1", 200),
                ev(n - 1000, "p/m1", 200)
            ),
        );
        reset();
        ingest(&path, now);
        let (tok, last_used, credits) = provider_usage(&snap("p"), now);
        assert_eq!(tok["p"], 150);
        // last_used 由 records 推导 → 同样只反映 24h 窗口内
        assert_eq!(last_used["p"], n - 1000);
        assert!(credits.contains_key("p"));
    }

    #[test]
    fn empty_and_missing_file_are_inert() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        let (path, _dir) = temp_events();
        reset();
        ingest(&path, 1); // 文件不存在 → 不 panic、状态不动
        assert!(!IDX.lock().unwrap().warm);
        append(&path, "");
        ingest(&path, 1_700_000_000_000);
        assert!(IDX.lock().unwrap().warm);
        let (tok, last_used, credits) = provider_usage(&snap("p"), 1_700_000_000_000);
        assert!(tok.is_empty() && last_used.is_empty() && credits.is_empty());
    }
}
