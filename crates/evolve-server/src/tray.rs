// evo-router tray 模式（macOS）：单二进制双模式之一。
// 同进程 = 网关（tokio 后台线程，spawn_gateway）+ AppKit 原生菜单栏（NSStatusItem）。
// 数据链路与 web 面板同源：本进程 127.0.0.1:{port} 的同一组 API
// （/api/live /healthz /api/stats?window=24h /api/providers /api/events）。
//
// 线程模型：NSApplication 主线程跑 UI；tokio 多线程池跑网关与轮询；
// 0.25s 主线程 NSTimer 触发轮询（异步发往 tokio）并经 mpsc 通道回收快照渲染。
// 所有 Retained<NSView*> 仅主线程触碰，经 UiRefs 裸指针全局持有（MainPtr）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, AllocAnyThread, ClassType, MainThreadMarker};
use objc2_app_kit::{
    NSAlert, NSApplication, NSApplicationActivationPolicy, NSButton, NSColor,
    NSControlStateValue, NSFont, NSImage, NSLayoutAttribute, NSPopover, NSPopoverBehavior,
    NSProgressIndicator, NSStackView, NSStatusBar, NSStatusItem, NSSwitch, NSTextField,
    NSUserInterfaceLayoutOrientation, NSView, NSWorkspace,
};
use objc2_foundation::{NSArray, NSData, NSObject, NSPoint, NSRect, NSSize, NSString, NSTimer, NSUserDefaults};
use serde_json::Value;

use crate::config;

// ── 全局状态 ──

static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
static SNAP: Mutex<Option<Snapshot>> = Mutex::new(None);
static ACTIONS: std::sync::LazyLock<Mutex<HashMap<isize, Box<dyn Fn() + Send>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
static GW_TX: Mutex<Option<tokio::sync::oneshot::Sender<()>>> = Mutex::new(None);
static TRAY_CFG: Mutex<Option<(Option<std::path::PathBuf>, u16)>> = Mutex::new(None);
static PULSE: AtomicBool = AtomicBool::new(false);
static TICK: AtomicU64 = AtomicU64::new(0);
static LAST_HEALTHY: AtomicBool = AtomicBool::new(true);
static SINK: OnceLock<Retained<ActionSink>> = OnceLock::new();
static TX: OnceLock<mpsc::Sender<Snapshot>> = OnceLock::new();

// 跨线程句柄（仅主线程解引用）
struct MainPtr<T: ?Sized>(*const T);
unsafe impl<T: ?Sized> Send for MainPtr<T> {}
unsafe impl<T: ?Sized> Sync for MainPtr<T> {}

struct UiRefs {
    stack: MainPtr<NSStackView>,
    button: MainPtr<NSButton>,
    switch_ctl: MainPtr<NSSwitch>,
    popover: MainPtr<NSPopover>,
}
static UI: OnceLock<UiRefs> = OnceLock::new();

// 稳定 tag → 动作
const TAG_STATUS: isize = 0;
const TAG_SWITCH: isize = 1;
const TAG_GW_TOGGLE: isize = 2;
const TAG_COPY: isize = 3;
const TAG_GITHUB: isize = 4;
const TAG_DASHBOARD: isize = 5;
const TAG_EXIT: isize = 6;

#[derive(Default, Clone)]
struct Snapshot {
    healthy: bool,
    inflight: u64,
    requests: u64,
    success: u64,
    tokens: u64,
    plans: Vec<(String, i64)>,
    recent: Vec<RecentRow>,
    providers: Vec<ProvRow>,
    last_sync: String,
    last_fail_ts: Option<i64>,
}

#[derive(Clone)]
struct RecentRow {
    model: String,
    status: i64,
    total_ms: i64,
    ttft: Option<i64>,
    tok_s: Option<f64>,
    agent: String,
    time: String,
}

#[derive(Clone)]
struct ProvRow {
    key: String,
    kind: String,
    disabled: bool,
    pct: Option<i64>,
    tok24: u64,
}

fn snap() -> Snapshot {
    SNAP.lock().unwrap().clone().unwrap_or_default()
}

fn hhmm(ms: i64) -> String {
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t = (ms / 1000) as libc::time_t;
        if libc::localtime_r(&t, &mut tm).is_null() {
            return "--:--".into();
        }
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    }
}

