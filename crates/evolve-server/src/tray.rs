// evo-router tray 模式（macOS）：单二进制双模式之一。
// 同进程 = 网关（tokio 后台线程，spawn_gateway）+ AppKit 菜单栏。
// UI：NSPanel borderless（Stats 风格，底色/圆角）+ frame 布局大文本块 + 按钮行。
// 数据链路与 web 面板同源：/api/live /healthz /api/stats?window=24h /api/providers /api/events。
// 红线：所有 ObjC 对象只主线程触碰；msg_send 接收者/参数一律 lldb 验证过的形态。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, ClassType, MainThreadMarker};
use objc2_app_kit::{
    NSAlert, NSApplication, NSBackingStoreType, NSButton, NSColor,
    NSEvent, NSFont, NSImage, NSPanel, NSStatusBar, NSStatusItem, NSTextField,
    NSView, NSWindowStyleMask, NSWorkspace, NSWindow,
};
use objc2_foundation::{
    NSArray, NSData, NSObject, NSUserDefaults, NSPoint, NSRect, NSSize, NSString,
};
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
static SINK: OnceLock<Retained<ActionSink>> = OnceLock::new();
static TX: OnceLock<mpsc::Sender<Snapshot>> = OnceLock::new();
struct UnsafeRef<T: ?Sized>(*mut T);
impl<T: ?Sized> Clone for UnsafeRef<T> {
    fn clone(&self) -> Self { UnsafeRef(self.0) }
}
impl<T: ?Sized> Copy for UnsafeRef<T> {}
unsafe impl<T: ?Sized> Send for UnsafeRef<T> {}
unsafe impl<T: ?Sized> Sync for UnsafeRef<T> {}
static STATUS_ITEM: OnceLock<UnsafeRef<NSStatusItem>> = OnceLock::new();
static PANEL_REFS: Mutex<Option<UnsafePanelRefs>> = Mutex::new(None);

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
    last_fail_ts: Option<i64>,
}

