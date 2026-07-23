//! Configuration loading and validation (spec §2.1).
//!
//! Two-pass parse: the file is first read into a generic JSON value that is walked for
//! unknown keys (recursively — typos like `okStatuscode` must warn, not vanish), then
//! deserialized into typed structs and semantically validated. The loader returns the
//! warnings instead of logging them so tests can assert on them.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::schedule::Schedule;

pub const DEFAULT_CONFIG_PATH: &str = "/config/config.json";

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Config {
    pub services: Vec<ServiceConfig>,
    #[serde(default)]
    pub heartbeat: Option<HeartbeatConfig>,
    #[serde(default)]
    pub notify: Vec<NotifyConfig>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ServiceConfig {
    pub service: String,
    // `try_from = "String"` on Schedule parses each expression exactly once, at startup
    // (spec §2.2.3); a bad expression fails deserialization naming the field and the
    // original expression.
    pub schedule: Schedule,
    pub url: String,
    #[serde(rename = "okStatusCode")]
    pub ok_status_code: u16,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type")]
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
pub struct NotifyConfig {
    pub topic: String,
    pub schedule: Schedule,
    #[serde(rename = "minutesBetween")]
    pub minutes_between: f64,
}

/// Non-fatal findings from the unknown-key walk. `IgnoredNodes` logs at info level,
/// `UnknownKey` at warn level (spec §2.1).
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigWarning {
    IgnoredNodes,
    UnknownKey { path: String },
}

impl ConfigWarning {
    pub fn is_info(&self) -> bool {
        matches!(self, Self::IgnoredNodes)
    }
}

impl fmt::Display for ConfigWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IgnoredNodes => write!(f, "config key \"nodes\" is ignored"),
            Self::UnknownKey { path } => write!(f, "unknown config key \"{path}\" is ignored"),
        }
    }
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
    InvalidUrl {
        service: String,
        url: String,
        reason: String,
    },
    StatusCodeRange {
        service: String,
        code: u16,
    },
    InvalidUuid {
        uuid: String,
    },
    EmptyTopic {
        index: usize,
    },
    DuplicateTopic {
        topic: String,
    },
    NegativeMinutesBetween {
        topic: String,
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
            Self::InvalidUrl {
                service,
                url,
                reason,
            } => write!(
                f,
                "service \"{service}\" has invalid url \"{url}\": {reason}"
            ),
            Self::StatusCodeRange { service, code } => write!(
                f,
                "service \"{service}\" has okStatusCode {code} outside 1..=599"
            ),
            Self::InvalidUuid { uuid } => write!(
                f,
                "heartbeat uuid \"{uuid}\" is not a hyphenated 8-4-4-4-12 hex UUID"
            ),
            Self::EmptyTopic { index } => write!(f, "notify[{index}].topic must not be empty"),
            Self::DuplicateTopic { topic } => write!(f, "duplicate notify topic \"{topic}\""),
            Self::NegativeMinutesBetween { topic, value } => write!(
                f,
                "notify topic \"{topic}\" has negative minutesBetween {value}"
            ),
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

pub fn load(path: &Path) -> Result<(Config, Vec<ConfigWarning>), ConfigError> {
    let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_owned(),
        source,
    })?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|source| ConfigError::Json { source })?;
    let warnings = collect_warnings(&value);
    let config: Config =
        serde_path_to_error::deserialize(&value).map_err(|e| ConfigError::Shape {
            location: e.path().to_string(),
            message: e.inner().to_string(),
        })?;
    validate(&config)?;
    Ok((config, warnings))
}

const SERVICE_KEYS: &[&str] = &["service", "schedule", "url", "okStatusCode"];
const NOTIFY_KEYS: &[&str] = &["topic", "schedule", "minutesBetween"];