fn now_hhmmss() -> String {
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t = (ms / 1000) as libc::time_t;
        if libc::localtime_r(&t, &mut tm).is_null() {
            return "--:--:--".into();
        }
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

fn fmt_tok(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        format!("{n}")
    }
}

fn short_kind(kind: &str) -> String {
    match kind {
        "coding" => "CP".into(),
        "go" => "GO".into(),
        "agent" => "AP".into(),
        "token" => "TOK".into(),
        other => other.get(0..4.min(other.len())).unwrap_or("—").to_uppercase(),
    }
}

// ── 轮询（tokio 侧，与 web 面板同源 API）──

async fn get_json(path: &str) -> Option<Value> {
    let (_cfgp, port) = TRAY_CFG.lock().unwrap().clone().unwrap_or((None, 8787));
    let url = format!("http://127.0.0.1:{port}{path}");
    let body = reqwest::get(&url).await.ok()?.text().await.ok()?;
    serde_json::from_str(&body).ok()
}

async fn poll_live(tx: mpsc::Sender<Snapshot>) {
    let mut s = snap();
    if let Some(v) = get_json("/api/live").await {
        s.inflight = v["inflight"].as_u64().unwrap_or(0);
    } else {
        s.inflight = 0;
    }
    s.healthy = get_json("/healthz").await.is_some();
    let _ = tx.send(s);
}

async fn poll_data(tx: mpsc::Sender<Snapshot>) {
    let mut s = snap();
    if let Some(v) = get_json("/api/stats?window=24h").await {
        let models = v.get("models").cloned().unwrap_or_else(|| v.clone());
        let (mut r, mut ok, mut t) = (0u64, 0u64, 0u64);
        if let Some(m) = models.as_object() {
            for mv in m.values() {
                r += mv["requests"].as_u64().unwrap_or(0);
                ok += mv["success"].as_u64().unwrap_or(0);
                t += mv["prompt_tokens"].as_u64().unwrap_or(0)
                    + mv["completion_tokens"].as_u64().unwrap_or(0);
            }
        }
        s.requests = r;
        s.success = ok;
        s.tokens = t;
    }
    if let Some(v) = get_json("/api/providers").await {
        if let Some(arr) = v["providers"].as_array() {
            let mut provs = Vec::new();
            let mut plans: HashMap<String, i64> = HashMap::new();
            for p in arr {
                let key = p["key"].as_str().unwrap_or("-").to_string();
                let kind = p["plan_kind"].as_str().unwrap_or("-").to_string();
                let disabled = p["disabled"].as_bool().unwrap_or(false);
                let tok24 = p["tokens_24h"].as_u64().unwrap_or(0);
                let w5 = p["windows"]["5h"].as_f64();
                let used = p["used_5h"].as_f64().unwrap_or(0.0);
                // 配额进度（dashboard 卡同款）：observed 实测优先 → windows 兜底
                let mut pct: Option<i64> = None;
                if let Some(o5) = p["observed"]["5h"].as_object() {
                    let l = o5["limit"].as_f64();
                    let r = o5["remaining"].as_f64();
                    if let (Some(l), Some(r)) = (l, r) {
                        if l > 0.0 {
                            pct = Some(((l - r) / l * 100.0).round() as i64);
                        }
                    }
                }
                if pct.is_none() {
                    if let Some(w) = w5 {
                        if w > 0.0 {
                            pct = Some((used / w * 100.0).round() as i64);
                        }
                    }
                }
                if let Some(pct) = pct {
                    let short = short_kind(&kind);
                    plans
                        .entry(short)
                        .and_modify(|v| *v = (*v).max(pct))
                        .or_insert(pct);
                }
                provs.push(ProvRow { key, kind, disabled, pct, tok24 });
            }
            provs.sort_by(|a, b| b.disabled.cmp(&a.disabled).then(b.tok24.cmp(&a.tok24)));
            provs.truncate(8);
            s.providers = provs;
            s.plans = plans.into_iter().collect();
            s.plans.sort_by(|a, b| b.1.cmp(&a.1));
        }
    }
    if let Some(v) = get_json("/api/events?limit=30").await {
        if let Some(arr) = v.as_array() {
            if let Some(newest) = arr.iter().rev().find(|e| e["kind"] == "chat") {
                let status = newest["status"].as_i64().unwrap_or(0);
                s.last_fail_ts = if status != 200 {
                    newest["ts"].as_i64().or_else(|| newest["ts"].as_f64().map(|f| f as i64))
                } else {
                    None
                };
            }
            let mut rows = Vec::new();
            for e in arr.iter().rev().filter(|e| e["kind"] == "chat") {
                let status = e["status"].as_i64().unwrap_or(0);
                let total = e["total_ms"].as_i64().unwrap_or(0);
                let ttft_raw = e["ttft_ms"].as_i64().unwrap_or(-1);
                let out = e["completion_tokens"].as_u64().unwrap_or(0);
                let gen_ms = (total - ttft_raw.max(0)).max(1) as f64;
                let tok_s = if out > 0 { Some(out as f64 / (gen_ms / 1000.0)) } else { None };
                let agent = e["agent"]
                    .as_str()
                    .unwrap_or("-")
                    .split('/')
                    .next()
                    .unwrap_or("-")
                    .to_string();
                let ts = e["ts"].as_i64().unwrap_or(0);
                rows.push(RecentRow {
                    model: e["chosen"].as_str().unwrap_or("-").to_string(),
                    status,
                    total_ms: total,
                    ttft: if ttft_raw > 0 { Some(ttft_raw) } else { None },
                    tok_s,
                    agent,
                    time: hhmm(ts),
                });
                if rows.len() >= 5 {
                    break;
                }
            }
            s.recent = rows;
        }
    }
    s.last_sync = now_hhmmss();
    let _ = tx.send(s);
}

