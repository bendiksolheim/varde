//! Process wiring only (spec §3): config → state → spawn loops → serve. Kept thin; unit
//! coverage excludes this file (justified in ci.yml) — the end-to-end test in tests/e2e.rs
//! exercises it: spawn binary, poll GET /, SIGTERM, assert exit 0.

use std::sync::Arc;

use tracing_subscriber::EnvFilter;
use varde::{check, config, heartbeat, notify, server, state::AppState};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let mut args = std::env::args().skip(1);
    if let Some(cmd) = args.next() {
        if cmd != "test-notify" {
            eprintln!("error: unknown argument \"{cmd}\"");
            std::process::exit(1);
        }
        let only = match parse_only_arg(args) {
            Ok(only) => only,
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime construction cannot fail");
        std::process::exit(runtime.block_on(run_test_notify(only)));
    }

    let path = config::config_path();
    let config = match config::load(&path) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

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

    for service in config.services.clone() {
        tokio::spawn(check::check_loop(client.clone(), service, state.clone()));
    }
    if let Some(hb) = config.heartbeat.clone() {
        // VARDE_HC_BASE_URL / VARDE_NTFY_BASE_URL are test seams (see README); the
        // config schema stays legacy-compatible.
        let base = heartbeat::base_url(&hb, std::env::var("VARDE_HC_BASE_URL").ok());
        tokio::spawn(heartbeat::heartbeat_loop(
            client.clone(),
            state.clone(),
            hb,
            base,
        ));
    }
    for entry in config.notify.clone() {
        // VARDE_NTFY_BASE_URL / VARDE_TELEGRAM_BASE_URL / VARDE_PUSHOVER_BASE_URL are
        // test seams (see README); the config schema stays legacy-compatible.
        let base = notify::base_url(&entry, std::env::var(notify::env_var(&entry)).ok());
        tokio::spawn(notify::notify_loop(
            client.clone(),
            state.clone(),
            entry,
            base,
        ));
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

/// `varde test-notify [--only <type>]`: manually fire every configured notify entry (or
/// just one type), bypassing the rate-limit/outage state machine — verifies credentials
/// without waiting for a real outage. Deliberately has no HTTP surface: invoked via
/// `docker exec <container> /varde test-notify` (README), never reachable over the network.
async fn run_test_notify(only: Option<String>) -> i32 {
    let config = match config::load(&config::config_path()) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    let entries: Vec<_> = config
        .notify
        .iter()
        .filter(|entry| only.as_deref().is_none_or(|kind| kind == entry.kind()))
        .collect();
    if entries.is_empty() {
        match only {
            Some(kind) => eprintln!("error: no notify entry configured with type \"{kind}\""),
            None => eprintln!("error: no notify entries configured"),
        }
        return 1;
    }

    let client = check::build_client(check::CHECK_TIMEOUT);
    let mut all_ok = true;
    for entry in entries {
        let base = notify::base_url(entry, std::env::var(notify::env_var(entry)).ok());
        let ok = notify::send_test(&client, entry, &base).await;
        all_ok &= ok;
        println!(
            "{} ({}): {}",
            entry.kind(),
            notify::recipient(entry),
            if ok { "sent" } else { "FAILED" }
        );
    }
    if all_ok { 0 } else { 1 }
}

/// Parses the arguments following `test-notify`: nothing, or `--only <type>`.
fn parse_only_arg(mut args: impl Iterator<Item = String>) -> Result<Option<String>, String> {
    match args.next() {
        None => Ok(None),
        Some(flag) if flag == "--only" => match args.next() {
            Some(kind) => Ok(Some(kind)),
            None => Err("--only requires a value".to_string()),
        },
        Some(other) => Err(format!("unknown argument \"{other}\"")),
    }
}

async fn shutdown_signal() {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("installing SIGTERM handler cannot fail");
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}
