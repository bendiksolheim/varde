//! The status endpoint (spec §2.7): `GET /` and nothing else.

use std::fmt;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::any;
use serde::Serialize;

use crate::state::AppState;

pub const DEFAULT_PORT: u16 = 3000;

#[derive(Debug, Serialize)]
struct StatusResponse {
    operational: bool,
    services: Vec<ServiceEntry>,
}

#[derive(Debug, Serialize)]
struct ServiceEntry {
    service: String,
    ok: Option<bool>,
    #[serde(rename = "lastChecked")]
    last_checked: Option<String>,
    #[serde(rename = "latencyMs")]
    latency_ms: Option<u64>,
}

pub fn router(state: Arc<AppState>) -> Router {
    // Deliberately trivial (spec §2.7): one route, everything else — including non-GET
    // on `/` — is 404, never 405.
    Router::new()
        .route("/", any(status_handler))
        .fallback(async || StatusCode::NOT_FOUND)
        .with_state(state)
}

async fn status_handler(method: Method, State(state): State<Arc<AppState>>) -> Response {
    if method != Method::GET {
        return StatusCode::NOT_FOUND.into_response();
    }
    let services: Vec<ServiceEntry> = state
        .snapshot()
        .into_iter()
        .map(|(service, status)| match status {
            Some(status) => ServiceEntry {
                service,
                ok: Some(status.ok),
                last_checked: Some(format_second_precision(status.last_checked)),
                latency_ms: status.latency_ms,
            },
            None => ServiceEntry {
                service,
                ok: None,
                last_checked: None,
                latency_ms: None,
            },
        })
        .collect();
    // Unchecked services don't count against operational (spec §2.7).
    let operational = services.iter().all(|entry| entry.ok != Some(false));
    let code = if operational {
        StatusCode::OK
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (
        code,
        Json(StatusResponse {
            operational,
            services,
        }),
    )
        .into_response()
}

/// UTC RFC 3339 at second precision, e.g. `2026-07-18T12:34:56Z` (spec §2.7).
fn format_second_precision(ts: jiff::Timestamp) -> String {
    jiff::Timestamp::from_second(ts.as_second())
        .expect("truncating to seconds keeps the timestamp in range")
        .to_string()
}

#[derive(Debug, PartialEq, Eq)]
pub struct PortError {
    value: String,
}

impl fmt::Display for PortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "PORT must be an integer in 1..=65535, got \"{}\"",
            self.value
        )
    }
}

impl std::error::Error for PortError {}