fn collect_warnings(root: &serde_json::Value) -> Vec<ConfigWarning> {
    let mut warnings = Vec::new();
    let Some(obj) = root.as_object() else {
        return warnings; // non-object root fails typed deserialization with its own error
    };
    for (key, value) in obj {
        match key.as_str() {
            "services" => walk_entries(value, "services", SERVICE_KEYS, &mut warnings),
            "notify" => walk_entries(value, "notify", NOTIFY_KEYS, &mut warnings),
            "heartbeat" => {
                // `uuid` is a known key only for healthchecks.io; a stray uuid on an
                // httpbin heartbeat warns (spec §2.1). Unknown/missing type gets the
                // permissive set — typed deserialization reports the real problem.
                let known: &[&str] = match value.get("type").and_then(|t| t.as_str()) {
                    Some("httpbin") => &["type", "schedule"],
                    _ => &["type", "uuid", "schedule"],
                };
                warn_unknown_keys(value, "heartbeat", known, &mut warnings);
            }
            "nodes" => warnings.push(ConfigWarning::IgnoredNodes),
            other => warnings.push(ConfigWarning::UnknownKey {
                path: other.to_string(),
            }),
        }
    }
    warnings
}

fn walk_entries(
    value: &serde_json::Value,
    name: &str,
    known: &[&str],
    warnings: &mut Vec<ConfigWarning>,
) {
    if let Some(entries) = value.as_array() {
        for (i, entry) in entries.iter().enumerate() {
            warn_unknown_keys(entry, &format!("{name}[{i}]"), known, warnings);
        }
    }
}

fn warn_unknown_keys(
    value: &serde_json::Value,
    prefix: &str,
    known: &[&str],
    warnings: &mut Vec<ConfigWarning>,
) {
    if let Some(obj) = value.as_object() {
        for key in obj.keys() {
            if !known.contains(&key.as_str()) {
                warnings.push(ConfigWarning::UnknownKey {
                    path: format!("{prefix}.{key}"),
                });
            }
        }
    }
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
        validate_url(service)?;
        if !(1..=599).contains(&service.ok_status_code) {
            return Err(ConfigError::StatusCodeRange {
                service: service.service.clone(),
                code: service.ok_status_code,
            });
        }
    }

    if let Some(heartbeat) = &config.heartbeat
        && let HeartbeatConfig::HealthchecksIo { uuid, .. } = heartbeat
        && !is_loose_uuid(uuid)
    {
        return Err(ConfigError::InvalidUuid { uuid: uuid.clone() });
    }

    let mut topics = HashSet::new();
    for (index, notify) in config.notify.iter().enumerate() {
        if notify.topic.is_empty() {
            return Err(ConfigError::EmptyTopic { index });
        }
        if !topics.insert(notify.topic.as_str()) {
            return Err(ConfigError::DuplicateTopic {
                topic: notify.topic.clone(),
            });
        }
        if notify.minutes_between < 0.0 {
            return Err(ConfigError::NegativeMinutesBetween {
                topic: notify.topic.clone(),
                value: notify.minutes_between,
            });
        }
    }
    Ok(())
}

fn validate_url(service: &ServiceConfig) -> Result<(), ConfigError> {
    let parsed = url::Url::parse(&service.url).map_err(|e| ConfigError::InvalidUrl {
        service: service.service.clone(),
        url: service.url.clone(),
        reason: e.to_string(),
    })?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(ConfigError::InvalidUrl {
            service: service.service.clone(),
            url: service.url.clone(),
            reason: format!("scheme must be http or https, got \"{}\"", parsed.scheme()),
        });
    }
    Ok(())
}

