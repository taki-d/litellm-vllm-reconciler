use anyhow::Result;
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use vllm_reconciler::{
    config::Config,
    reconciler::Reconciler,
    telemetry::{self, log},
};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        log("fatal", json!({"error": error.to_string()}));
        std::process::exit(1);
    }
}
async fn run() -> Result<()> {
    let mut path = "config.yaml".to_string();
    let mut once = false;
    let mut dry_run = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                path = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--config requires a path"))?
            }
            "--once" => once = true,
            "--dry-run" => dry_run = true,
            "--help" | "-h" => {
                println!("vllm-reconciler [--config config.yaml] [--once] [--dry-run]");
                return Ok(());
            }
            _ => anyhow::bail!("unknown argument; use --help"),
        }
    }
    let mut config = Config::load(&path)?;
    config.reconcile.dry_run |= dry_run;
    let metrics = Arc::new(Mutex::new(telemetry::Metrics::default()));
    let mut reconciler = Reconciler::new(config.clone(), metrics.clone())?;
    if once {
        return reconciler.run_once(Instant::now()).await;
    }
    let listener = tokio::net::TcpListener::bind(&config.listen_address).await?;
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
    let server_stop = stop_rx.clone();
    let mut server = tokio::spawn(async move {
        let mut stop = server_stop;
        axum::serve(listener, telemetry::router(metrics))
            .with_graceful_shutdown(async move {
                let _ = stop.changed().await;
            })
            .await
    });
    tokio::spawn(async move {
        shutdown_signal().await;
        log("shutdown_requested", json!({}));
        let _ = stop_tx.send(true);
    });
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(config.reconcile.startup_delay_seconds)) => {},
        _ = stop_rx.changed() => { server.await??; return Ok(()); },
        result = &mut server => { result??; anyhow::bail!("health server stopped"); }
    }
    loop {
        if *stop_rx.borrow() {
            break;
        }
        // Finish the current cycle on SIGTERM rather than cancel a write in flight.
        if let Err(error) = reconciler.run_once(Instant::now()).await {
            log("reconcile_failed", json!({"error": error.to_string()}));
        }
        if *stop_rx.borrow() {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(config.reconcile.interval_seconds)) => {},
            _ = stop_rx.changed() => break,
            result = &mut server => { result??; anyhow::bail!("health server stopped"); }
        }
    }
    server.await??;
    Ok(())
}
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