// ── 网关进程内启停 ──

fn gw_start() {
    let rt = RT.get().expect("runtime");
    rt.spawn(async move {
        let (cfgp, port) = TRAY_CFG.lock().unwrap().clone().unwrap_or((None, 8787));
        let Ok((cfg, _)) = config::load_config(cfgp.as_deref()) else {
            tracing::warn!("tray: config load failed");
            return;
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        match crate::spawn_gateway(cfg, "127.0.0.1".into(), port, rx).await {
            Ok(_) => {
                *GW_TX.lock().unwrap() = Some(tx);
                tracing::info!("tray: gateway started on :{port}");
            }
            Err(e) => tracing::warn!("tray: gateway start failed: {e}（已有实例？UI-only）"),
        }
    });
}

fn gw_stop() {
    if let Some(tx) = GW_TX.lock().unwrap().take() {
        let _ = tx.send(());
    }
}

// ── 动作汇（target/action 胶水）──

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "EvoActionSink"]
    struct ActionSink;

    impl ActionSink {
        #[unsafe(method(fire:))]
        fn fire(&self, sender: *mut AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            if let Some(f) = ACTIONS.lock().unwrap().get(&tag) {
                f();
            }
        }
    }
);

impl ActionSink {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        unsafe { msg_send![Self::class(), new] }
    }
}

fn register(tag: isize, f: impl Fn() + Send + 'static) {
    ACTIONS.lock().unwrap().insert(tag, Box::new(f));
}

fn shell(cmd: &str) {
    let owned = cmd.to_string();
    std::thread::spawn(move || {
        let _ = std::process::Command::new("/bin/zsh").arg("-c").arg(owned).output();
    });
}

// ── UI helpers ──

unsafe fn label(text: impl AsRef<str>, size: f64, bold: bool, mono: bool) -> Retained<NSTextField> {
    let mtm = MainThreadMarker::new().unwrap();
    let ns = NSString::from_str(text.as_ref());
    let l = NSTextField::labelWithString(&ns, mtm);
    let weight = if bold { 0.5 } else { 0.0 };
    let font = if mono {
        NSFont::monospacedSystemFontOfSize_weight(size, weight)
    } else {
        NSFont::systemFontOfSize_weight(size, weight)
    };
    l.setFont(Some(&font));
    l
}

unsafe fn colored(l: &NSTextField, c: &NSColor) {
    let _: () = msg_send![l, setTextColor: c];
}

fn rv(v: &NSView) -> Retained<NSView> {
    unsafe { Retained::retain(v as *const NSView as *mut NSView).unwrap() }
}

