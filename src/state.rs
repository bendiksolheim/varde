//! Shared in-memory state (spec §3). The latest result per service — no history.

use std::collections::HashMap;
use std::sync::RwLock;

use jiff::Timestamp;

use crate::config::Config;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStatus {
    pub ok: bool,
    pub last_checked: Timestamp,
    pub latency_ms: Option<u64>,
}

pub struct AppState {
    /// Configured service names in config order — the endpoint renders every configured
    /// service, checked or not (spec §2.7).
    service_names: Vec<String>,
    statuses: RwLock<HashMap<String, ServiceStatus>>,
}

impl AppState {
    pub fn new(config: &Config) -> Self {
        Self {
            service_names: config.services.iter().map(|s| s.service.clone()).collect(),
            statuses: RwLock::new(HashMap::new()),
        }
    }

    /// Overwrite the service's entry, returning the previous one (for transition logs).
    pub fn record(&self, service: &str, status: ServiceStatus) -> Option<ServiceStatus> {
        // Poisoning is unreachable: no code panics while holding the lock (plain map ops).
        self.statuses
            .write()
            .expect("state lock cannot be poisoned")
            .insert(service.to_string(), status)
    }

    /// Latest results in config order; `None` for services not yet checked.
    pub fn snapshot(&self) -> Vec<(String, Option<ServiceStatus>)> {
        let statuses = self.statuses.read().expect("state lock cannot be poisoned");
        self.service_names
            .iter()
            .map(|name| (name.clone(), statuses.get(name).cloned()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(names: &[&str]) -> Config {
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
        serde_json::from_value(serde_json::json!({ "services": services })).unwrap()
    }

    fn status(ok: bool) -> ServiceStatus {
        ServiceStatus {
            ok,
            last_checked: "2026-07-18T12:00:00Z".parse().unwrap(),
            latency_ms: Some(42),
        }
    }

    #[test]
    fn snapshot_preserves_config_order_with_none_for_unchecked() {
        let state = AppState::new(&test_config(&["b", "a", "c"]));
        state.record("a", status(true));
        let snapshot = state.snapshot();
        assert_eq!(
            snapshot,
            vec![
                ("b".to_string(), None),
                ("a".to_string(), Some(status(true))),
                ("c".to_string(), None),
            ]
        );
    }

    #[test]
    fn record_overwrites_and_returns_previous() {
        let state = AppState::new(&test_config(&["a"]));
        assert_eq!(state.record("a", status(true)), None);
        assert_eq!(state.record("a", status(false)), Some(status(true)));
        assert_eq!(state.snapshot()[0].1, Some(status(false)));
    }
}
