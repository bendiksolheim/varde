//! Dead man's switch (spec §2.4): ping upstream only when everything checked is up;
//! a missing ping *is* the alarm, so failures here are logged, never fatal.

use std::sync::Arc;

use crate::config::HeartbeatConfig;
use crate::schedule;
use crate::state::{AppState, ServiceStatus};

pub const DEFAULT_HC_BASE_URL: &str = "https://hc-ping.com";
pub const DEFAULT_HTTPBIN_BASE_URL: &str = "https://httpbin.org";

/// Default ping base for the configured type, overridable via `VARDE_HC_BASE_URL`
/// (test seam; the config schema stays legacy-compatible).
pub fn base_url(heartbeat: &HeartbeatConfig, env_override: Option<String>) -> String {
    env_override.unwrap_or_else(|| {
        match heartbeat {
            HeartbeatConfig::HealthchecksIo { .. } => DEFAULT_HC_BASE_URL,
            HeartbeatConfig::Httpbin { .. } => DEFAULT_HTTPBIN_BASE_URL,
        }
        .to_string()
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum HeartbeatOutcome {
    /// Something is down — skipping the ping is the alarm (spec §2.4.3).
    Skipped,
    Sent,
    /// Transport error or non-2xx (spec §2.4.4): logged; retried at the next tick.
    Failed,
}

/// One heartbeat tick over the given snapshot. Services never checked yet are vacuously
/// OK (spec §2.4.1), which keeps the heartbeat correct during the startup window.
pub async fn heartbeat_tick(
    client: &reqwest::Client,
    snapshot: &[(String, Option<ServiceStatus>)],
    heartbeat: &HeartbeatConfig,
    base_url: &str,
) -> HeartbeatOutcome {
    let all_up = snapshot
        .iter()
        .all(|(_, status)| status.as_ref().is_none_or(|s| s.ok));
    if !all_up {
        tracing::debug!("heartbeat skipped: at least one service is down");
        return HeartbeatOutcome::Skipped;
    }
    let url = match heartbeat {
        HeartbeatConfig::HealthchecksIo { uuid, .. } => format!("{base_url}/{uuid}"),
        HeartbeatConfig::Httpbin { .. } => format!("{base_url}/get"),
    };
    match client.get(&url).send().await {
        Ok(response) if response.status().is_success() => {
            tracing::info!("heartbeat sent");
            HeartbeatOutcome::Sent
        }
        Ok(response) => {
            tracing::warn!(status = %response.status(), "heartbeat ping answered non-2xx");
            HeartbeatOutcome::Failed
        }
        Err(e) => {
            tracing::warn!(error = %e, "heartbeat ping failed");
            HeartbeatOutcome::Failed
        }
    }
}

/// First tick at the first *scheduled* occurrence, not at startup (spec §2.4), giving
/// the initial round of checks time to land.
pub async fn heartbeat_loop(
    client: reqwest::Client,
    state: Arc<AppState>,
    heartbeat: HeartbeatConfig,
    base_url: String,
) {
    loop {
        schedule::sleep_until_next(heartbeat.schedule()).await;
        heartbeat_tick(&client, &state.snapshot(), &heartbeat, &base_url).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const UUID: &str = "12345678-1234-1234-1234-123456789012";

    fn hc_heartbeat() -> HeartbeatConfig {
        serde_json::from_value(serde_json::json!({
            "type": "healthchecks.io", "uuid": UUID, "schedule": "every 1 minute"
        }))
        .unwrap()
    }

    fn httpbin_heartbeat() -> HeartbeatConfig {
        serde_json::from_value(serde_json::json!({
            "type": "httpbin", "schedule": "every 1 minute"
        }))
        .unwrap()
    }

    fn client() -> reqwest::Client {
        crate::check::build_client(std::time::Duration::from_millis(500))
    }

    fn entry(name: &str, ok: Option<bool>) -> (String, Option<ServiceStatus>) {
        (
            name.to_string(),
            ok.map(|ok| ServiceStatus {
                ok,
                last_checked: "2026-07-18T12:00:00Z".parse().unwrap(),
                latency_ms: None,
            }),
        )
    }

    #[tokio::test]
    async fn all_up_pings_healthchecks_uuid_exactly_once() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/{UUID}")))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let snapshot = vec![entry("a", Some(true)), entry("b", Some(true))];
        let outcome = heartbeat_tick(&client(), &snapshot, &hc_heartbeat(), &server.uri()).await;
        assert_eq!(outcome, HeartbeatOutcome::Sent);
        server.verify().await;
    }

    #[tokio::test]
    async fn all_up_pings_httpbin_get() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/get"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let outcome = heartbeat_tick(
            &client(),
            &[entry("a", Some(true))],
            &httpbin_heartbeat(),
            &server.uri(),
        )
        .await;
        assert_eq!(outcome, HeartbeatOutcome::Sent);
        server.verify().await;
    }

    #[tokio::test]
    async fn any_down_sends_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let snapshot = vec![entry("a", Some(true)), entry("b", Some(false))];
        let outcome = heartbeat_tick(&client(), &snapshot, &hc_heartbeat(), &server.uri()).await;
        assert_eq!(outcome, HeartbeatOutcome::Skipped);
        server.verify().await;
    }

    #[tokio::test]
    async fn empty_and_unchecked_states_are_vacuously_ok() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(2)
            .mount(&server)
            .await;
        // Nothing configured / nothing checked yet → ping.
        let outcome = heartbeat_tick(&client(), &[], &hc_heartbeat(), &server.uri()).await;
        assert_eq!(outcome, HeartbeatOutcome::Sent);
        // Unchecked services are ignored while checked ones decide.
        let snapshot = vec![entry("a", None), entry("b", Some(true))];
        let outcome = heartbeat_tick(&client(), &snapshot, &hc_heartbeat(), &server.uri()).await;
        assert_eq!(outcome, HeartbeatOutcome::Sent);
        server.verify().await;
    }

    #[tokio::test]
    async fn unchecked_do_not_mask_a_down_service() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let snapshot = vec![entry("a", None), entry("b", Some(false))];
        let outcome = heartbeat_tick(&client(), &snapshot, &hc_heartbeat(), &server.uri()).await;
        assert_eq!(outcome, HeartbeatOutcome::Skipped);
        server.verify().await;
    }

    #[tokio::test]
    async fn transport_failure_is_logged_not_fatal() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let unbound = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        drop(listener);
        let outcome = heartbeat_tick(&client(), &[], &hc_heartbeat(), &unbound).await;
        assert_eq!(outcome, HeartbeatOutcome::Failed);
    }

    #[tokio::test]
    async fn non_2xx_ping_counts_as_failure() {
        // Legacy treated a 404 from a bad UUID as success; varde does not (spec §2.4.4).
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        let outcome = heartbeat_tick(&client(), &[], &hc_heartbeat(), &server.uri()).await;
        assert_eq!(outcome, HeartbeatOutcome::Failed);
        server.verify().await;
    }

    #[tokio::test]
    async fn loop_first_tick_waits_for_scheduled_occurrence() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/{UUID}")))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let heartbeat: HeartbeatConfig = serde_json::from_value(serde_json::json!({
            "type": "healthchecks.io", "uuid": UUID, "schedule": "every 1 second"
        }))
        .unwrap();
        let config = serde_json::from_value(serde_json::json!({ "services": [] })).unwrap();
        let state = Arc::new(AppState::new(&config));
        let handle = tokio::spawn(heartbeat_loop(client(), state, heartbeat, server.uri()));
        // No immediate ping at spawn; the first scheduled occurrence is ≤1s away.
        assert!(server.received_requests().await.unwrap().is_empty());
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if !server.received_requests().await.unwrap().is_empty() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("heartbeat should fire at the first scheduled occurrence");
        handle.abort();
    }

    #[test]
    fn base_url_resolution() {
        assert_eq!(base_url(&hc_heartbeat(), None), DEFAULT_HC_BASE_URL);
        assert_eq!(
            base_url(&httpbin_heartbeat(), None),
            DEFAULT_HTTPBIN_BASE_URL
        );
        assert_eq!(
            base_url(&hc_heartbeat(), Some("http://mock".into())),
            "http://mock"
        );
    }
}