unsafe fn hrow(views: Vec<Retained<NSView>>, spacing: f64) -> Retained<NSStackView> {
    let arr = NSArray::from_retained_slice(&views);
    let row = NSStackView::stackViewWithViews(&arr, MainThreadMarker::new().unwrap());
    row.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
    row.setSpacing(spacing);
    row.setAlignment(NSLayoutAttribute::CenterY);
    row
}

unsafe fn vstack(views: Vec<Retained<NSView>>, spacing: f64) -> Retained<NSStackView> {
    let arr = NSArray::from_retained_slice(&views);
    let col = NSStackView::stackViewWithViews(&arr, MainThreadMarker::new().unwrap());
    col.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
    col.setSpacing(spacing);
    col.setAlignment(NSLayoutAttribute::Leading);
    col
}

unsafe fn bar(pct: i64, width: f64) -> Retained<NSProgressIndicator> {
    let mtm = MainThreadMarker::new().unwrap();
    let p = NSProgressIndicator::new(mtm);
    p.setIndeterminate(false);
    p.setMinValue(0.0);
    p.setMaxValue(100.0);
    p.setDoubleValue(pct.clamp(0, 100) as f64);
    p.setTranslatesAutoresizingMaskIntoConstraints(false);
    p.heightAnchor().constraintEqualToConstant(6.0).setActive(true);
    p.widthAnchor().constraintEqualToConstant(width).setActive(true);
    p
}

unsafe fn dot(color: &NSColor) -> Retained<NSTextField> {
    let d = label("●", 8.0, false, false);
    colored(&d, color);
    d
}

unsafe fn make_button(title: &str, tag: isize, small: bool) -> Retained<NSButton> {
    let mtm = MainThreadMarker::new().unwrap();
    let b = NSButton::new(mtm);
    let ns = NSString::from_str(title);
    b.setTitle(&ns);
    if small {
        b.setControlSize(objc2_app_kit::NSControlSize::Small);
        b.setFont(Some(&NSFont::systemFontOfSize_weight(11.0, 0.0)));
    }
    let sink = sink_of();
    let _: () = msg_send![&b, setTarget: sink];
    let _: () = msg_send![&b, setAction: sel!(fire:)];
    let _: () = msg_send![&b, setTag: tag];
    b
}

trait AsNsView {
    fn as_nsview(&self) -> &NSView;
}
impl AsNsView for NSTextField {
    fn as_nsview(&self) -> &NSView {
        unsafe { &*(std::ptr::from_ref::<NSTextField>(self) as *const NSView) }
    }
}
impl AsNsView for NSProgressIndicator {
    fn as_nsview(&self) -> &NSView {
        unsafe { &*(std::ptr::from_ref::<NSProgressIndicator>(self) as *const NSView) }
    }
}
impl AsNsView for NSSwitch {
    fn as_nsview(&self) -> &NSView {
        unsafe { &*(std::ptr::from_ref::<NSSwitch>(self) as *const NSView) }
    }
}
impl AsNsView for NSButton {
    fn as_nsview(&self) -> &NSView {
        unsafe { &*(std::ptr::from_ref::<NSButton>(self) as *const NSView) }
    }
}
impl AsNsView for NSStackView {
    fn as_nsview(&self) -> &NSView {
        unsafe { &*(std::ptr::from_ref::<NSStackView>(self) as *const NSView) }
    }
}
impl AsNsView for Retained<NSView> {
    fn as_nsview(&self) -> &NSView {
        self
    }
}

// ── 图标四态 ──

fn fail_recent(s: &Snapshot) -> bool {
    s.last_fail_ts
        .map(|ts| {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
            now - ts < 60_000
        })
        .unwrap_or(false)
}

unsafe fn apply_icon(s: &Snapshot, pulse: bool) {
    let Some(ui) = UI.get() else { return };
    let btn = unsafe { &*ui.button.0 };
    let color: Retained<NSColor> = if !s.healthy {
        NSColor::quaternaryLabelColor()
    } else if fail_recent(s) {
        NSColor::systemRedColor()
    } else if s.inflight > 0 && pulse {
        NSColor::tertiaryLabelColor()
    } else {
        NSColor::labelColor()
    };
    let _: () = msg_send![btn, setContentTintColor: &*color];
}

// ── 面板渲染 ──

