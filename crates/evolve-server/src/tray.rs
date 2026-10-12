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
use objc2_app_kit::{NSAlert, NSApplication, NSApplicationActivationPolicy, NSButton,
    NSColor, NSImage, NSMenu, NSMenuItem, NSProgressIndicator, NSStatusBar, NSStatusBarButton,
    NSStatusItem, NSWorkspace};
use objc2_foundation::{NSData, NSSize, NSObject, NSUserDefaults, NSString, NSTimer, NSRunLoop};
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
macro_rules! main_ptr {
    ($t:ty) => {{
        struct Wrap(*mut $t);
        unsafe impl Send for Wrap {}
        unsafe impl Sync for Wrap {}
        Wrap
    }};
}
// 主线程专属裸指针通用 wrapper（Send/Sync 强转，仅主线程触碰）
struct UnsafeSend<T: ?Sized>(*mut T);
unsafe impl<T: ?Sized> Send for UnsafeSend<T> {}
unsafe impl<T: ?Sized> Sync for UnsafeSend<T> {}
static MENU: OnceLock<UnsafeSend<NSMenu>> = OnceLock::new();
// NSStatusItem 保活（仅主线程触碰）
static STATUS_ITEM: OnceLock<UnsafeSend<NSStatusItem>> = OnceLock::new(); // 仅主线程触碰

// 菜单项槽位（NSMenu 原生下拉）。
static UI: OnceLock<UiRefs> = OnceLock::new();

#[derive(Debug)]
struct MainItem(*mut NSMenuItem);
unsafe impl Send for MainItem {}
unsafe impl Sync for MainItem {}

struct UiRefs {
    gw_line: MainItem,
    st_line: MainItem,
    plans_line: MainItem,
    provs: [MainItem; 8],
    recent_title: MainItem,
    recents: [MainItem; 5],
    gw_btn: MainItem,
    auto_item: MainItem,
}

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
    last_used: Option<i64>,
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
                let last_used = p["last_used_ms"]
                    .as_i64()
                    .or_else(|| p["last_used_ms"].as_f64().map(|f| f as i64))
                    .filter(|&v| v > 0);
                provs.push(ProvRow { key, kind, disabled, pct, tok24, last_used });
            }
            // 排序（用户裁决）：最近使用优先；无记录沉底；禁用殿后
            provs.sort_by(|a, b| {
                b.last_used.cmp(&a.last_used).then(b.disabled.cmp(&a.disabled))
            });
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

fn tray_dbg(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/ev-tray-dbg.log") {
        let _ = writeln!(f, "{} {}", now_hhmmss(), msg);
    }
}

fn shell(cmd: &str) {
    let owned = cmd.to_string();
    std::thread::spawn(move || {
        let _ = std::process::Command::new("/bin/zsh").arg("-c").arg(owned).output();
    });
}

// ── UI helpers ──

fn fail_recent(s: &Snapshot) -> bool {
    s.last_fail_ts
        .map(|ts| {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
            now - ts < 60_000
        })
        .unwrap_or(false)
}

