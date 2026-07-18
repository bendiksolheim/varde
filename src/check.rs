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

/// One health check. Never fails: transport errors are results (down, no latency).
/// Completed responses always carry latency, up or down; the body is never read.
pub async fn check_once(client: &reqwest::Client, service: &ServiceConfig) -> ServiceStatus {
    let start = Instant::now();
    let response = client.get(&service.url).send().await;
    let latency_ms = start.elapsed().as_millis() as u64; // whole ms, truncated
    let last_checked = Timestamp::now();
    match response {
        Ok(response) => ServiceStatus {
            ok: response.status().as_u16() == service.ok_status_code,
            last_checked,
            latency_ms: Some(latency_ms),
        },
        Err(_) => ServiceStatus {
            ok: false,
            last_checked,
            latency_ms: None,
        },
    }
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
            tracing::warn!(
                service = service.service,
                latency_ms = status.latency_ms,
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