unsafe fn section_title(t: &str) -> Retained<NSView> {
    let l = label(t, 10.0, true, false);
    colored(&l, &NSColor::secondaryLabelColor());
    rv(l.as_nsview())
}

unsafe fn stat_block(name: &str, value: &str) -> Retained<NSView> {
    let top = label(name, 10.0, false, false);
    colored(&top, &NSColor::secondaryLabelColor());
    let val = label(value, 14.0, true, true);
    let col = vstack(vec![rv(top.as_nsview()), rv(val.as_nsview())], 1.0);
    rv(col.as_nsview())
}

unsafe fn render(s: &Snapshot) {
    let Some(ui) = UI.get() else { return };
    let stack = unsafe { &*ui.stack.0 };
    {
        let empty: Retained<NSArray<NSView>> = NSArray::from_retained_slice(&[]);
        let _: () = msg_send![stack, setArrangedSubviews: &*empty];
    }

    let green = NSColor::systemGreenColor();
    let red = NSColor::systemRedColor();
    let orange = NSColor::systemOrangeColor();
    let gray = NSColor::systemGrayColor();
    let blue = NSColor::systemBlueColor();
    let sec = NSColor::secondaryLabelColor();
    let tert = NSColor::tertiaryLabelColor();

    // 头部：状态点 + 标题 + 进行中 + 复制 + 同步时间
    let (dc, title) = if s.healthy {
        (&green, "网关运行中 · 8787")
    } else {
        (&red, "网关已停止")
    };
    let mut head = vec![rv(dot(dc).as_nsview()), rv(label(title, 12.0, true, false).as_nsview())];
    if s.inflight > 0 {
        let chip = label(&format!("{} 进行中", s.inflight), 10.0, true, false);
        colored(&chip, &blue);
        head.push(rv(chip.as_nsview()));
    }
    head.push(rv(make_button("⧉", TAG_COPY, true).as_nsview()));
    head.push(rv(label(s.last_sync.clone(), 10.0, false, false).as_nsview()));
    stack.addArrangedSubview(&hrow(head, 6.0));

    // 统计行
    let rate = if s.requests > 0 {
        (100.0 * s.success as f64 / s.requests as f64).round() as i64
    } else {
        0
    };
    let stat_row = hrow(
        vec![
            rv(stat_block("请求", &format!("{}", s.requests)).as_nsview()),
            rv(stat_block("成功率", &format!("{rate}%")).as_nsview()),
            rv(stat_block("Tokens", &fmt_tok(s.tokens)).as_nsview()),
            {
                let t = label("最近 24h", 9.0, false, false);
                colored(&t, &tert);
                rv(t.as_nsview())
            },
        ],
        14.0,
    );
    stack.addArrangedSubview(&stat_row);

    // 方案 5h 用量
    if !s.plans.is_empty() {
        stack.addArrangedSubview(&section_title("方案 5h 用量"));
        for (name, pct) in &s.plans {
            let disp = if *pct > 999 { ">999%".to_string() } else { format!("{pct}%") };
            let row = hrow(
                vec![
                    rv(label(name, 11.0, true, false).as_nsview()),
                    rv(bar(*pct, 120.0).as_nsview()),
                    rv(label(&disp, 11.0, false, true).as_nsview()),
                ],
                6.0,
            );
            stack.addArrangedSubview(&row);
        }
    }

    // 最近决策
    if !s.recent.is_empty() {
        stack.addArrangedSubview(&section_title("最近决策"));
        for r in &s.recent {
            let dc = if r.status == 200 { &green } else { &red };
            let mut meta = vec![rv(label(r.time.clone(), 9.0, false, false).as_nsview())];
            if let Some(tt) = r.ttft {
                meta.push(rv(label(format!("TTFT {tt}ms"), 9.0, false, false).as_nsview()));
            }
            if let Some(ts) = r.tok_s {
                if ts > 0.0 {
                    meta.push(rv(label(format!("{ts:.0} tok/s"), 9.0, false, false).as_nsview()));
                }
            }
            meta.push(rv(label(r.agent.clone(), 9.0, false, true).as_nsview()));
            let sub = hrow(meta, 5.0);
            let col = vstack(
                vec![
                    rv(label(r.model.clone(), 10.5, false, false).as_nsview()),
                    rv(sub.as_nsview()),
                ],
                1.0,
            );
            let right = if r.status == 200 {
                rv(label(format!("{}ms", r.total_ms), 10.0, false, true).as_nsview())
            } else {
                let f = label("fail", 10.0, false, true);
                colored(&f, &red);
                rv(f.as_nsview())
            };
            let row = hrow(
                vec![rv(dot(dc).as_nsview()), rv(col.as_nsview()), right],
                6.0,
            );
            stack.addArrangedSubview(&row);
        }
    }

    // Provider 管理
    if !s.providers.is_empty() {
        stack.addArrangedSubview(&section_title("Provider 管理"));
        for p in &s.providers {
            let dc = if p.disabled {
                &gray
            } else if matches!(p.pct, Some(v) if v >= 85) {
                &orange
            } else {
                &green
            };
            let mut sub = Vec::new();
            if p.disabled {
                sub.push(rv(label("已禁用", 9.0, false, false).as_nsview()));
            }
            if p.tok24 > 0 {
                sub.push(rv(label(format!("{} · 24h", fmt_tok(p.tok24)), 9.0, false, false).as_nsview()));
            }
            let left = if sub.is_empty() {
                rv(label(p.key.clone(), 10.5, false, false).as_nsview())
            } else {
                let col = vstack(
                    vec![
                        rv(label(p.key.clone(), 10.5, false, false).as_nsview()),
                        rv(hrow(sub, 4.0).as_nsview()),
                    ],
                    1.0,
                );
                rv(col.as_nsview())
            };
            let right: Retained<NSView> = if let Some(pct) = p.pct {
                let disp = if pct > 999 { ">999%".to_string() } else { format!("{pct}%") };
                rv(hrow(
                    vec![rv(bar(pct, 90.0).as_nsview()), rv(label(disp, 10.0, false, true).as_nsview())],
                    5.0,
                )
                .as_nsview())
            } else if p.kind == "api" {
                rv(label("按量", 9.0, false, false).as_nsview())
            } else {
                rv(label("—", 9.0, false, false).as_nsview())
            };
            let row = hrow(vec![rv(dot(dc).as_nsview()), left, right], 6.0);
            stack.addArrangedSubview(&row);
        }
    }

    // 分隔 + 控制区
    let div = label("─────────────────────────", 8.0, false, false);
    colored(&div, &tert);
    stack.addArrangedSubview(&div);

    let sw = unsafe { &*ui.switch_ctl.0 };
    let autostart_row = hrow(
        vec![
            rv(sw.as_nsview()),
            rv(label("开机自动启动", 11.0, false, false).as_nsview()),
        ],
        6.0,
    );
    stack.addArrangedSubview(&autostart_row);

    let gw_title = if s.healthy { "停止网关" } else { "启动网关" };
    stack.addArrangedSubview(&make_button(gw_title, TAG_GW_TOGGLE, true));

    let link_row = hrow(
        vec![
            rv(make_button("开源地址", TAG_GITHUB, true).as_nsview()),
            rv(make_button("完整面板", TAG_DASHBOARD, true).as_nsview()),
            rv(make_button("退出", TAG_EXIT, true).as_nsview()),
        ],
        10.0,
    );
    stack.addArrangedSubview(&link_row);
}

