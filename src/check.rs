//! Health checks (spec §2.3): GET, exact status match, headers-only latency.

use std::sync::Arc;
use std::time::{Duration, Instant};

use jiff::Timestamp;

use crate::config::ServiceConfig;
use crate::schedule;
use crate::state::{AppState, ServiceStatus};

/// Hardcoded per spec §2.3.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// The one outbound HTTP client (spec §3): rustls with embedded roots, no redirects,
/// identifying User-Agent. The timeout is injectable so tests can shorten it.
pub fn build_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .user_agent(concat!("varde/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("static client configuration cannot fail to build")
}

/// Same as `build_client`, but skips TLS certificate verification entirely. Only ever
/// hand this to services with an explicit `skipTlsVerification` opt-in (e.g. LAN devices
/// with self-signed certs) — never to heartbeat/notify traffic, which talks to trusted
/// public endpoints.
pub fn build_insecure_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .user_agent(concat!("varde/", env!("CARGO_PKG_VERSION")))
        .danger_accept_invalid_certs(true)
        .build()
        .expect("static client configuration cannot fail to build")
}

/// One health check. Never fails: transport errors are results (down, no latency).
/// Completed responses always carry latency, up or down; the body is never read.
pub async fn check_once(client: &reqwest::Client, service: &ServiceConfig) -> ServiceStatus {
    let start = Instant::now();
    let response = client.get(&service.url).send().await;
    let latency_ms = start.elapsed().as_millis() as u64; // whole ms, truncated
    let last_checked = Timestamp::now();
    match response {
        Ok(response) => {
            let status_code = response.status().as_u16();
            let ok = status_code == service.ok_status_code;
            ServiceStatus {
                ok,
                last_checked,
                latency_ms: Some(latency_ms),
                error: (!ok).then(|| {
                    format!(
                        "unexpected status {status_code} (want {})",
                        service.ok_status_code
                    )
                }),
            }
        }
        Err(e) => ServiceStatus {
            ok: false,
            last_checked,
            latency_ms: None,
            error: Some(describe_error(&e)),
        },
    }
}

/// `reqwest::Error`'s own message is a generic wrapper ("error sending request for url
/// (...)") for every failure kind; the actual reason (timeout, connection refused,
/// certificate error, ...) lives further down the source chain.
fn describe_error(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(err) = source {
        message.push_str(": ");
        message.push_str(&err.to_string());
        source = err.source();
    }
    message
}