unsafe fn apply_icon(s: &Snapshot, pulse: bool) {
    // 图标 tint（四态）：通过 status item button 找到（STATUS_ITEM 只在主线程触碰）
    let Some(w) = STATUS_ITEM.get() else { return };
    let btn: Option<Retained<NSStatusBarButton>> = {
        // UnsafeSend<NSStatusItem> → button(mtm)
        let mtm = MainThreadMarker::new().unwrap_or_else(|| unreachable!());
        status_item_button(w.0, mtm)
    };
    let Some(btn) = btn else { return };
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

unsafe fn status_item_button(item: *mut NSStatusItem, mtm: MainThreadMarker) -> Option<Retained<NSStatusBarButton>> {
    let item = &*item;
    let btn: Option<Retained<NSStatusBarButton>> = item.button(mtm);
    let raw = btn.map(|b| Retained::into_raw(b)).unwrap_or(std::ptr::null_mut());
    if raw.is_null() { None } else { Some(Retained::retain(raw).unwrap()) }
}

// ── 面板渲染 ──

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
    register(TAG_SWITCH, || {
        unsafe {
            let defaults = NSUserDefaults::standardUserDefaults();
            let key = objc2_foundation::ns_string!("ev-autostart");
            let saved = defaults.boolForKey(key);
            let on = !saved;
            defaults.setBool_forKey(on, key);
            // 刷新勾选状态
            if let Some(ui) = UI.get() {
                let obj = ui.auto_item.0;
                let _: () = msg_send![obj, setState: if on { 1isize } else { 0 }];
            }
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


fn nsstr(t: &str) -> Retained<NSString> {
    NSString::from_str(t)
}

unsafe fn add_sep(menu: &NSMenu) {
    let sep = NSMenuItem::separatorItem(MainThreadMarker::new().unwrap());
    menu.addItem(&sep);
}

unsafe fn add_item(menu: &NSMenu, title: &str, tag: isize) -> Retained<NSMenuItem> {
    let it = NSMenuItem::new(MainThreadMarker::new().unwrap());
    it.setTitle(&nsstr(title));
    if tag >= 0 {
        let sink: &Retained<ActionSink> = SINK.get().unwrap();
        let target: *mut ActionSink = Retained::as_ptr(sink) as *mut ActionSink;
        let _: () = msg_send![&it, setTarget: target];
        let _: () = msg_send![&it, setAction: sel!(fire:)];
        let _: () = msg_send![&it, setTag: tag];
    }
    menu.addItem(&it);
    it
}

unsafe fn build_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    let sink = SINK.get().unwrap();

    macro_rules! slot {
        ($title:expr, $tag:expr) => {{
            let it = NSMenuItem::new(mtm);
            it.setTitle(&nsstr($title));
            if $tag >= 0 {
                let target: *mut ActionSink = Retained::as_ptr(sink) as *mut ActionSink;
                let _: () = msg_send![&it, setTarget: target];
                let _: () = msg_send![&it, setAction: sel!(fire:)];
                let _: () = msg_send![&it, setTag: $tag];
            }
            menu.addItem(&it);
            it
        }};
    }

    let gw_line = slot!("● 网关已停止", -1);
    add_sep(&menu);
    let st_line = slot!("请求 … · 成功率 … · Tokens …", -1);
    let plans_line = slot!("方案 5h：…", -1);
    add_sep(&menu);
    // 最近使用 Provider（8 槽位占位，更新时填数据/隐藏）
    let provs_logo = slot!("Provider（最近使用）", -1);
    let _ = provs_logo.setTitle(&nsstr("Provider（最近使用）"));
    let mut provs = Vec::new();
    for _ in 0..8 {
        let it = slot!("", -1);
        it.setHidden(true);
        provs.push(MainItem(Retained::as_ptr(&it) as *mut NSMenuItem));
    }
    add_sep(&menu);
    let recent_title = slot!("最近决策", -1);
    let mut recents = Vec::new();
    for _ in 0..5 {
        let it = slot!("", -1);
        it.setHidden(true);
        recents.push(MainItem(Retained::as_ptr(&it) as *mut NSMenuItem));
    }
    add_sep(&menu);
    let gw_btn = slot!("停止网关", TAG_GW_TOGGLE);
    let auto_item = slot!("开机自动啟动", TAG_SWITCH);
    let _copy = slot!("复制接入配置", TAG_COPY);
    let _dashboard = slot!("打开完整面板", TAG_DASHBOARD);
    let _github = slot!("开源地址 (GitHub)", TAG_GITHUB);
    add_sep(&menu);
    let _exit = slot!("退出 EvolveRouter…", TAG_EXIT);

    let _ = prologue_menu_items(&menu);

    let _ = UI.set(UiRefs {
        gw_line: MainItem(Retained::as_ptr(&gw_line) as *mut NSMenuItem),
        st_line: MainItem(Retained::as_ptr(&st_line) as *mut NSMenuItem),
        plans_line: MainItem(Retained::as_ptr(&plans_line) as *mut NSMenuItem),
        provs: provs.try_into().unwrap(),
        recent_title: MainItem(Retained::as_ptr(&recent_title) as *mut NSMenuItem),
        recents: recents.try_into().unwrap(),
        gw_btn: MainItem(Retained::as_ptr(&gw_btn) as *mut NSMenuItem),
        auto_item: MainItem(Retained::as_ptr(&auto_item) as *mut NSMenuItem),
    });

    menu
}

// 占位：占位（防 unused 警告得起名字）——真实在 render/update_menu 中使用
fn prologue_menu_items(_menu: &NSMenu) -> bool {
    true
}

unsafe fn update_menu(s: &Snapshot) {
    let Some(ui) = UI.get() else { return };
    let set = |it: &MainItem, t: &str| unsafe {
        let obj = &*it.0;
        let _: () = msg_send![obj, setTitle: &*nsstr(t)];
    };
    let set_hidden = |it: &MainItem, hide: bool| unsafe {
        let obj = &*it.0;
        let _: () = msg_send![obj, setHidden: hide];
    };
    let state = |it: &MainItem, st: isize| unsafe {
        let obj = &*it.0;
        let _: () = msg_send![obj, setState: st];
    };

    if s.healthy {
        set(&ui.gw_line, "● 网关运行中 · 8787");
    } else {
        set(&ui.gw_line, "● 网关已停止");
    }
    let rate = if s.requests > 0 {
        (100.0 * s.success as f64 / s.requests as f64).round() as u64
    } else {
        0
    };
    set(
        &ui.st_line,
        &format!(
            "请求 {} · 成功率 {}% · Tokens {}（24h）",
            s.requests,
            rate,
            fmt_tok(s.tokens)
        ),
    );
    let mut plans_txt = String::from("方案 5h：");
    if s.plans.is_empty() {
        plans_txt.push_str("无套餐用量");
    } else {
        for (i, (k, v)) in s.plans.iter().enumerate() {
            if i > 0 {
                plans_txt.push_str(" · ");
            }
            let v = v.min(&999);
            let v = if *v > 999 { 999 } else { *v };
            plans_txt.push_str(&format!("{k} {v}%"));
        }
    }
    set(&ui.plans_line, &plans_txt);

    for (i, slot) in ui.provs.iter().enumerate() {
        if let Some(p) = s.providers.get(i) {
            set_hidden(slot, false);
            let dot = if p.disabled { "○" } else { "●" };
            let sub = if p.disabled {
                format!("{dot} {} 已禁用", p.key)
            } else if let Some(pct) = p.pct {
                let v = pct.min(999);
                let t24 = if p.tok24 > 0 {
                    format!(" · 24h {}", fmt_tok(p.tok24))
                } else {
                    String::new()
                };
                format!("{dot} {} {v}%{t24}", p.key)
            } else if p.kind == "api" {
                format!("{dot} {} 按量", p.key)
            } else {
                format!("{dot} {}", p.key)
            };
            set(slot, &sub);
        } else {
            set_hidden(slot, true);
        }
    }

    if s.recent.is_empty() {
        set(&ui.recent_title, "最近决策（空）");
    } else {
        set(&ui.recent_title, "最近决策");
    }
    for (i, slot) in ui.recents.iter().enumerate() {
        if let Some(r) = s.recent.get(i) {
            set_hidden(slot, false);
            let mark = if r.status == 200 { "●" } else { "✗" };
            let ts = r.tok_s.map(|v| format!(" · {:.0}tok/s", v)).unwrap_or_default();
            set(
                slot,
                &format!("{} {} {}ms · {}{} · {}", mark, r.model, r.total_ms, r.agent, ts, r.time),
            );
        } else {
            set_hidden(slot, true);
        }
    }

    set(&ui.gw_btn, if s.healthy { "停止网关" } else { "启动网关" });

    let defaults = NSUserDefaults::standardUserDefaults();
    let key = objc2_foundation::ns_string!("ev-autostart");
    let obj: *mut AnyObject = msg_send![&defaults, objectForKey: key];
    let saved = if !obj.is_null() { defaults.boolForKey(key) } else { true };
    state(&ui.auto_item, if saved { 1 } else { 0 });

    if let Some(w) = MENU.get().filter(|p| !p.0.is_null()) {
        unsafe {
            let m = &*w.0;
            let _: () = msg_send![m, update];
        }
    }
}

unsafe fn build_ui(mtm: MainThreadMarker, app: &NSApplication) {
    // 状态项＋图标（template：自动明暗；纯 logo alpha 展示，不 tint）
    let status_item = NSStatusBar::systemStatusBar().statusItemWithLength(-1.0);
    let icon = load_icon(mtm);
    if let Some(button) = status_item.button(mtm) {
        button.setImage(Some(&icon));
    }
    let _ = STATUS_ITEM.set(UnsafeSend(Retained::as_ptr(&status_item) as *mut NSStatusItem));
    std::mem::forget(status_item.clone()); // 保活

    register_actions();

    let menu = build_menu(mtm);
    let _ = MENU.set(UnsafeSend(Retained::as_ptr(&menu) as *mut NSMenu));
    std::mem::forget(menu.clone()); // 保活
    status_item.setMenu(Some(&menu));

    // 首帧数据 + 轮询定时器（0.25s tick，live 1s / data 3s）
    if let Some(s) = SNAP.lock().ok().and_then(|g| g.clone()) {
        update_menu(&s);
    }
    let (tx, rx) = mpsc::channel::<Snapshot>();
    let _ = TX.set(tx);
    let block = block2::RcBlock::new(move || {
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
        update_menu(&s);
        if s.healthy != LAST_HEALTHY.load(Ordering::Relaxed) {
            LAST_HEALTHY.store(s.healthy, Ordering::Relaxed);
            update_menu(&s);
        }
        if s.inflight > 0 && s.healthy {
            if n % 2 == 0 {
                PULSE.fetch_xor(true, Ordering::Relaxed);
            }
        } else {
            PULSE.store(false, Ordering::Relaxed);
        }
        apply_icon(&s, PULSE.load(Ordering::Relaxed));
    });
    let timer: Retained<NSTimer> = msg_send![NSTimer::class(),
        timerWithTimeInterval: 0.25f64, repeats: true, block: &*block];
    let rl: Retained<objc2_foundation::NSRunLoop> = objc2_foundation::NSRunLoop::currentRunLoop();
    let _: () = msg_send![&rl, addTimer: &*timer forMode: objc2_foundation::ns_string!("NSRunLoopCommonModes")];
    std::mem::forget(timer);
    std::mem::forget(block);
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