// ── 动作实现 ──

fn open_url(url: &str) {
    unsafe {
        let Some(mtm) = MainThreadMarker::new() else { return };
        let ns = NSString::from_str(url);
        if let Some(u) = objc2_foundation::NSURL::URLWithString(&ns) {
            NSWorkspace::sharedWorkspace().openURL(&u);
        }
    }
}

fn register_actions() {
    register(TAG_STATUS, || {
        let Some(ui) = UI.get() else { return };
        unsafe {
            let pop = &*ui.popover.0;
            if pop.isShown() {
                pop.performClose(None);
            } else {
                let btn = &*ui.button.0;
                let _: () = msg_send![pop,
                    showRelativeToRect: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0))
                    ofView: btn
                    preferredEdge: 2usize]; // NSRectEdgeMinY
            }
        }
    });
    register(TAG_SWITCH, || {
        let Some(ui) = UI.get() else { return };
        unsafe {
            let sw = &*ui.switch_ctl.0;
            let on: isize = msg_send![sw, state];
            let on = on == 1;
            NSUserDefaults::standardUserDefaults().setBool_forKey(on, objc2_foundation::ns_string!("ev-autostart"));
            let action = if on { "enable" } else { "disable" };
            shell(&format!(
                "launchctl {action} gui/$(id -u)/ai.evolve-router.tray 2>/dev/null; launchctl {action} gui/$(id -u)/ai.evolve-router.gateway 2>/dev/null"
            ));
        }
    });
    register(TAG_GW_TOGGLE, || {
        let s = snap();
        if s.healthy { gw_stop() } else { gw_start() }
    });
    register(TAG_COPY, || {
        let (_cfgp, port) = TRAY_CFG.lock().unwrap().clone().unwrap_or((None, 8787));
        let cfg = format!("baseURL  = http://127.0.0.1:{port}/v1\nmodel    = auto\nprovider = evo-router");
        unsafe {
            let ns = NSString::from_str(&cfg);
            let pb: *mut AnyObject = msg_send![objc2::class!(NSPasteboard), generalPasteboard];
            let _: () = msg_send![pb, clearContents];
            let _: () = msg_send![pb, setString: &*ns forType: objc2_foundation::ns_string!("public.utf8-plain-text")];
        }
    });
    register(TAG_GITHUB, || open_url("https://github.com/holny/evolve-router"));
    register(TAG_DASHBOARD, || {
        let (_cfgp, port) = TRAY_CFG.lock().unwrap().clone().unwrap_or((None, 8787));
        open_url(&format!("http://127.0.0.1:{port}"));
    });
    register(TAG_EXIT, || unsafe {
        let Some(mtm) = MainThreadMarker::new() else { return };
        let alert = NSAlert::new(mtm);
        alert.setMessageText(objc2_foundation::ns_string!("确定彻底退出 EvolveRouter 吗？"));
        alert.setInformativeText(objc2_foundation::ns_string!("网关与菜单栏将全部关闭。\n重新打开：应用程序 → EvolveRouter"));
        alert.addButtonWithTitle(objc2_foundation::ns_string!("退出"));
        alert.addButtonWithTitle(objc2_foundation::ns_string!("取消"));
        if alert.runModal() == 1000 {
            shell("(sleep 0.3; launchctl bootout gui/$(id -u)/ai.evolve-router.tray) >/dev/null 2>&1 &");
            let app = NSApplication::sharedApplication(mtm);
            let _: () = msg_send![&app, terminate: std::ptr::null::<AnyObject>()];
        }
    });
}

