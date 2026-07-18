//! Process wiring only (spec §3): config → state → spawn loops → serve. Kept thin; unit
//! coverage excludes this file (justified in ci.yml) — the end-to-end test in tests/e2e.rs
//! exercises it: spawn binary, poll GET /, SIGTERM, assert exit 0.

use std::sync::Arc;

use tracing_subscriber::EnvFilter;
use varde::{check, config, server, state::AppState};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let path = config::config_path();
    let (config, warnings) = match config::load(&path) {
        Ok(loaded) => loaded,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    for warning in warnings {
        if warning.is_info() {
            tracing::info!("{warning}");
        } else {
            tracing::warn!("{warning}");
        }
    }

    let port = match server::resolve_port(std::env::var("PORT").ok().as_deref()) {
        Ok(port) => port,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    // Single-threaded runtime: the workload is a handful of timers and outbound calls.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime construction cannot fail");
    runtime.block_on(run(config, port));
}

async fn run(config: config::Config, port: u16) {
    let state = Arc::new(AppState::new(&config));
    let client = check::build_client(check::CHECK_TIMEOUT);

    // Heartbeat + notify loops arrive in Phase 4.
    for service in config.services.clone() {
        tokio::spawn(check::check_loop(client.clone(), service, state.clone()));
    }

    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("error: cannot bind 0.0.0.0:{port}: {e}");
            std::process::exit(1);
        }
    };
    tracing::info!(port, "varde listening");
    axum::serve(listener, server::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
    // SIGTERM/SIGINT: exit 0 immediately; in-flight checks are dropped (spec §2.8).
    tracing::info!("shutting down");
}

async fn shutdown_signal() {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("installing SIGTERM handler cannot fail");
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}