/// Resolve the listen port from the `PORT` env var value (spec §2.7): default 3000,
/// otherwise an integer in 1..=65535 — anything else is a startup error.
pub fn resolve_port(env_value: Option<&str>) -> Result<u16, PortError> {
    match env_value {
        None => Ok(DEFAULT_PORT),
        Some(raw) => match raw.parse::<u16>() {
            Ok(port) if port >= 1 => Ok(port),
            _ => Err(PortError {
                value: raw.to_string(),
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ServiceStatus;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn state_with(names: &[&str]) -> Arc<AppState> {
        let services: Vec<serde_json::Value> = names
            .iter()
            .map(|name| {
                serde_json::json!({
                    "service": name,
                    "schedule": "every 1 minute",
                    "url": "http://localhost:1",
                    "okStatusCode": 200
                })
            })
            .collect();
        let config = serde_json::from_value(serde_json::json!({ "services": services })).unwrap();
        Arc::new(AppState::new(&config))
    }

    async fn get(router: Router, method: &str, path: &str) -> (StatusCode, String) {
        let response = router
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let code = response.status();
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .map(|v| v.to_str().unwrap()),
            if code == StatusCode::NOT_FOUND {
                None
            } else {
                Some("application/json")
            }
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (code, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn empty_state_is_operational_with_null_entries() {
        let (code, body) = get(router(state_with(&["a", "b"])), "GET", "/").await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(
            body,
            r#"{"operational":true,"services":[{"service":"a","ok":null,"lastChecked":null,"latencyMs":null},{"service":"b","ok":null,"lastChecked":null,"latencyMs":null}]}"#
        );
    }

    #[tokio::test]
    async fn mixed_state_returns_500_with_exact_shape() {
        let state = state_with(&["up", "wrong-status", "unreachable", "not-yet"]);
        let ts: jiff::Timestamp = "2026-07-17T12:34:56.789Z".parse().unwrap();
        state.record(
            "up",
            ServiceStatus {
                ok: true,
                last_checked: ts,
                latency_ms: Some(42),
            },
        );
        state.record(
            "wrong-status",
            ServiceStatus {
                ok: false,
                last_checked: ts,
                latency_ms: Some(87), // down with latency: completed response (§2.3)
            },
        );
        state.record(
            "unreachable",
            ServiceStatus {
                ok: false,
                last_checked: ts,
                latency_ms: None, // down without latency: transport error
            },
        );
        let (code, body) = get(router(state), "GET", "/").await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        // Exact field names, config order, second-precision UTC timestamps, compact JSON.
        assert_eq!(
            body,
            r#"{"operational":false,"services":[{"service":"up","ok":true,"lastChecked":"2026-07-17T12:34:56Z","latencyMs":42},{"service":"wrong-status","ok":false,"lastChecked":"2026-07-17T12:34:56Z","latencyMs":87},{"service":"unreachable","ok":false,"lastChecked":"2026-07-17T12:34:56Z","latencyMs":null},{"service":"not-yet","ok":null,"lastChecked":null,"latencyMs":null}]}"#
        );
    }

    #[tokio::test]
    async fn all_up_returns_200() {
        let state = state_with(&["a"]);
        state.record(
            "a",
            ServiceStatus {
                ok: true,
                last_checked: "2026-07-17T00:00:00Z".parse().unwrap(),
                latency_ms: Some(1),
            },
        );
        let (code, body) = get(router(state), "GET", "/").await;
        assert_eq!(code, StatusCode::OK);
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["operational"], serde_json::json!(true));
        let re = parsed["services"][0]["lastChecked"].as_str().unwrap();
        assert_eq!(re, "2026-07-17T00:00:00Z");
    }

    #[tokio::test]
    async fn unknown_paths_and_methods_are_404() {
        for (method, path) in [
            ("GET", "/status"),
            ("GET", "/favicon.ico"),
            ("POST", "/"),
            ("DELETE", "/"),
            ("HEAD", "/"),
        ] {
            let (code, _) = get(router(state_with(&[])), method, path).await;
            assert_eq!(code, StatusCode::NOT_FOUND, "{method} {path}");
        }
    }

    #[tokio::test]
    async fn real_socket_smoke_test() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Spawn axum's future directly: a wrapper closure would leave its own
        // never-reached post-await lines uncovered.
        use std::future::IntoFuture;
        tokio::spawn(axum::serve(listener, router(state_with(&["a"]))).into_future());
        let body = reqwest::get(format!("http://{addr}/")).await.unwrap();
        assert_eq!(body.status().as_u16(), 200);
        let parsed: serde_json::Value = serde_json::from_str(&body.text().await.unwrap()).unwrap();
        assert_eq!(parsed["operational"], serde_json::json!(true));
    }

    #[test]
    fn port_resolution() {
        assert_eq!(resolve_port(None), Ok(DEFAULT_PORT));
        assert_eq!(resolve_port(Some("8080")), Ok(8080));
        assert_eq!(resolve_port(Some("65535")), Ok(65535));
        for bad in ["0", "65536", "-1", "abc", ""] {
            let err = resolve_port(Some(bad)).unwrap_err();
            assert!(err.to_string().contains(bad), "got: {err}");
            assert!(err.to_string().contains("1..=65535"));
        }
    }
}