/// Loose UUID shape check (spec §2.1): hyphenated 8-4-4-4-12 hex, case-insensitive.
/// Deliberately does NOT check RFC 4122 version/variant bits, and deliberately rejects
/// unhyphenated/braced/URN forms the `uuid` crate would accept.
fn is_loose_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    let lens = [8, 4, 4, 4, 12];
    parts.len() == lens.len()
        && parts
            .iter()
            .zip(lens)
            .all(|(p, len)| p.len() == len && p.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn load_str(json: &str) -> Result<(Config, Vec<ConfigWarning>), ConfigError> {
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
        let (config, warnings) = load(&fixture("full.json")).unwrap();
        assert_eq!(warnings, vec![]);
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
        assert_eq!(config.notify.len(), 1);
        assert_eq!(config.notify[0].topic, "my-ntfy-topic");
        assert_eq!(config.notify[0].schedule.to_string(), "Every 10 minutes");
        assert_eq!(config.notify[0].minutes_between, 120.0);
    }

    #[test]
    fn minimal_config_parses_with_defaults() {
        let (config, warnings) = load(&fixture("minimal.json")).unwrap();
        assert_eq!(warnings, vec![]);
        assert_eq!(config.services, vec![]);
        assert_eq!(config.heartbeat, None);
        assert_eq!(config.notify, vec![]);
    }

    #[test]
    fn heartbeat_accepts_loose_uuid_vector() {
        // 12345678-1234-1234-1234-123456789012 has invalid RFC 4122 variant bits but the
        // legacy suite accepts it; the loose validator must keep doing so.
        let json = r#"{"services": [], "heartbeat": {"type": "healthchecks.io",
            "uuid": "12345678-1234-1234-1234-123456789012", "schedule": "every 1 minute"}}"#;
        let (config, _) = load_str(json).unwrap();
        assert!(matches!(
            config.heartbeat,
            Some(HeartbeatConfig::HealthchecksIo { .. })
        ));
    }

    #[test]
    fn heartbeat_accepts_httpbin_without_uuid() {
        let json =
            r#"{"services": [], "heartbeat": {"type": "httpbin", "schedule": "every 1 minute"}}"#;
        let (config, warnings) = load_str(json).unwrap();
        assert_eq!(
            config.heartbeat,
            Some(HeartbeatConfig::Httpbin {
                schedule: crate::schedule::parse("every 1 minute").unwrap()
            })
        );
        assert_eq!(warnings, vec![]);
    }

    #[test]
    fn httpbin_with_stray_uuid_parses_and_warns() {
        let json = r#"{"services": [], "heartbeat": {"type": "httpbin",
            "uuid": "12345678-1234-1234-1234-123456789012", "schedule": "every 1 minute"}}"#;
        let (config, warnings) = load_str(json).unwrap();
        assert!(matches!(
            config.heartbeat,
            Some(HeartbeatConfig::Httpbin { .. })
        ));
        assert_eq!(
            warnings,
            vec![ConfigWarning::UnknownKey {
                path: "heartbeat.uuid".into()
            }]
        );
        assert!(!warnings[0].is_info());
        assert_eq!(
            warnings[0].to_string(),
            "unknown config key \"heartbeat.uuid\" is ignored"
        );
    }

    #[test]
    fn heartbeat_missing_uuid_rejected() {
        let json = r#"{"services": [], "heartbeat": {"type": "healthchecks.io",
            "schedule": "every 1 minute"}}"#;
        let err = load_str(json).unwrap_err();
        assert!(err.to_string().contains("uuid"), "got: {err}");
    }

    #[test]
    fn heartbeat_malformed_uuid_rejected() {
        for bad in ["not-a-valid-uuid", "12345678123412341234123456789012"] {
            let json = format!(
                r#"{{"services": [], "heartbeat": {{"type": "healthchecks.io",
                    "uuid": "{bad}", "schedule": "every 1 minute"}}}}"#
            );
            let err = load_str(&json).unwrap_err();
            assert!(
                err.to_string().contains(bad),
                "error should name the uuid, got: {err}"
            );
        }
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
    fn relative_url_rejected() {
        let err = load_str(&one_service("url", r#""example.com/foo""#)).unwrap_err();
        assert!(err.to_string().contains("example.com/foo"), "got: {err}");
    }

    #[test]
    fn garbage_url_rejected() {
        let err = load_str(&one_service("url", r#""not a url""#)).unwrap_err();
        assert!(err.to_string().contains("not a url"), "got: {err}");
    }

    #[test]
    fn non_http_scheme_rejected() {
        let err = load_str(&one_service("url", r#""ftp://server/file""#)).unwrap_err();
        assert!(err.to_string().contains("scheme"), "got: {err}");
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
            {"topic": "t", "schedule": "every -5 minutes", "minutesBetween": 1}
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
    fn duplicate_notify_topics_rejected() {
        let json = r#"{"services": [], "notify": [
            {"topic": "t", "schedule": "every 1 minute", "minutesBetween": 1},
            {"topic": "t", "schedule": "every 1 minute", "minutesBetween": 2}
        ]}"#;
        let err = load_str(json).unwrap_err();
        assert_eq!(err.to_string(), "duplicate notify topic \"t\"");
    }

    #[test]
    fn empty_notify_topic_rejected() {
        let json = r#"{"services": [], "notify": [
            {"topic": "", "schedule": "every 1 minute", "minutesBetween": 1}
        ]}"#;
        let err = load_str(json).unwrap_err();
        assert_eq!(err.to_string(), "notify[0].topic must not be empty");
    }

    #[test]
    fn negative_minutes_between_rejected() {
        let json = r#"{"services": [], "notify": [
            {"topic": "t", "schedule": "every 1 minute", "minutesBetween": -1}
        ]}"#;
        let err = load_str(json).unwrap_err();
        assert!(err.to_string().contains("minutesBetween"), "got: {err}");
        assert!(err.to_string().contains("-1"));
    }

    #[test]
    fn fractional_minutes_between_accepted() {
        let json = r#"{"services": [], "notify": [
            {"topic": "t", "schedule": "every 1 minute", "minutesBetween": 1.5}
        ]}"#;
        let (config, _) = load_str(json).unwrap();
        assert_eq!(config.notify[0].minutes_between, 1.5);
    }

    #[test]
    fn missing_minutes_between_rejected() {
        // No default (spec §2.1): the legacy zod schema had none either.
        let json = r#"{"services": [], "notify": [
            {"topic": "t", "schedule": "every 1 minute"}
        ]}"#;
        let err = load_str(json).unwrap_err();
        assert!(err.to_string().contains("minutesBetween"), "got: {err}");
    }

    #[test]
    fn nodes_key_ignored_with_info_warning() {
        let json = r#"{"services": [], "nodes": [{"anything": true}]}"#;
        let (_, warnings) = load_str(json).unwrap();
        assert_eq!(warnings, vec![ConfigWarning::IgnoredNodes]);
        assert!(warnings[0].is_info());
        assert_eq!(warnings[0].to_string(), "config key \"nodes\" is ignored");
    }

    #[test]
    fn unknown_keys_warn_recursively() {
        let json = r#"{
            "services": [{"service": "a", "schedule": "every 1 minute",
                          "url": "http://x", "okStatusCode": 200, "okStatuscode": 200}],
            "heartbeat": {"type": "healthchecks.io",
                          "uuid": "12345678-1234-1234-1234-123456789012",
                          "schedule": "every 1 minute", "extra": 1},
            "notify": [{"topic": "t", "schedule": "every 1 minute",
                        "minutesBetween": 1, "minutesbetween": 2}],
            "nodes": [],
            "someTypo": true
        }"#;
        let (_, warnings) = load_str(json).unwrap();
        let paths: Vec<String> = warnings.iter().map(ToString::to_string).collect();
        assert_eq!(warnings.len(), 5, "got: {paths:?}");
        assert!(warnings.contains(&ConfigWarning::UnknownKey {
            path: "services[0].okStatuscode".into()
        }));
        assert!(warnings.contains(&ConfigWarning::UnknownKey {
            path: "heartbeat.extra".into()
        }));
        assert!(warnings.contains(&ConfigWarning::UnknownKey {
            path: "notify[0].minutesbetween".into()
        }));
        assert!(warnings.contains(&ConfigWarning::IgnoredNodes));
        assert!(warnings.contains(&ConfigWarning::UnknownKey {
            path: "someTypo".into()
        }));
    }

    #[test]
    fn non_object_root_rejected() {
        let err = load_str("[1, 2]").unwrap_err();
        assert!(matches!(err, ConfigError::Shape { .. }));
    }

    #[test]
    fn wrong_typed_sections_rejected_without_walk_panics() {
        // The unknown-key walk runs before typed deserialization and must shrug at
        // sections of the wrong JSON type; the typed pass reports the real error.
        for bad in [
            r#"{"services": 42}"#,
            r#"{"services": [42]}"#,
            r#"{"heartbeat": 42, "services": []}"#,
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
