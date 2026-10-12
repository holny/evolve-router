pub mod anthropic;
pub mod config;
pub mod identity;
pub mod meta;
pub mod quality;
pub mod relay;
pub mod rewrite;
pub mod service;
pub mod state;
pub mod translate;
pub mod stream;

#[cfg(all(target_os = "macos", feature = "tray"))]
pub mod tray;

/// tray 模式在进程内启动网关：bind 失败 = 已有实例在跑（返回 Err，调用方转 UI-only）。
/// shutdown 通道用于面板"停止网关"按钮优雅停机（flush 飞轮后退出 axum loop）。
pub async fn spawn_gateway(
    cfg: config::Config,
    host: String,
    port: u16,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<String> {
    use std::sync::Arc;
    let state = Arc::new(state::build_state(cfg));
    let app = state::build_router(state.clone());
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await
        .map_err(|e| anyhow::anyhow!("bind {addr}: {e}"))?;
    state.start_background();
    tracing::info!("evolve gateway (tray in-process) listening on http://{addr}");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).with_graceful_shutdown(async {
            let _ = shutdown.await;
        }).await;
        state.flywheel.flush();
        tracing::info!("tray gateway stopped, flywheel flushed");
    });
    Ok(addr)
}