/// Immediate first check, then on schedule. Sleeping happens after the tick, until the
/// first occurrence strictly after now — a slow check delays its own next tick and missed
/// occurrences are skipped (spec §2.3). The body has no panic paths: `check_once` returns
/// plain data and `record` recovers poisoned locks.
pub async fn check_loop(client: reqwest::Client, service: ServiceConfig, state: Arc<AppState>) {
    loop {
        let status = check_once(&client, &service).await;
        tracing::debug!(
            service = service.service,
            ok = status.ok,
            latency_ms = status.latency_ms,
            "checked"
        );
        if !status.ok {
            let error = status.error.as_deref().unwrap_or("");
            tracing::warn!(
                service = service.service,
                latency_ms = status.latency_ms,
                error,
                "check failed"
            );
        }
        let previous = state.record(&service.service, status.clone());
        if previous.map(|p| p.ok) != Some(status.ok) {
            tracing::info!(
                service = service.service,
                ok = status.ok,
                "state transition"
            );
        }
        schedule::sleep_until_next(&service.schedule).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn service(url: &str, ok_status_code: u16) -> ServiceConfig {
        serde_json::from_value(serde_json::json!({
            "service": "svc",
            "schedule": "every 1 second",
            "url": url,
            "okStatusCode": ok_status_code
        }))
        .unwrap()
    }

    fn test_client() -> reqwest::Client {
        build_client(Duration::from_millis(500))
    }

    /// A minimal HTTPS server presenting a freshly generated self-signed certificate for
    /// 127.0.0.1, so tests can prove the strict client rejects it and the insecure client
    /// (`danger_accept_invalid_certs`) accepts it.
    async fn start_self_signed_server(
        status_code: u16,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        use tokio_rustls::rustls;

        let _ = rustls::crypto::ring::default_provider().install_default();

        let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()]).unwrap();
        let cert_der = cert.cert.der().clone();
        let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into());
        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Each test opens exactly one connection, so a single accept (no loop) suffices.
        let handle = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (stream, _) = listener.accept().await.unwrap();
            if let Ok(mut tls) = acceptor.accept(stream).await {
                let mut buf = [0u8; 1024];
                let _ = tls.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 {status_code} status\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                );
                let _ = tls.write_all(response.as_bytes()).await;
                let _ = tls.shutdown().await;
            }
        });
        (addr, handle)
    }

    #[tokio::test]
    async fn matching_status_is_up_with_latency() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let status = check_once(&test_client(), &service(&server.uri(), 200)).await;
        assert!(status.ok);
        assert!(status.latency_ms.is_some());
    }

    #[tokio::test]
    async fn wrong_status_is_down_with_latency() {
        // A completed response always records latency, even when down (spec §2.3).
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let status = check_once(&test_client(), &service(&server.uri(), 200)).await;
        assert!(!status.ok);
        assert!(status.latency_ms.is_some());
        assert_eq!(
            status.error.as_deref(),
            Some("unexpected status 500 (want 200)")
        );
    }

    #[tokio::test]
    async fn redirect_is_compared_as_is_and_not_followed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(301).insert_header("Location", "/redirected".to_string()),
            )
            .mount(&server)
            .await;
        // The redirect target must receive zero requests.
        Mock::given(method("GET"))
            .and(path("/redirected"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let status = check_once(&test_client(), &service(&server.uri(), 301)).await;
        assert!(status.ok, "301 matches okStatusCode 301");

        let status = check_once(&test_client(), &service(&server.uri(), 200)).await;
        assert!(!status.ok, "301 does not match okStatusCode 200");
        assert!(status.latency_ms.is_some());
        server.verify().await;
    }

    #[tokio::test]
    async fn timeout_is_down_without_latency() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)))
            .mount(&server)
            .await;
        let status = check_once(&test_client(), &service(&server.uri(), 200)).await;
        assert!(!status.ok);
        assert_eq!(status.latency_ms, None);
        assert!(status.error.is_some());
    }

    #[tokio::test]
    async fn connection_refused_is_down_without_latency() {
        // Bind and drop a listener so the port is known-unbound.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let url = format!("http://127.0.0.1:{port}");
        let status = check_once(&test_client(), &service(&url, 200)).await;
        assert!(!status.ok);
        assert_eq!(status.latency_ms, None);
        assert!(status.error.is_some());
    }

    #[tokio::test]
    async fn garbage_response_is_down_without_latency() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            use tokio::io::AsyncWriteExt;
            socket.write_all(b"this is not http\r\n\r\n").await.unwrap();
        });
        let url = format!("http://{addr}");
        let status = check_once(&test_client(), &service(&url, 200)).await;
        assert!(!status.ok);
        assert_eq!(status.latency_ms, None);
        assert!(status.error.is_some());
    }

    #[tokio::test]
    async fn strict_client_rejects_self_signed_cert() {
        let (addr, handle) = start_self_signed_server(200).await;
        let url = format!("https://{addr}");
        let status = check_once(&test_client(), &service(&url, 200)).await;
        assert!(!status.ok);
        assert_eq!(status.latency_ms, None);
        let error = status.error.expect("transport error expected");
        assert!(error.to_lowercase().contains("certificate"), "got: {error}");
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn insecure_client_accepts_self_signed_cert() {
        let (addr, handle) = start_self_signed_server(200).await;
        let url = format!("https://{addr}");
        let insecure_client = build_insecure_client(Duration::from_millis(500));
        let status = check_once(&insecure_client, &service(&url, 200)).await;
        assert!(status.ok, "error: {:?}", status.error);
        assert!(status.latency_ms.is_some());
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn latency_reflects_response_delay() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(50)))
            .mount(&server)
            .await;
        let status = check_once(&test_client(), &service(&server.uri(), 200)).await;
        assert!(status.ok);
        assert!(
            status.latency_ms.unwrap() >= 50,
            "got {:?}",
            status.latency_ms
        );
    }

    #[tokio::test]
    async fn check_loop_writes_state_and_survives_transitions() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let service = service(&server.uri(), 200);
        let config = serde_json::from_value(serde_json::json!({
            "services": [{
                "service": "svc", "schedule": "every 1 second",
                "url": service.url, "okStatusCode": 200
            }]
        }))
        .unwrap();
        let state = Arc::new(AppState::new(&config));
        let handle = tokio::spawn(check_loop(test_client(), service, state.clone()));

        // Immediate first run lands without waiting for a schedule boundary.
        let first_checked = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(status) = &state.snapshot()[0].1 {
                    assert!(status.ok);
                    break status.last_checked;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("first check should land immediately");

        // Wait for a second, same-status tick (last_checked advances, ok unchanged) —
        // the no-transition branch.
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(status) = &state.snapshot()[0].1
                    && status.last_checked > first_checked
                {
                    assert!(status.ok);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("a second scheduled tick should land");

        // Flip the mock to failing; the next scheduled tick must record the transition.
        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(status) = &state.snapshot()[0].1
                    && !status.ok
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("loop should keep ticking and record the down transition");
        handle.abort();
    }
}
