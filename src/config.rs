//! Configuration loading and validation
//!
//! Parsed directly into typed structs and semantically validated. Every struct/variant
//! denies unknown fields, so a typo or a stray field anywhere fails the load instead of
//! silently vanishing.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::schedule::Schedule;

pub const DEFAULT_CONFIG_PATH: &str = "/config/config.json";

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub services: Vec<ServiceConfig>,
    #[serde(default)]
    pub heartbeat: Option<HeartbeatConfig>,
    #[serde(default)]
    pub notify: Vec<NotifyConfig>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub service: String,
    // `try_from = "String"` on Schedule parses each expression exactly once, at startup.
    //  A bad expression fails deserialization naming the field and the original expression.
    pub schedule: Schedule,
    pub url: String,
    #[serde(rename = "okStatusCode")]
    pub ok_status_code: u16,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum HeartbeatConfig {
    #[serde(rename = "healthchecks.io")]
    HealthchecksIo { uuid: String, schedule: Schedule },
    #[serde(rename = "httpbin")]
    Httpbin { schedule: Schedule },
}

impl HeartbeatConfig {
    pub fn schedule(&self) -> &Schedule {
        match self {
            Self::HealthchecksIo { schedule, .. } | Self::Httpbin { schedule } => schedule,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NtfyEntry {
    pub topic: String,
    pub schedule: Schedule,
    #[serde(rename = "minutesBetween")]
    pub minutes_between: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramEntry {
    #[serde(rename = "botToken")]
    pub bot_token: String,
    #[serde(rename = "chatId")]
    pub chat_id: String,
    pub schedule: Schedule,
    #[serde(rename = "minutesBetween")]
    pub minutes_between: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushoverEntry {
    #[serde(rename = "apiToken")]
    pub api_token: String,
    #[serde(rename = "userKey")]
    pub user_key: String,
    #[serde(default)]
    pub priority: Option<i8>,
    pub schedule: Schedule,
    #[serde(rename = "minutesBetween")]
    pub minutes_between: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum NotifyConfig {
    #[serde(rename = "ntfy")]
    Ntfy(NtfyEntry),
    #[serde(rename = "telegram")]
    Telegram(TelegramEntry),
    #[serde(rename = "pushover")]
    Pushover(PushoverEntry),
}

#[derive(Debug)]
pub enum ConfigError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Json {
        source: serde_json::Error,
    },
    Shape {
        location: String,
        message: String,
    },
    EmptyServiceName {
        index: usize,
    },
    DuplicateService {
        name: String,
    },
    StatusCodeRange {
        service: String,
        code: u16,
    },
    EmptyUuid,
    NegativeMinutesBetween {
        index: usize,
        value: f64,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(f, "cannot read config file {}: {source}", path.display())
            }
            Self::Json { source } => write!(f, "config is not valid JSON: {source}"),
            Self::Shape { location, message } => {
                write!(f, "invalid config at `{location}`: {message}")
            }
            Self::EmptyServiceName { index } => {
                write!(f, "services[{index}].service must not be empty")
            }
            Self::DuplicateService { name } => {
                write!(f, "duplicate service name \"{name}\"")
            }
            Self::StatusCodeRange { service, code } => write!(
                f,
                "service \"{service}\" has okStatusCode {code} outside 1..=599"
            ),
            Self::EmptyUuid => write!(f, "heartbeat uuid must not be empty"),
            Self::NegativeMinutesBetween { index, value } => {
                write!(f, "notify[{index}] has negative minutesBetween {value}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Resolve the config path: `CONFIG_PATH` env var, else `/config/config.json`.
pub fn config_path() -> PathBuf {
    std::env::var_os("CONFIG_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH))
}

pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_owned(),
        source,
    })?;
    let mut de = serde_json::Deserializer::from_str(&text);
    let config: Config = serde_path_to_error::deserialize(&mut de).map_err(|e| {
        let location = e.path().to_string();
        let source = e.into_inner();
        match source.classify() {
            serde_json::error::Category::Syntax | serde_json::error::Category::Eof => {
                ConfigError::Json { source }
            }
            serde_json::error::Category::Data | serde_json::error::Category::Io => {
                ConfigError::Shape {
                    location,
                    message: source.to_string(),
                }
            }
        }
    })?;
    validate(&config)?;
    Ok(config)
}

fn validate(config: &Config) -> Result<(), ConfigError> {
    let mut names = HashSet::new();
    for (index, service) in config.services.iter().enumerate() {
        if service.service.is_empty() {
            return Err(ConfigError::EmptyServiceName { index });
        }
        if !names.insert(service.service.as_str()) {
            return Err(ConfigError::DuplicateService {
                name: service.service.clone(),
            });
        }
        if !(1..=599).contains(&service.ok_status_code) {
            return Err(ConfigError::StatusCodeRange {
                service: service.service.clone(),
                code: service.ok_status_code,
            });
        }
    }

    if let Some(HeartbeatConfig::HealthchecksIo { uuid, .. }) = &config.heartbeat
        && uuid.is_empty()
    {
        return Err(ConfigError::EmptyUuid);
    }

    for (index, notify) in config.notify.iter().enumerate() {
        if notify.minutes_between() < 0.0 {
            return Err(ConfigError::NegativeMinutesBetween {
                index,
                value: notify.minutes_between(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn load_str(json: &str) -> Result<Config, ConfigError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, json).unwrap();
        load(&path)
    }

    /// Valid single-service config with one field swapped in, to isolate rejections.
    fn one_service(field: &str, value: &str) -> String {
        let mut entry = serde_json::json!({
            "service": "a",
            "schedule": "every 1 minute",
            "url": "http://localhost:1234",
            "okStatusCode": 200
        });
        entry[field] = serde_json::from_str(value).unwrap();
        serde_json::json!({ "services": [entry] }).to_string()
    }

    #[test]
    fn full_example_parses_with_every_field() {
        let config = load(&fixture("full.json")).unwrap();
        assert_eq!(config.services.len(), 2);
        assert_eq!(config.services[0].service, "Home Assistant");
        assert_eq!(config.services[0].schedule.to_string(), "Every 10 minutes");
        assert_eq!(config.services[0].schedule.interval_seconds(), 600);
        assert_eq!(config.services[0].url, "http://192.168.1.89:4357");
        assert_eq!(config.services[0].ok_status_code, 200);
        assert_eq!(config.services[1].service, "Nginx redirect");
        assert_eq!(config.services[1].schedule.interval_seconds(), 60);
        assert_eq!(config.services[1].ok_status_code, 301);
        assert_eq!(
            config.heartbeat,
            Some(HeartbeatConfig::HealthchecksIo {
                uuid: "12345678-1234-1234-1234-123456789012".into(),
                schedule: crate::schedule::parse("Every 10 minutes").unwrap()
            })
        );
        assert_eq!(config.notify.len(), 3);
        assert_eq!(
            config.notify[0],
            NotifyConfig::Ntfy(NtfyEntry {
                topic: "my-ntfy-topic".into(),
                schedule: crate::schedule::parse("Every 10 minutes").unwrap(),
                minutes_between: 120.0,
            })
        );
        assert_eq!(config.notify[0].schedule().to_string(), "Every 10 minutes");
        assert_eq!(config.notify[0].minutes_between(), 120.0);
        assert_eq!(
            config.notify[1],
            NotifyConfig::Telegram(TelegramEntry {
                bot_token: "123456:abc-def".into(),
                chat_id: "987654".into(),
                schedule: crate::schedule::parse("Every 10 minutes").unwrap(),
                minutes_between: 60.0,
            })
        );
        assert_eq!(config.notify[1].schedule().to_string(), "Every 10 minutes");
        assert_eq!(
            config.notify[2],
            NotifyConfig::Pushover(PushoverEntry {
                api_token: "my-pushover-token".into(),
                user_key: "my-pushover-user".into(),
                priority: None,
                schedule: crate::schedule::parse("Every 10 minutes").unwrap(),
                minutes_between: 30.0,
            })
        );
        assert_eq!(config.notify[2].schedule().to_string(), "Every 10 minutes");
        assert_eq!(config.notify[2].minutes_between(), 30.0);
    }

    #[test]
    fn minimal_config_parses_with_defaults() {
        let config = load(&fixture("minimal.json")).unwrap();
        assert_eq!(config.services, vec![]);
        assert_eq!(config.heartbeat, None);
        assert_eq!(config.notify, vec![]);
    }

    #[test]
    fn heartbeat_accepts_loose_uuid_vector() {
        let json = r#"{"services": [], "heartbeat": {"type": "healthchecks.io",
            "uuid": "12345678-1234-1234-1234-123456789012", "schedule": "every 1 minute"}}"#;
        let config = load_str(json).unwrap();
        assert!(matches!(
            config.heartbeat,
            Some(HeartbeatConfig::HealthchecksIo { .. })
        ));
    }

    #[test]
    fn heartbeat_accepts_httpbin_without_uuid() {
        let json =
            r#"{"services": [], "heartbeat": {"type": "httpbin", "schedule": "every 1 minute"}}"#;
        let config = load_str(json).unwrap();
        assert_eq!(
            config.heartbeat,
            Some(HeartbeatConfig::Httpbin {
                schedule: crate::schedule::parse("every 1 minute").unwrap()
            })
        );
    }

    #[test]
    fn httpbin_with_stray_uuid_rejected() {
        // `uuid` isn't a field of the httpbin variant — deny_unknown_fields rejects it
        // instead of silently ignoring it.
        let json = r#"{"services": [], "heartbeat": {"type": "httpbin",
            "uuid": "12345678-1234-1234-1234-123456789012", "schedule": "every 1 minute"}}"#;
        let err = load_str(json).unwrap_err();
        assert!(err.to_string().contains("uuid"), "got: {err}");
    }

    #[test]
    fn heartbeat_missing_uuid_rejected() {
        let json = r#"{"services": [], "heartbeat": {"type": "healthchecks.io",
            "schedule": "every 1 minute"}}"#;
        let err = load_str(json).unwrap_err();
        assert!(err.to_string().contains("uuid"), "got: {err}");
    }

    #[test]
    fn heartbeat_uuid_shape_not_validated() {
        // The uuid is an opaque healthchecks.io identifier, just interpolated into a
        // ping URL — only presence is checked, not shape.
        for accepted in ["not-a-valid-uuid", "12345678123412341234123456789012"] {
            let json = format!(
                r#"{{"services": [], "heartbeat": {{"type": "healthchecks.io",
                    "uuid": "{accepted}", "schedule": "every 1 minute"}}}}"#
            );
            let config = load_str(&json).unwrap();
            assert!(matches!(
                config.heartbeat,
                Some(HeartbeatConfig::HealthchecksIo { .. })
            ));
        }
    }

    #[test]
    fn heartbeat_empty_uuid_rejected() {
        let json = r#"{"services": [], "heartbeat": {"type": "healthchecks.io",
            "uuid": "", "schedule": "every 1 minute"}}"#;
        let err = load_str(json).unwrap_err();
        assert_eq!(err.to_string(), "heartbeat uuid must not be empty");
    }

    #[test]
    fn heartbeat_unknown_type_rejected() {
        let json = r#"{"services": [], "heartbeat": {"type": "invalid",
            "schedule": "every 1 minute"}}"#;
        let err = load_str(json).unwrap_err();
        assert!(err.to_string().contains("invalid"), "got: {err}");
    }

    #[test]
    fn missing_file_rejected_naming_path() {
        let err = load(Path::new("/nonexistent/nowhere.json")).unwrap_err();
        assert!(matches!(err, ConfigError::Read { .. }));
        assert!(err.to_string().contains("/nonexistent/nowhere.json"));
    }

    #[test]
    fn invalid_json_rejected() {
        let err = load_str("{ not json").unwrap_err();
        assert!(matches!(err, ConfigError::Json { .. }));
        assert!(err.to_string().contains("not valid JSON"));
    }

    #[test]
    fn missing_services_rejected() {
        let err = load_str(r#"{"notify": []}"#).unwrap_err();
        assert!(err.to_string().contains("services"), "got: {err}");
    }

    #[test]
    fn empty_service_name_rejected() {
        let err = load_str(&one_service("service", r#""""#)).unwrap_err();
        assert_eq!(err.to_string(), "services[0].service must not be empty");
    }

    #[test]
    fn url_shape_not_validated() {
        // The check loop's own HTTP client rejects a bad url at request time (surfaced
        // as a normal "down" result) — config load no longer re-validates it.
        for accepted in ["example.com/foo", "not a url", "ftp://server/file"] {
            let config = load_str(&one_service("url", &format!("{accepted:?}"))).unwrap();
            assert_eq!(config.services[0].url, accepted);
        }
    }

    #[test]
    fn status_code_bounds_rejected() {
        for bad in ["0", "600"] {
            let err = load_str(&one_service("okStatusCode", bad)).unwrap_err();
            assert!(
                err.to_string().contains("okStatusCode"),
                "got: {err} for {bad}"
            );
            assert!(err.to_string().contains(bad));
        }
    }

    #[test]
    fn fractional_status_code_rejected() {
        let err = load_str(&one_service("okStatusCode", "200.5")).unwrap_err();
        // Caught by typed deserialization; the path must name the field.
        assert!(err.to_string().contains("okStatusCode"), "got: {err}");
    }

    #[test]
    fn empty_schedule_rejected() {
        let err = load_str(&one_service("schedule", r#""""#)).unwrap_err();
        assert!(err.to_string().contains("schedule"), "got: {err}");
    }

    #[test]
    fn bad_schedule_rejected_naming_expression_and_field() {
        // Config integration for §6 Phase 2: the startup error names the expression.
        let err = load_str(&one_service("schedule", r#""banana""#)).unwrap_err();
        assert!(err.to_string().contains("\"banana\""), "got: {err}");
        assert!(
            err.to_string().contains("services[0].schedule"),
            "got: {err}"
        );

        let json = r#"{"services": [], "heartbeat": {"type": "httpbin",
            "schedule": "every 0 minutes"}}"#;
        let err = load_str(json).unwrap_err();
        assert!(err.to_string().contains("every 0 minutes"), "got: {err}");
        assert!(err.to_string().contains("heartbeat"), "got: {err}");

        let json = r#"{"services": [], "notify": [
            {"type": "ntfy", "topic": "t", "schedule": "every -5 minutes", "minutesBetween": 1}
        ]}"#;
        let err = load_str(json).unwrap_err();
        assert!(err.to_string().contains("every -5 minutes"), "got: {err}");
    }

    #[test]
    fn duplicate_service_names_rejected() {
        let json = r#"{"services": [
            {"service": "a", "schedule": "every 1 minute", "url": "http://x", "okStatusCode": 200},
            {"service": "a", "schedule": "every 1 minute", "url": "http://y", "okStatusCode": 200}
        ]}"#;
        let err = load_str(json).unwrap_err();
        assert_eq!(err.to_string(), "duplicate service name \"a\"");
    }

    #[test]
    fn duplicate_and_empty_notify_topics_accepted() {
        // Unlike service names, notify topics aren't shared map keys — each entry runs
        // its own independent rate-limit state, so dup/empty topics just mean
        // redundant ntfy posts, not corrupted state. No longer rejected.
        let json = r#"{"services": [], "notify": [
            {"type": "ntfy", "topic": "t", "schedule": "every 1 minute", "minutesBetween": 1},
            {"type": "ntfy", "topic": "t", "schedule": "every 1 minute", "minutesBetween": 2},
            {"type": "ntfy", "topic": "", "schedule": "every 1 minute", "minutesBetween": 1}
        ]}"#;
        let config = load_str(json).unwrap();
        assert_eq!(config.notify.len(), 3);
    }

    #[test]
    fn telegram_notify_variant_parses() {
        let json = r#"{"services": [], "notify": [
            {"type": "telegram", "botToken": "123:abc", "chatId": "42",
             "schedule": "every 1 minute", "minutesBetween": 5}
        ]}"#;
        let config = load_str(json).unwrap();
        assert_eq!(
            config.notify[0],
            NotifyConfig::Telegram(TelegramEntry {
                bot_token: "123:abc".into(),
                chat_id: "42".into(),
                schedule: crate::schedule::parse("every 1 minute").unwrap(),
                minutes_between: 5.0,
            })
        );
    }

    #[test]
    fn pushover_notify_variant_parses() {
        let json = r#"{"services": [], "notify": [
            {"type": "pushover", "apiToken": "app-tok", "userKey": "user-key",
             "priority": 1, "schedule": "every 1 minute", "minutesBetween": 5}
        ]}"#;
        let config = load_str(json).unwrap();
        assert_eq!(
            config.notify[0],
            NotifyConfig::Pushover(PushoverEntry {
                api_token: "app-tok".into(),
                user_key: "user-key".into(),
                priority: Some(1),
                schedule: crate::schedule::parse("every 1 minute").unwrap(),
                minutes_between: 5.0,
            })
        );
    }

    #[test]
    fn pushover_notify_variant_parses_without_priority() {
        // Priority omitted entirely — must default to None, not error.
        let json = r#"{"services": [], "notify": [
            {"type": "pushover", "apiToken": "app-tok", "userKey": "user-key",
             "schedule": "every 1 minute", "minutesBetween": 5}
        ]}"#;
        let config = load_str(json).unwrap();
        assert_eq!(
            config.notify[0],
            NotifyConfig::Pushover(PushoverEntry {
                api_token: "app-tok".into(),
                user_key: "user-key".into(),
                priority: None,
                schedule: crate::schedule::parse("every 1 minute").unwrap(),
                minutes_between: 5.0,
            })
        );
    }

    #[test]
    fn notify_unknown_type_rejected() {
        let json = r#"{"services": [], "notify": [
            {"type": "sms", "schedule": "every 1 minute", "minutesBetween": 1}
        ]}"#;
        let err = load_str(json).unwrap_err();
        assert!(err.to_string().contains("sms"), "got: {err}");
    }

    #[test]
    fn telegram_with_stray_topic_rejected() {
        // `topic` isn't a field of the telegram variant — deny_unknown_fields rejects
        // it instead of silently ignoring it.
        let json = r#"{"services": [], "notify": [
            {"type": "telegram", "botToken": "123:abc", "chatId": "42", "topic": "t",
             "schedule": "every 1 minute", "minutesBetween": 1}
        ]}"#;
        let err = load_str(json).unwrap_err();
        assert!(err.to_string().contains("topic"), "got: {err}");
    }

    #[test]
    fn pushover_with_stray_topic_rejected() {
        // `topic` isn't a field of the pushover variant — deny_unknown_fields rejects
        // it instead of silently ignoring it.
        let json = r#"{"services": [], "notify": [
            {"type": "pushover", "apiToken": "app-tok", "userKey": "user-key", "topic": "t",
             "schedule": "every 1 minute", "minutesBetween": 1}
        ]}"#;
        let err = load_str(json).unwrap_err();
        assert!(err.to_string().contains("topic"), "got: {err}");
    }

    #[test]
    fn negative_minutes_between_rejected() {
        let json = r#"{"services": [], "notify": [
            {"type": "ntfy", "topic": "t", "schedule": "every 1 minute", "minutesBetween": -1}
        ]}"#;
        let err = load_str(json).unwrap_err();
        assert!(err.to_string().contains("minutesBetween"), "got: {err}");
        assert!(err.to_string().contains("-1"));
    }

    #[test]
    fn fractional_minutes_between_accepted() {
        let json = r#"{"services": [], "notify": [
            {"type": "ntfy", "topic": "t", "schedule": "every 1 minute", "minutesBetween": 1.5}
        ]}"#;
        let config = load_str(json).unwrap();
        assert_eq!(config.notify[0].minutes_between(), 1.5);
    }

    #[test]
    fn missing_minutes_between_rejected() {
        // No default: the legacy zod schema had none either.
        let json = r#"{"services": [], "notify": [
            {"type": "ntfy", "topic": "t", "schedule": "every 1 minute"}
        ]}"#;
        let err = load_str(json).unwrap_err();
        assert!(err.to_string().contains("minutesBetween"), "got: {err}");
    }

    #[test]
    fn nodes_key_rejected() {
        // Legacy configs from the old system carried a top-level "nodes" section;
        // it's no longer tolerated — any unrecognized key is a hard failure.
        let json = r#"{"services": [], "nodes": [{"anything": true}]}"#;
        let err = load_str(json).unwrap_err();
        assert!(err.to_string().contains("nodes"), "got: {err}");
    }

    #[test]
    fn unknown_keys_rejected_everywhere() {
        // deny_unknown_fields on every struct/variant: a typo anywhere fails the load
        // and names the offending field, instead of silently being ignored.
        for (bad, expect) in [
            (r#"{"services": [], "someTypo": true}"#, "someTypo"),
            (
                r#"{"services": [{"service": "a", "schedule": "every 1 minute",
                    "url": "http://x", "okStatusCode": 200, "okStatuscode": 200}]}"#,
                "okStatuscode",
            ),
            (
                r#"{"services": [], "heartbeat": {"type": "healthchecks.io",
                    "uuid": "12345678-1234-1234-1234-123456789012",
                    "schedule": "every 1 minute", "extra": 1}}"#,
                "extra",
            ),
            (
                r#"{"services": [], "notify": [{"type": "ntfy", "topic": "t",
                    "schedule": "every 1 minute",
                    "minutesBetween": 1, "minutesbetween": 2}]}"#,
                "minutesbetween",
            ),
        ] {
            let err = load_str(bad).unwrap_err();
            assert!(
                err.to_string().contains(expect),
                "expected {expect} in error, got: {err}"
            );
        }
    }

    #[test]
    fn non_object_root_rejected() {
        let err = load_str("[1, 2]").unwrap_err();
        assert!(matches!(err, ConfigError::Shape { .. }));
    }

    #[test]
    fn wrong_typed_sections_rejected() {
        for bad in [
            r#"{"services": 42}"#,
            r#"{"services": [42]}"#,
            r#"{"heartbeat": 42, "services": []}"#,
            r#"{"notify": 42, "services": []}"#,
            r#"{"notify": [42], "services": []}"#,
        ] {
            let err = load_str(bad).unwrap_err();
            assert!(matches!(err, ConfigError::Shape { .. }), "for {bad}");
        }
    }

    #[test]
    fn heartbeat_schedule_accessor_covers_both_variants() {
        let hc = HeartbeatConfig::HealthchecksIo {
            uuid: "12345678-1234-1234-1234-123456789012".into(),
            schedule: crate::schedule::parse("every 1 minute").unwrap(),
        };
        let httpbin = HeartbeatConfig::Httpbin {
            schedule: crate::schedule::parse("every 2 hours").unwrap(),
        };
        assert_eq!(hc.schedule().interval_seconds(), 60);
        assert_eq!(httpbin.schedule().interval_seconds(), 7200);
    }

    #[test]
    fn config_path_env_override_and_default() {
        // Single test for both branches: env mutation must not race other tests.
        unsafe { std::env::set_var("CONFIG_PATH", "/tmp/other.json") };
        assert_eq!(config_path(), PathBuf::from("/tmp/other.json"));
        unsafe { std::env::remove_var("CONFIG_PATH") };
        assert_eq!(config_path(), PathBuf::from(DEFAULT_CONFIG_PATH));
    }
}
