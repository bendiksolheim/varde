//! End-to-end test (spec §5.5): spawn the real binary with a temp config against a mock
//! upstream, poll `GET /`, flip the mock, observe the flip, SIGTERM, assert exit 0.

use std::process::{Child, Command};
use std::time::{Duration, Instant};

use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

async fn poll_until<F: Fn(&serde_json::Value) -> bool>(
    url: &str,
    deadline: Duration,
    predicate: F,
) -> serde_json::Value {
    let start = Instant::now();
    loop {
        if let Ok(response) = reqwest::get(url).await
            && let Ok(text) = response.text().await
            && let Ok(body) = serde_json::from_str::<serde_json::Value>(&text)
        {
            if predicate(&body) {
                return body;
            }
            if start.elapsed() > deadline {
                panic!("timed out waiting for endpoint condition; last body: {body}");
            }
        } else if start.elapsed() > deadline {
            panic!("timed out waiting for the endpoint to answer at all");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn binary_checks_services_and_shuts_down_cleanly() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "services": [{
                "service": "mock",
                "schedule": "every 1 seconds",
                "url": mock.uri(),
                "okStatusCode": 200
            }],
            "nodes": []
        })
        .to_string(),
    )
    .unwrap();

    // Fixed high port risks collisions; instead let the OS pick by probing a free port.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let child = Command::new(env!("CARGO_BIN_EXE_varde"))
        .env("CONFIG_PATH", &config_path)
        .env("PORT", port.to_string())
        .spawn()
        .unwrap();
    let mut child = KillOnDrop(child);
    let url = format!("http://127.0.0.1:{port}/");

    // The immediate first check lands and the endpoint reports it.
    let body = poll_until(&url, Duration::from_secs(10), |body| {
        body["services"][0]["ok"] == serde_json::json!(true)
    })
    .await;
    assert_eq!(body["operational"], serde_json::json!(true));
    assert_eq!(body["services"][0]["service"], serde_json::json!("mock"));

    // Flip the mock to failing: the endpoint must flip to 500 within a schedule interval.
    mock.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&mock)
        .await;
    poll_until(&url, Duration::from_secs(10), |body| {
        body["services"][0]["ok"] == serde_json::json!(false)
    })
    .await;
    let response = reqwest::get(&url).await.unwrap();
    assert_eq!(response.status().as_u16(), 500);

    // SIGTERM → graceful exit 0 (spec §2.8).
    let pid = child.0.id() as libc::pid_t;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "binary did not exit after SIGTERM"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(status.success(), "expected exit 0, got {status:?}");
}

/// Phase 4 e2e extension (spec §6): heartbeat pings arrive while everything is up,
/// killing the service produces a ntfy down-POST, restoring it produces the recovery
/// POST and the heartbeat resumes.
#[tokio::test]
async fn binary_heartbeats_and_notifies_through_an_outage() {
    let uuid = "12345678-1234-1234-1234-123456789012";
    let service_mock = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&service_mock)
        .await;
    // One upstream mock plays both healthchecks.io and ntfy.sh; paths disambiguate.
    let upstream = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .mount(&upstream)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "services": [{
                "service": "mock",
                "schedule": "every 1 seconds",
                "url": service_mock.uri(),
                "okStatusCode": 200
            }],
            "heartbeat": {
                "type": "healthchecks.io",
                "uuid": uuid,
                "schedule": "every 1 seconds"
            },
            "notify": [{
                "topic": "e2e-topic",
                "schedule": "every 1 seconds",
                "minutesBetween": 120
            }]
        })
        .to_string(),
    )
    .unwrap();

    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let child = Command::new(env!("CARGO_BIN_EXE_varde"))
        .env("CONFIG_PATH", &config_path)
        .env("PORT", port.to_string())
        .env("VARDE_HC_BASE_URL", upstream.uri())
        .env("VARDE_NTFY_BASE_URL", upstream.uri())
        .spawn()
        .unwrap();
    let _child = KillOnDrop(child);

    let requests = |kind: &'static str| {
        let upstream = &upstream;
        let uuid_path = format!("/{uuid}");
        async move {
            upstream
                .received_requests()
                .await
                .unwrap()
                .into_iter()
                .filter(|r| match kind {
                    "ping" => r.method == wiremock::http::Method::GET && r.url.path() == uuid_path,
                    "down" => {
                        r.method == wiremock::http::Method::POST
                            && r.url.path() == "/e2e-topic"
                            && r.headers.get("Title").is_some_and(|t| t == "Service down")
                    }
                    _ => {
                        r.method == wiremock::http::Method::POST
                            && r.url.path() == "/e2e-topic"
                            && r.headers
                                .get("Title")
                                .is_some_and(|t| t == "Services recovered")
                    }
                })
                .collect::<Vec<_>>()
        }
    };
    let wait_for = |kind: &'static str, min: usize| async move {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let matched = requests(kind).await;
            if matched.len() >= min {
                return matched;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {kind}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };

    // All up → heartbeat pings arrive on schedule; no notifications.
    wait_for("ping", 2).await;
    assert!(requests("down").await.is_empty());

    // Kill the service → a down notification with the exact §2.6 body; pings pause.
    service_mock.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&service_mock)
        .await;
    let down = wait_for("down", 1).await;
    assert_eq!(down[0].body, b"1 service down: mock");
    assert_eq!(
        down[0].headers.get("Tags").map(|t| t.to_str().unwrap()),
        Some("warning")
    );
    let pings_during_outage = requests("ping").await.len();

    // Restore → exactly one recovery message; heartbeat resumes.
    service_mock.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&service_mock)
        .await;
    let recovered = wait_for("recovered", 1).await;
    assert_eq!(recovered[0].body, b"All services back up");
    wait_for("ping", pings_during_outage + 1).await;
    assert_eq!(requests("recovered").await.len(), 1);
}

#[tokio::test]
async fn binary_exits_1_on_bad_config() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.json");
    std::fs::write(&config_path, r#"{"services": [{"service": ""}]}"#).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_varde"))
        .env("CONFIG_PATH", &config_path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        !output.stderr.is_empty(),
        "expected a readable error on stderr"
    );
}

#[tokio::test]
async fn binary_exits_1_on_bad_port() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.json");
    std::fs::write(&config_path, r#"{"services": []}"#).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_varde"))
        .env("CONFIG_PATH", &config_path)
        .env("PORT", "70000")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("PORT"), "got: {stderr}");
}