#[derive(Clone)]
struct RecentRow {
    model: String,
    status: i64,
    total_ms: i64,
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

struct UnsafePanelRefs {
    panel: UnsafeRef<NSPanel>,
    info: UnsafeRef<NSTextField>,
    gw_btn: UnsafeRef<NSButton>,
    auto_btn: UnsafeRef<NSButton>,
}

impl Clone for UnsafePanelRefs {
    fn clone(&self) -> Self { *self }
}
impl Copy for UnsafePanelRefs {}

fn snap() -> Snapshot {
    SNAP.lock().unwrap().clone().unwrap_or_default()
}

unsafe fn panel() -> Option<UnsafePanelRefs> {
    PANEL_REFS.lock().unwrap().clone()
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
                    plans.entry(short).and_modify(|v| *v = (*v).max(pct)).or_insert(pct);
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

// ── 动作汇 ──

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

impl PanelDelegate {
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

fn fail_recent(s: &Snapshot) -> bool {
    s.last_fail_ts
        .map(|ts| {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
            now - ts < 60_000
        })
        .unwrap_or(false)
}

// ── 图标四态（tint）──

unsafe fn apply_icon(s: &Snapshot, pulse: bool) {
    let Some(st) = STATUS_ITEM.get() else { return };
    let Some(mtm) = MainThreadMarker::new() else { return };
    let item = unsafe { &*st.0 };
    let Some(btn) = item.button(mtm) else { return };
    let color: Retained<NSColor> = if !s.healthy {
        NSColor::quaternaryLabelColor()
    } else if fail_recent(s) {
        NSColor::systemRedColor()
    } else if s.inflight > 0 && pulse {
        NSColor::tertiaryLabelColor()
    } else {
        NSColor::labelColor()
    };
    let _: () = msg_send![&btn, setContentTintColor: &*color];
}

fn load_icon(mtm: MainThreadMarker) -> Retained<NSImage> {
    let bytes = include_bytes!("../assets/tray-icon@2x.png");
    let data = NSData::with_bytes(bytes);
    let img = unsafe { NSImage::initWithData(mtm.alloc(), &data) }.expect("tray icon");
    img.setSize(NSSize::new(22.0, 22.0));
    img.setTemplate(true);
    img
}

// ── 动作 ──


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
    register(TAG_SWITCH, || unsafe {
        let defaults = NSUserDefaults::standardUserDefaults();
        let key = objc2_foundation::ns_string!("ev-autostart");
        let on = !defaults.boolForKey(key);
        defaults.setBool_forKey(on, key);
        if let Some(pr) = panel() {
            let btn: Retained<NSButton> = Retained::retain(pr.auto_btn.0).unwrap();
            let t = NSString::from_str(if on { "自启：开" } else { "自启：关" });
            let _: () = msg_send![&btn, setTitle: &*t];
        }
        let action = if on { "enable" } else { "disable" };
        shell(&format!(
            "launchctl {action} gui/$(id -u)/ai.evolve-router.tray 2>/dev/null; launchctl {action} gui/$(id -u)/ai.evolve-router.gateway 2>/dev/null"
        ));
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
            let _: () = msg_send![pb, setString: &*ns, forType: objc2_foundation::ns_string!("public.utf8-plain-text")];
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
    register(TAG_STATUS, toggle_panel);
}

fn toggle_panel() {
    let Some(pr) = (unsafe { panel() }) else { return };
    unsafe {
        let p = &*pr.panel.0;
        if p.isVisible() {
            hide_panel(false);
        } else {
            // 刚被外部点击收起（resign ≤350ms 内）：本次点击视为"收起"，不重弹
            if let Some(t) = *LAST_OUT.lock().unwrap() {
                if t.elapsed() < std::time::Duration::from_millis(350) {
                    return;
                }
            }
            // 定位：跟随状态栏图标——按钮矩形转屏幕坐标，面板左上贴按钮左下（右缘防溢出）
            let st = STATUS_ITEM.get().unwrap();
            let item = &*st.0;
            let Some(mtm) = MainThreadMarker::new() else { return };
            let Some(bbtn) = item.button(mtm) else { return };
            let fr: NSRect = msg_send![&bbtn, frame];
            let win: Retained<NSWindow> = msg_send![&bbtn, window];
            let sc: NSRect = msg_send![&win, convertRectToScreen: fr];
            let mut x = sc.origin.x + 8.0;
            // 按钮所在屏（frame 包含按钮中心的屏）为锚——右缘 clamp 用该屏，多显示器不再跨界
            let anchor_screen: *mut AnyObject = {
                let screens: *mut AnyObject = msg_send![objc2::class!(NSScreen), screens];
                let cnt: u64 = msg_send![screens, count];
                let mut found: *mut AnyObject = std::ptr::null_mut();
                for i in 0..cnt as usize {
                    let sobj: *mut AnyObject = msg_send![screens, objectAtIndex: i];
                    let f: NSRect = msg_send![sobj, frame];
                    if sc.origin.x >= f.origin.x
                        && sc.origin.x <= f.origin.x + f.size.width
                        && sc.origin.y >= f.origin.y - 1.0
                        && sc.origin.y <= f.origin.y + f.size.height + 1.0
                    {
                        found = sobj;
                        break;
                    }
                }
                found
            };
            if !anchor_screen.is_null() {
                let af: NSRect = msg_send![anchor_screen, frame];
                if x + 340.0 > af.origin.x + af.size.width {
                    x = af.origin.x + af.size.width - 344.0;
                }
                if x < af.origin.x + 4.0 {
                    x = af.origin.x + 4.0;
                }
            }
            p.setFrameTopLeftPoint(NSPoint::new(x, sc.origin.y - 4.0));
            p.orderFrontRegardless();
            p.makeKeyAndOrderFront(None);
            install_outside_click_monitor();
        }
    }
}

// ── 点击外部自动隐藏：全局鼠标监听（不依赖 key/resign，accessory app 通杀）──

static MONITOR: Mutex<Option<UnsafeRef<AnyObject>>> = Mutex::new(None);

unsafe fn install_outside_click_monitor() {
    if MONITOR.lock().unwrap().is_some() {
        return;
    }
    let block = block2::RcBlock::new(|_evt: std::ptr::NonNull<NSEvent>| {
        // 用户点击了我们 app 以外的任何地方（桌面/Finder/其他 app）→ 收面板
        hide_panel(false);
        if let Some(m) = MONITOR.lock().unwrap().take() {
            let _: () = msg_send![objc2::class!(NSEvent), removeMonitor: m.0];
        }
    });
    let mask = objc2_app_kit::NSLeftMouseUpMask; // LeftMouseUp bit mask (static)
    if let Some(m) = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(mask, &block) {
        std::mem::forget(block); // 监听器随面板生命周期
        *MONITOR.lock().unwrap() = Some(UnsafeRef(Retained::into_raw(m) as *mut AnyObject));
    }
}

unsafe fn remove_outside_click_monitor() {
    if let Some(m) = MONITOR.lock().unwrap().take() {
        let _: () = msg_send![objc2::class!(NSEvent), removeMonitor: m.0];
    }
}

static LAST_OUT: Mutex<Option<std::time::Instant>> = Mutex::new(None);

fn hide_panel(resign: bool) {
    let Some(pr) = (unsafe { panel() }) else { return };
    unsafe {
        let panel = &*pr.panel.0;
        panel.orderOut(None);
        remove_outside_click_monitor();
    }
    if resign {
        *LAST_OUT.lock().unwrap() = Some(std::time::Instant::now());
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "EvoPanelDelegate"]
    struct PanelDelegate;

    impl PanelDelegate {
        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _note: *mut AnyObject) {
            hide_panel(true);
        }
    }
);

static PANEL_DELEGATE: OnceLock<Retained<PanelDelegate>> = OnceLock::new();

// ── 面板（NSPanel + frame 布局文字块 + 按钮行）──

unsafe fn build_panel(mtm: MainThreadMarker) -> UnsafePanelRefs {
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        mtm.alloc(),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(340.0, 470.0)),
        NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel | NSWindowStyleMask::UtilityWindow,
        NSBackingStoreType::Buffered,
        false,
    );
    panel.setLevel(87);
    panel.setHidesOnDeactivate(false);
    panel.setBecomesKeyOnlyIfNeeded(false);
    panel.setOpaque(true);
    panel.setHasShadow(true);
    let bg: *mut AnyObject = msg_send![objc2_app_kit::NSColor::class(), windowBackgroundColor];
    let _: () = msg_send![&panel, setBackgroundColor: bg];

    let content = NSView::new(mtm);
    content.setFrameSize(NSSize::new(340.0, 470.0));
    panel.setContentView(Some(&content));
    content.setWantsLayer(true);
    let lv: *mut AnyObject = msg_send![&content, layer];
    if !lv.is_null() {
        let _: () = msg_send![lv, setCornerRadius: 12f64];
        let _: () = msg_send![lv, setMasksToBounds: true];
    }

    // 信息块（单一 wrapping label，整段更新）
    let info = NSTextField::wrappingLabelWithString(
        &NSString::from_str("加载中…"),
        mtm,
    );
    info.setFrameOrigin(NSPoint::new(14.0, 92.0));
    info.setFrameSize(NSSize::new(312.0, 366.0));
    info.setFont(Some(&NSFont::monospacedSystemFontOfSize_weight(11.5, 0.0)));
    info.setSelectable(false);
    info.setDrawsBackground(false);
    info.setBordered(false);
    content.addSubview(&info);

    let sink: &Retained<ActionSink> = SINK.get().unwrap();
    let target: *mut ActionSink = Retained::as_ptr(sink) as *mut ActionSink;
    let mkbtn = |x: f64, y: f64, w: f64, title: &str, tag: isize| {
        let b = NSButton::new(mtm);
        let t = NSString::from_str(title);
        let _: () = msg_send![&b, setTitle: &*t];
        let _: () = msg_send![&b, setBezelStyle: 7isize];
        b.setFrameOrigin(NSPoint::new(x, y));
        b.setFrameSize(NSSize::new(w, 26.0));
        let _: () = msg_send![&b, setTarget: target];
        let _: () = msg_send![&b, setAction: sel!(fire:)];
        let _: () = msg_send![&b, setTag: tag];
        content.addSubview(&b);
        b
    };
    let dlg = PanelDelegate::new(mtm);
    let _ = PANEL_DELEGATE.set(dlg.clone());
    let _: () = msg_send![&panel, setDelegate: Retained::as_ptr(&dlg) as *mut AnyObject];

    let gw_btn = mkbtn(14.0, 56.0, 150.0, "停止网关", TAG_GW_TOGGLE);
    let auto_btn = mkbtn(176.0, 56.0, 150.0, "自启：开", TAG_SWITCH);
    let _copy = mkbtn(14.0, 24.0, 100.0, "复制配置", TAG_COPY);
    let _dash = mkbtn(120.0, 24.0, 100.0, "完整面板", TAG_DASHBOARD);
    let _github = mkbtn(226.0, 24.0, 100.0, "GitHub", TAG_GITHUB);
    let _exit = mkbtn(14.0, 434.0, 150.0, "退出 EvolveRouter", TAG_EXIT);
    let _ = (&_copy, &_dash, &_github, &_exit);

    let raw = UnsafePanelRefs {
        panel: UnsafeRef(Retained::as_ptr(&panel) as *mut NSPanel),
        info: UnsafeRef(Retained::as_ptr(&info) as *mut NSTextField),
        gw_btn: UnsafeRef(Retained::as_ptr(&gw_btn) as *mut NSButton),
        auto_btn: UnsafeRef(Retained::as_ptr(&auto_btn) as *mut NSButton),
    };
    std::mem::forget(panel);
    std::mem::forget(info);
    std::mem::forget(gw_btn);
    std::mem::forget(auto_btn);
    raw
}

unsafe fn update_panel(s: &Snapshot) {
    let Some(pr) = (unsafe { panel() }) else { return };
    let info = unsafe { &*pr.info.0 };
    let rate = if s.requests > 0 {
        (100.0 * s.success as f64 / s.requests as f64).round() as u64
    } else {
        0
    };
    let mut t = if s.healthy { "● 网关运行中 · 8787".to_string() } else { "● 网关已停止".to_string() };
    t.push_str(&format!("\n请求 {}   成功率 {}%   Tokens {}（24h）", s.requests, rate, fmt_tok(s.tokens)));
    t.push_str("\n──────────────────────────────\n");
    if s.plans.is_empty() {
        t.push_str("方案 5h：无套餐用量\n");
    } else {
        let mut line = String::from("方案 5h：");
        for (i, (k, v)) in s.plans.iter().enumerate() {
            if i > 0 {
                line.push_str(" · ");
            }
            let v = (*v).min(999);
            line.push_str(&format!("{k} {v}%"));
        }
        t.push_str(&line);
        t.push('\n');
    }
    t.push_str("\nProvider（最近使用）：\n");
    if s.providers.is_empty() {
        t.push_str("（无）\n");
    }
    for p in &s.providers {
        let dot = if p.disabled { "○" } else { "●" };
        if p.disabled {
            t.push_str(&format!("{} {} 已禁用\n", dot, p.key));
        } else if let Some(pct) = p.pct {
            let t24 = if p.tok24 > 0 { format!("  24h {}", fmt_tok(p.tok24)) } else { String::new() };
            t.push_str(&format!("{} {} {}%{}\n", dot, p.key, pct.min(999), t24));
        } else {
            t.push_str(&format!("{} {} 按量\n", dot, p.key));
        }
    }
    t.push_str("──────────────────────────────\n最近决策：\n");
    if s.recent.is_empty() {
        t.push_str("（无）\n");
    }
    for r in &s.recent {
        let mark = if r.status == 200 { "●" } else { "✗" };
        let ts = r.tok_s.map(|v| format!(" {:.0}tok/s", v)).unwrap_or_default();
        t.push_str(&format!("{} {}  {}ms  {} {}{}\n", mark, r.model, r.total_ms, r.agent, ts, r.time));
    }
    let ns = NSString::from_str(&t);
    let _: () = msg_send![info, setStringValue: &*ns];

    let gw = unsafe { &*pr.gw_btn.0 };
    let gw_t = NSString::from_str(if s.healthy { "停止网关" } else { "启动网关" });
    let _: () = msg_send![gw, setTitle: &*gw_t];

    let defaults = NSUserDefaults::standardUserDefaults();
    let key = objc2_foundation::ns_string!("ev-autostart");
    let saved = defaults.boolForKey(key);
    let auto = unsafe { &*pr.auto_btn.0 };
    let auto_t = NSString::from_str(if saved { "自启：开" } else { "自启：关" });
    let _: () = msg_send![auto, setTitle: &*auto_t];
}

// ── 装配 ──

unsafe fn build_ui(mtm: MainThreadMarker, app: &NSApplication) {
    let status_item = NSStatusBar::systemStatusBar().statusItemWithLength(-1.0);
    let sink = ActionSink::new(mtm);
    let _ = SINK.set(sink.clone());
    let icon = load_icon(mtm);
    if let Some(button) = status_item.button(mtm) {
        button.setImage(Some(&icon));
        let _: () = msg_send![&button, setTarget: &*sink];
        let _: () = msg_send![&button, setAction: sel!(fire:)];
        let _: () = msg_send![&button, setTag: TAG_STATUS];
    }
    let _ = STATUS_ITEM.set(UnsafeRef(Retained::as_ptr(&status_item) as *mut NSStatusItem));
    std::mem::forget(status_item);

    register_actions();

    let pr = build_panel(mtm);
    *PANEL_REFS.lock().unwrap() = Some(pr);

    if let Some(s) = SNAP.lock().ok().and_then(|g| g.clone()) {
        update_panel(&s);
    }

    let (tx, rx) = mpsc::channel::<Snapshot>();
    let _ = TX.set(tx);

    // 轮询循环（普通线程）：250ms tick；UI 更新经 dispatch2 主队列回投
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let n = TICK.fetch_add(1, Ordering::Relaxed);
        if n % 4 == 1 {
            if let (Some(t), Some(rt)) = (TX.get().cloned(), RT.get()) {
                rt.spawn(poll_live(t));
            }
        }
        if n % 12 == 6 {
            if let (Some(t), Some(rt)) = (TX.get().cloned(), RT.get()) {
                rt.spawn(poll_data(t));
            }
        }
        while let Ok(s) = rx.try_recv() {
            *SNAP.lock().unwrap() = Some(s);
        }
        let s = snap();
        let pulse = if s.healthy && s.inflight > 0 {
            PULSE.fetch_xor(true, Ordering::Relaxed)
        } else {
            PULSE.store(false, Ordering::Relaxed);
            false
        };
        // 主线程：面板刷新 + 图标 tint
        dispatch2::DispatchQueue::main().exec_async(move || {
            unsafe {
                update_panel(&s);
                apply_icon(&s, pulse);
            }
        });
    });

    let _ = app;
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
        {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/ev-tray-dbg.log") {
                let _ = writeln!(f, "[run] before build_ui");
            }
        }
        let app = NSApplication::sharedApplication(mtm);
        build_ui(mtm, &app);
        {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/ev-tray-dbg.log") {
                let _ = writeln!(f, "[run] after build_ui, entering NSApp.run");
            }
        }
        app.run();
    }
    Ok(())
}