// ── UI 装配 ──


unsafe fn sink_of() -> &'static ActionSink {
    SINK.get().unwrap()
}

fn load_icon(mtm: MainThreadMarker) -> Retained<NSImage> {
    let bytes = include_bytes!("../assets/tray-icon@2x.png");
    let data = NSData::with_bytes(bytes);
    let img = unsafe { NSImage::initWithData(NSImage::alloc(), &data) }.expect("tray icon");
    img.setSize(NSSize::new(22.0, 22.0));
    img.setTemplate(true);
    img
}

unsafe fn build_ui(mtm: MainThreadMarker, app: &NSApplication) {
    let status_item = NSStatusBar::systemStatusBar().statusItemWithLength(-1.0);
    let button = status_item.button(mtm).unwrap();
    let icon = load_icon(mtm);
    button.setImage(Some(&icon));

    let sink = ActionSink::new(mtm);
    let _ = SINK.set(sink.clone());

    // 状态按钮 → 弹出面板
    let _: () = msg_send![&button, setTarget: &*sink];
    let _: () = msg_send![&button, setAction: sel!(fire:)];
    let _: () = msg_send![&button, setTag: TAG_STATUS];

    // 面板：容器 = 竖排 NSStackView 本体（宽度约束唯一；高度由 fittingSize 供给 popover）
    let stack = NSStackView::stackViewWithViews(&NSArray::from_retained_slice(&[]), mtm);
    stack.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
    stack.setSpacing(9.0);
    stack.setAlignment(NSLayoutAttribute::Leading);
    stack.setTranslatesAutoresizingMaskIntoConstraints(false);
    stack.widthAnchor().constraintEqualToConstant(340.0).setActive(true);
    let container: Retained<NSView> =
        Retained::retain(stack.as_nsview() as *const NSView as *mut NSView).unwrap();

    // 自启开关（持久实例；未设置过默认开）
    let sw = NSSwitch::new(mtm);
    let defaults = NSUserDefaults::standardUserDefaults();
    let key = objc2_foundation::ns_string!("ev-autostart");
    let obj: *mut AnyObject = unsafe { msg_send![&defaults, objectForKey: key] };
    let saved = if !obj.is_null() { defaults.boolForKey(key) } else { true };
    sw.setState(if saved { 1 } else { 0 });
    let _: () = msg_send![&sw, setTarget: &*sink];
    let _: () = msg_send![&sw, setAction: sel!(fire:)];
    let _: () = msg_send![&sw, setTag: TAG_SWITCH];

    let vc = objc2_app_kit::NSViewController::new(mtm);
    vc.setView(container.as_nsview());
    let popover = NSPopover::new(mtm);
    popover.setBehavior(NSPopoverBehavior::Transient);
    popover.setContentViewController(Some(&vc));

    let _ = UI.set(UiRefs {
        stack: MainPtr(Retained::as_ptr(&stack)),
        button: MainPtr(Retained::as_ptr(&button) as *const NSButton as *mut NSButton),
        switch_ctl: MainPtr(Retained::as_ptr(&sw)),
        popover: MainPtr(Retained::as_ptr(&popover)),
    });

    register_actions();

    // 初始渲染 + 轮询定时器
    render(&snap());
    let (tx, rx) = mpsc::channel::<Snapshot>();
    let _ = TX.set(tx);
    let block = block2::RcBlock::new(move || {
        let poll_tx = TX.get().cloned();
        let n = TICK.fetch_add(1, Ordering::Relaxed);
        if n % 4 == 1 {
            if let (Some(t), Some(rt)) = (poll_tx.clone(), RT.get()) {
                rt.spawn(poll_live(t));
            }
        }
        if n % 12 == 6 {
            if let (Some(t), Some(rt)) = (poll_tx.clone(), RT.get()) {
                rt.spawn(poll_data(t));
            }
        }
        while let Ok(s) = rx.try_recv() {
            let data_changed = {
                let g = SNAP.lock().unwrap();
                g.as_ref().map(|old| old.last_sync != s.last_sync).unwrap_or(true)
            };
            *SNAP.lock().unwrap() = Some(s.clone());
            if data_changed {
                let cur = snap();
                render(&cur);
            }
        }
        let s = snap();
        if s.inflight > 0 && s.healthy {
            if n % 2 == 0 {
                PULSE.fetch_xor(true, Ordering::Relaxed);
            }
        } else {
            PULSE.store(false, Ordering::Relaxed);
        }
        apply_icon(&s, PULSE.load(Ordering::Relaxed));
        // 健康态变化即时刷新按钮文案
        if s.healthy != LAST_HEALTHY.load(Ordering::Relaxed) {
            LAST_HEALTHY.store(s.healthy, Ordering::Relaxed);
            render(&s);
        }
    });
    let timer: Retained<NSTimer> =
        msg_send![NSTimer::class(), timerWithTimeInterval: 0.25f64, block: &*block, repeats: true];
    let rl: Retained<objc2_foundation::NSRunLoop> = objc2_foundation::NSRunLoop::currentRunLoop();
    let _: () = msg_send![&rl, addTimer: &*timer forMode: objc2_foundation::ns_string!("kCFRunLoopCommonModes")];
    std::mem::forget(timer); // 定时器与 block 与 app 同生命周期
    std::mem::forget(block);

    let _: () = msg_send![app, setActivationPolicy: NSApplicationActivationPolicy::Accessory];
}

// ── 入口 ──

pub fn run(config_path: Option<std::path::PathBuf>, port_override: Option<u16>) -> anyhow::Result<()> {
    let Some(mtm) = MainThreadMarker::new() else {
        anyhow::bail!("tray 必须在主线程运行");
    };
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let _ = RT.set(rt);
    *TRAY_CFG.lock().unwrap() = Some((config_path, port_override.unwrap_or(8787)));
    gw_start();

    unsafe {
        let app = NSApplication::sharedApplication(mtm);
        build_ui(mtm, &app);
        app.run();
    }
    Ok(())
}
