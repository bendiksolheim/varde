//! Ntfy notifications (spec §2.5–§2.6): rate-limited down messages while an outage
//! lasts, one recovery message when it ends. This is a redesign of the legacy behavior
//! (recovery messages are new; state updates only on successful send; 2xx required).

use std::sync::Arc;

use jiff::Timestamp;

use crate::config::NotifyConfig;
use crate::schedule;
use crate::state::{AppState, ServiceStatus};

pub const DEFAULT_NTFY_BASE_URL: &str = "https://ntfy.sh";

/// Per-entry state (spec §2.5): two topics never share a rate-limit window.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct NotifyState {
    pub last_sent: Option<Timestamp>,
    pub was_down: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum NotifyOutcome {
    /// Nothing failing, nothing to recover from.
    Quiet,
    /// Failing, but inside the rate-limit window.
    RateLimited,
    SentDown,
    /// Send failed (transport or non-2xx): `last_sent` untouched → retried next tick.
    SendFailed,
    Recovered,
    /// Recovery send failed: `was_down` stays true → retried next tick.
    RecoveryFailed,
}

/// Exact legacy wording (spec §2.6): singular/plural `service`, comma-joined with a
/// final `and`, no Oxford comma. `None` when nothing is failing.
pub fn format_notification_message(failing: &[&str]) -> Option<String> {
    match failing {
        [] => None,
        [only] => Some(format!("1 service down: {only}")),
        [init @ .., last] => Some(format!(
            "{} services down: {} and {}",
            failing.len(),
            init.join(", "),
            last
        )),
    }
}

/// One notify tick (spec §2.5). `now` is a parameter so tests drive time explicitly.
pub async fn notify_tick(
    client: &reqwest::Client,
    snapshot: &[(String, Option<ServiceStatus>)],
    entry: &NotifyConfig,
    state: &mut NotifyState,
    now: Timestamp,
    base_url: &str,
) -> NotifyOutcome {
    // Failing services in config order (spec §2.5.1) — wording depends on order.
    let failing: Vec<&str> = snapshot
        .iter()
        .filter(|(_, status)| status.as_ref().is_some_and(|s| !s.ok))
        .map(|(name, _)| name.as_str())
        .collect();

    let Some(body) = format_notification_message(&failing) else {
        if !state.was_down {
            return NotifyOutcome::Quiet;
        }
        // Outage just ended. Recovery is never rate-limited (at most one per outage by
        // construction), and it resets the window: the first down-message of the next
        // outage always sends immediately (spec §2.5.3).
        if post(
            client,
            base_url,
            &entry.topic,
            "Services recovered",
            "white_check_mark",
            "All services back up".to_string(),
        )
        .await
        {
            tracing::info!(topic = entry.topic, "recovery notification sent");
            state.was_down = false;
            state.last_sent = None;
            return NotifyOutcome::Recovered;
        }
        return NotifyOutcome::RecoveryFailed;
    };

    // `was_down` tracks observed reality unconditionally; only `last_sent` is gated on
    // send success (interview decision, rust.md §7).
    state.was_down = true;
    let due = match state.last_sent {
        None => true,
        // Strictly greater, in fractional minutes — inherited from legacy (spec §2.5).
        Some(sent_at) => now.duration_since(sent_at).as_secs_f64() / 60.0 > entry.minutes_between,
    };
    if !due {
        tracing::debug!(topic = entry.topic, "notification rate-limited");
        return NotifyOutcome::RateLimited;
    }
    if post(
        client,
        base_url,
        &entry.topic,
        "Service down",
        "warning",
        body,
    )
    .await
    {
        tracing::info!(topic = entry.topic, "down notification sent");
        state.last_sent = Some(now);
        return NotifyOutcome::SentDown;
    }
    NotifyOutcome::SendFailed
}

/// A send succeeds iff the POST completes with 2xx (spec §2.5.5).
async fn post(
    client: &reqwest::Client,
    base_url: &str,
    topic: &str,
    title: &str,
    tags: &str,
    body: String,
) -> bool {
    let result = client
        .post(format!("{base_url}/{topic}"))
        .header("Title", title)
        .header("Tags", tags)
        .body(body)
        .send()
        .await;
    match result {
        Ok(response) if response.status().is_success() => true,
        Ok(response) => {
            tracing::warn!(topic, status = %response.status(), "ntfy answered non-2xx");
            false
        }
        Err(e) => {
            tracing::warn!(topic, error = %e, "ntfy send failed");
            false
        }
    }
}

/// First tick at the first scheduled occurrence (spec §2.8); state lives here, one
/// instance per notify entry.
pub async fn notify_loop(
    client: reqwest::Client,
    app: Arc<AppState>,
    entry: NotifyConfig,
    base_url: String,
) {
    let mut state = NotifyState::default();
    loop {
        schedule::sleep_until_next(&entry.schedule).await;
        let now = Timestamp::now();
        notify_tick(&client, &app.snapshot(), &entry, &mut state, now, &base_url).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn entry_with(minutes_between: f64) -> NotifyConfig {
        serde_json::from_value(serde_json::json!({
            "topic": "my-topic",
            "schedule": "every 1 minute",
            "minutesBetween": minutes_between
        }))
        .unwrap()
    }

    fn client() -> reqwest::Client {
        crate::check::build_client(std::time::Duration::from_millis(500))
    }

    fn snap(entries: &[(&str, Option<bool>)]) -> Vec<(String, Option<ServiceStatus>)> {
        entries
            .iter()
            .map(|(name, ok)| {
                (
                    name.to_string(),
                    ok.map(|ok| ServiceStatus {
                        ok,
                        last_checked: "2026-07-18T12:00:00Z".parse().unwrap(),
                        latency_ms: None,
                    }),
                )
            })
            .collect()
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    async fn expect_down_post(server: &MockServer, body: &str, count: u64) {
        Mock::given(method("POST"))
            .and(path("/my-topic"))
            .and(header("Title", "Service down"))
            .and(header("Tags", "warning"))
            .and(body_string(body.to_string()))
            .respond_with(ResponseTemplate::new(200))
            .expect(count)
            .mount(server)
            .await;
    }

    async fn expect_recovery_post(server: &MockServer, count: u64) {
        Mock::given(method("POST"))
            .and(path("/my-topic"))
            .and(header("Title", "Services recovered"))
            .and(header("Tags", "white_check_mark"))
            .and(body_string("All services back up".to_string()))
            .respond_with(ResponseTemplate::new(200))
            .expect(count)
            .mount(server)
            .await;
    }

    #[test]
    fn message_format_vectors() {
        // Inherited from the legacy suite (spec §2.6).
        assert_eq!(format_notification_message(&[]), None);
        assert_eq!(
            format_notification_message(&["my-service-0"]).unwrap(),
            "1 service down: my-service-0"
        );
        assert_eq!(
            format_notification_message(&["my-service-0", "my-service-1"]).unwrap(),
            "2 services down: my-service-0 and my-service-1"
        );
        assert_eq!(
            format_notification_message(&["my-service-0", "my-service-1", "my-service-2"]).unwrap(),
            "3 services down: my-service-0, my-service-1 and my-service-2"
        );
        assert_eq!(
            format_notification_message(&["a", "b", "c", "d"]).unwrap(),
            "4 services down: a, b, c and d"
        );
        // Names containing commas are joined verbatim; no escaping.
        assert_eq!(
            format_notification_message(&["x, y", "z"]).unwrap(),
            "2 services down: x, y and z"
        );
    }

    #[tokio::test]
    async fn all_up_never_down_stays_quiet() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let mut state = NotifyState::default();
        let outcome = notify_tick(
            &client(),
            &snap(&[("a", Some(true)), ("b", None)]),
            &entry_with(120.0),
            &mut state,
            ts("2026-07-18T12:00:00Z"),
            &server.uri(),
        )
        .await;
        assert_eq!(outcome, NotifyOutcome::Quiet);
        assert_eq!(state, NotifyState::default());
        server.verify().await;
    }

    #[tokio::test]
    async fn first_down_sends_verbatim_message_and_updates_state() {
        let server = MockServer::start().await;
        // Only the down service is named; the up one is not.
        expect_down_post(&server, "1 service down: b", 1).await;
        let mut state = NotifyState::default();
        let now = ts("2026-07-18T12:00:00Z");
        let outcome = notify_tick(
            &client(),
            &snap(&[("a", Some(true)), ("b", Some(false))]),
            &entry_with(120.0),
            &mut state,
            now,
            &server.uri(),
        )
        .await;
        assert_eq!(outcome, NotifyOutcome::SentDown);
        assert_eq!(state.last_sent, Some(now));
        assert!(state.was_down);
        server.verify().await;
    }

    #[tokio::test]
    async fn rate_limit_boundaries() {
        let server = MockServer::start().await;
        expect_down_post(&server, "1 service down: a", 2).await;
        let entry = entry_with(120.0);
        let down = snap(&[("a", Some(false))]);
        let mut state = NotifyState::default();
        let t0 = ts("2026-07-18T12:00:00Z");
        let c = client();

        assert_eq!(
            notify_tick(&c, &down, &entry, &mut state, t0, &server.uri()).await,
            NotifyOutcome::SentDown
        );
        // Under the window: no request.
        let t1 = ts("2026-07-18T13:00:00Z");
        assert_eq!(
            notify_tick(&c, &down, &entry, &mut state, t1, &server.uri()).await,
            NotifyOutcome::RateLimited
        );
        // Exactly minutesBetween: "more than" is strict — still no send (spec §2.5).
        let t2 = ts("2026-07-18T14:00:00Z");
        assert_eq!(
            notify_tick(&c, &down, &entry, &mut state, t2, &server.uri()).await,
            NotifyOutcome::RateLimited
        );
        // Past the window: second send, last_sent advances.
        let t3 = ts("2026-07-18T14:00:01Z");
        assert_eq!(
            notify_tick(&c, &down, &entry, &mut state, t3, &server.uri()).await,
            NotifyOutcome::SentDown
        );
        assert_eq!(state.last_sent, Some(t3));
        server.verify().await;
    }

    #[tokio::test]
    async fn recovery_flow_resets_rate_limit_and_is_not_rate_limited() {
        let server = MockServer::start().await;
        expect_down_post(&server, "1 service down: a", 2).await;
        expect_recovery_post(&server, 1).await;
        let entry = entry_with(120.0);
        let down = snap(&[("a", Some(false))]);
        let up = snap(&[("a", Some(true))]);
        let mut state = NotifyState::default();
        let c = client();

        let t0 = ts("2026-07-18T12:00:00Z");
        assert_eq!(
            notify_tick(&c, &down, &entry, &mut state, t0, &server.uri()).await,
            NotifyOutcome::SentDown
        );
        // Recovery fires immediately even though a down-message was just sent.
        let t1 = ts("2026-07-18T12:01:00Z");
        assert_eq!(
            notify_tick(&c, &up, &entry, &mut state, t1, &server.uri()).await,
            NotifyOutcome::Recovered
        );
        assert_eq!(
            state,
            NotifyState {
                last_sent: None,
                was_down: false
            }
        );
        // Next tick with everything up: nothing.
        let t2 = ts("2026-07-18T12:02:00Z");
        assert_eq!(
            notify_tick(&c, &up, &entry, &mut state, t2, &server.uri()).await,
            NotifyOutcome::Quiet
        );
        // A fresh outage minutes later sends immediately: the window never carries
        // over between outages (spec §2.5.3).
        let t3 = ts("2026-07-18T12:03:00Z");
        assert_eq!(
            notify_tick(&c, &down, &entry, &mut state, t3, &server.uri()).await,
            NotifyOutcome::SentDown
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn failed_down_send_is_retried_because_state_is_not_updated() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500)) // non-2xx = failure (spec §2.5.5)
            .expect(2)
            .mount(&server)
            .await;
        let entry = entry_with(120.0);
        let down = snap(&[("a", Some(false))]);
        let mut state = NotifyState::default();
        let c = client();

        let t0 = ts("2026-07-18T12:00:00Z");
        assert_eq!(
            notify_tick(&c, &down, &entry, &mut state, t0, &server.uri()).await,
            NotifyOutcome::SendFailed
        );
        // was_down tracks reality even though the send failed; last_sent does not.
        assert!(state.was_down);
        assert_eq!(state.last_sent, None);
        // Next tick retries immediately — not rate-limited by the failed attempt.
        let t1 = ts("2026-07-18T12:01:00Z");
        assert_eq!(
            notify_tick(&c, &down, &entry, &mut state, t1, &server.uri()).await,
            NotifyOutcome::SendFailed
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn transport_failure_counts_as_failed_send() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let unbound = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        drop(listener);
        let mut state = NotifyState::default();
        let outcome = notify_tick(
            &client(),
            &snap(&[("a", Some(false))]),
            &entry_with(0.0),
            &mut state,
            ts("2026-07-18T12:00:00Z"),
            &unbound,
        )
        .await;
        assert_eq!(outcome, NotifyOutcome::SendFailed);
        assert_eq!(state.last_sent, None);
    }

    #[tokio::test]
    async fn failed_recovery_send_keeps_was_down_and_retries() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;
        let entry = entry_with(120.0);
        let up = snap(&[("a", Some(true))]);
        let mut state = NotifyState {
            last_sent: Some(ts("2026-07-18T11:00:00Z")),
            was_down: true,
        };
        let c = client();
        let outcome = notify_tick(
            &c,
            &up,
            &entry,
            &mut state,
            ts("2026-07-18T12:00:00Z"),
            &server.uri(),
        )
        .await;
        assert_eq!(outcome, NotifyOutcome::RecoveryFailed);
        assert!(state.was_down, "retried at the next tick");
        server.verify().await;

        // The retry succeeds once ntfy answers.
        let server2 = MockServer::start().await;
        expect_recovery_post(&server2, 1).await;
        let outcome = notify_tick(
            &c,
            &up,
            &entry,
            &mut state,
            ts("2026-07-18T12:01:00Z"),
            &server2.uri(),
        )
        .await;
        assert_eq!(outcome, NotifyOutcome::Recovered);
        server2.verify().await;
    }

    #[tokio::test]
    async fn two_entries_have_independent_state() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(2)
            .mount(&server)
            .await;
        let entry_a = entry_with(120.0);
        let entry_b: NotifyConfig = serde_json::from_value(serde_json::json!({
            "topic": "other-topic", "schedule": "every 1 minute", "minutesBetween": 120
        }))
        .unwrap();
        let down = snap(&[("a", Some(false))]);
        let mut state_a = NotifyState::default();
        let mut state_b = NotifyState::default();
        let c = client();
        let t0 = ts("2026-07-18T12:00:00Z");
        // Topic A sends; topic B's window is untouched and it also sends.
        assert_eq!(
            notify_tick(&c, &down, &entry_a, &mut state_a, t0, &server.uri()).await,
            NotifyOutcome::SentDown
        );
        assert_eq!(
            notify_tick(&c, &down, &entry_b, &mut state_b, t0, &server.uri()).await,
            NotifyOutcome::SentDown
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn zero_minutes_between_sends_every_tick() {
        let server = MockServer::start().await;
        expect_down_post(&server, "1 service down: a", 3).await;
        let entry = entry_with(0.0);
        let down = snap(&[("a", Some(false))]);
        let mut state = NotifyState::default();
        let c = client();
        for second in ["00", "01", "02"] {
            let now = ts(&format!("2026-07-18T12:00:{second}Z"));
            assert_eq!(
                notify_tick(&c, &down, &entry, &mut state, now, &server.uri()).await,
                NotifyOutcome::SentDown,
                "at :{second}"
            );
        }
        server.verify().await;
    }

    #[tokio::test]
    async fn loop_ticks_on_schedule_with_local_state() {
        let server = MockServer::start().await;
        expect_down_post(&server, "1 service down: a", 1).await;
        let config = serde_json::from_value(serde_json::json!({
            "services": [{
                "service": "a", "schedule": "every 1 minute",
                "url": "http://localhost:1", "okStatusCode": 200
            }]
        }))
        .unwrap();
        let app = Arc::new(AppState::new(&config));
        app.record(
            "a",
            ServiceStatus {
                ok: false,
                last_checked: "2026-07-18T12:00:00Z".parse().unwrap(),
                latency_ms: None,
            },
        );
        let entry: NotifyConfig = serde_json::from_value(serde_json::json!({
            "topic": "my-topic", "schedule": "every 1 second", "minutesBetween": 120
        }))
        .unwrap();
        let handle = tokio::spawn(notify_loop(client(), app, entry, server.uri()));
        // First tick waits for the first scheduled occurrence, then sends once and is
        // rate-limited afterwards.
        assert!(server.received_requests().await.unwrap().is_empty());
        tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
        handle.abort();
        server.verify().await;
    }
}
