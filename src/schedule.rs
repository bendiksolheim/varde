//! Schedule expressions (spec §2.2).
//!
//! Hand-rolled interval grammar: `every N seconds|minutes|hours|days`, N ≥ 1 — the Phase 0
//! spike rejected `hron` (no seconds support, no bare interval form; see rust.md). A
//! normalization step in front of the parser makes parsing case-insensitive and tolerant
//! of singular/plural mismatches and stray whitespace.
//!
//! Occurrences are epoch-anchored: `next_after(t)` returns the smallest multiple of the
//! interval strictly after `t`, counted from the Unix epoch. For every real-world interval
//! this equals wall-clock alignment (`every 10 minutes` fires at :00/:10/:20/…), matching
//! the legacy later.js behavior. The choice is documented in the README and must stay
//! stable.
//!
//! This module is the single seam for schedule parsing (spec §2.2.5): swapping the
//! grammar/parser must not touch any other module.

use std::fmt;

use jiff::Timestamp;

/// Ten years. Caps the interval so epoch-anchored arithmetic can never leave jiff's
/// Timestamp range (~year 9999) for any realistic `t`, keeping `next_after` infallible.
const MAX_INTERVAL_SECONDS: u64 = 10 * 365 * 24 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct Schedule {
    expression: String,
    interval_seconds: i64,
}

impl Schedule {
    /// The smallest epoch-anchored multiple of the interval strictly after `t`.
    /// Occurrences skipped while a tick ran are never replayed — callers always ask
    /// "what is next from *now*" (spec §2.3).
    pub fn next_after(&self, t: Timestamp) -> Timestamp {
        let n = self.interval_seconds;
        let next = (t.as_second().div_euclid(n) + 1) * n;
        Timestamp::from_second(next)
            .expect("interval is capped at 10 years, keeping next_after in Timestamp range")
    }

    pub fn interval_seconds(&self) -> i64 {
        self.interval_seconds
    }
}

/// Displays the original (pre-normalization) expression, for logs and error context.
impl fmt::Display for Schedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.expression)
    }
}

/// Lets config fields deserialize straight into `Schedule`, so every expression is
/// parsed exactly once, at startup (spec §2.2.3).
impl TryFrom<String> for Schedule {
    type Error = ScheduleError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        parse(&value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleError {
    expression: String,
    kind: ErrorKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ErrorKind {
    Empty,
    Grammar,
    InvalidNumber { token: String },
    ZeroInterval,
    UnknownUnit { unit: String },
    TooLarge,
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Always name the original expression (spec §2.2.3).
        write!(f, "cannot parse schedule \"{}\": ", self.expression)?;
        match &self.kind {
            ErrorKind::Empty => write!(f, "expression is empty"),
            ErrorKind::Grammar => {
                write!(f, "expected \"every N seconds|minutes|hours|days\"")
            }
            ErrorKind::InvalidNumber { token } => {
                write!(f, "\"{token}\" is not a positive integer")
            }
            ErrorKind::ZeroInterval => write!(f, "interval must be at least 1"),
            ErrorKind::UnknownUnit { unit } => write!(
                f,
                "unknown unit \"{unit}\", expected seconds|minutes|hours|days"
            ),
            ErrorKind::TooLarge => write!(f, "interval exceeds the 10-year maximum"),
        }
    }
}

impl std::error::Error for ScheduleError {}

/// Trim, lowercase, collapse whitespace, and canonicalize units to their plural form.
/// Purely lexical and idempotent; garbage passes through for the parser to reject.
pub fn normalize(expr: &str) -> String {
    expr.split_whitespace()
        .map(|token| {
            let token = token.to_ascii_lowercase();
            match token.as_str() {
                "second" => "seconds".to_string(),
                "minute" => "minutes".to_string(),
                "hour" => "hours".to_string(),
                "day" => "days".to_string(),
                _ => token,
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn parse(expr: &str) -> Result<Schedule, ScheduleError> {
    let error = |kind| ScheduleError {
        expression: expr.to_string(),
        kind,
    };
    let normalized = normalize(expr);
    if normalized.is_empty() {
        return Err(error(ErrorKind::Empty));
    }
    let tokens: Vec<&str> = normalized.split(' ').collect();
    let ["every", count, unit] = tokens.as_slice() else {
        return Err(error(ErrorKind::Grammar));
    };
    let count: u64 = count.parse().map_err(|_| {
        error(ErrorKind::InvalidNumber {
            token: count.to_string(),
        })
    })?;
    if count == 0 {
        return Err(error(ErrorKind::ZeroInterval));
    }
    let unit_seconds: u64 = match *unit {
        "seconds" => 1,
        "minutes" => 60,
        "hours" => 3600,
        "days" => 86400,
        _ => {
            return Err(error(ErrorKind::UnknownUnit {
                unit: unit.to_string(),
            }));
        }
    };
    let interval = count
        .checked_mul(unit_seconds)
        .filter(|&s| s <= MAX_INTERVAL_SECONDS)
        .ok_or_else(|| error(ErrorKind::TooLarge))?;
    Ok(Schedule {
        expression: expr.to_string(),
        interval_seconds: interval as i64,
    })
}

/// Sleep until the schedule's next occurrence after now, returning that occurrence.
/// The only clock read in the scheduling path — tick functions take `now` as a parameter
/// (spec §3).
pub async fn sleep_until_next(schedule: &Schedule) -> Timestamp {
    let now = Timestamp::now();
    let next = schedule.next_after(now);
    tokio::time::sleep(next.duration_since(now).unsigned_abs()).await;
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn normalization_table() {
        let table = [
            ("Every 10 minutes", "every 10 minutes"),
            ("Every 1 minutes", "every 1 minutes"),
            ("every 5 minutes", "every 5 minutes"),
            ("every 30 seconds", "every 30 seconds"),
            ("every 2 hours", "every 2 hours"),
            ("every 1 minute", "every 1 minutes"),
            ("every 1 second", "every 1 seconds"),
            ("every 1 hour", "every 1 hours"),
            ("every 1 day", "every 1 days"),
            ("  Every   10\tMINUTES  ", "every 10 minutes"),
            ("every weekday at 09:00", "every weekday at 09:00"),
            ("at 10:00 am", "at 10:00 am"),
            ("banana", "banana"),
            ("", ""),
            ("every -5 minutes", "every -5 minutes"),
        ];
        for (input, expected) in table {
            assert_eq!(normalize(input), expected, "normalize({input:?})");
            // Idempotence: normalizing a normalized form is a no-op.
            assert_eq!(normalize(expected), expected, "idempotence for {input:?}");
        }
    }

    #[test]
    fn real_world_forms_parse_with_expected_spacing() {
        let table = [
            ("Every 10 minutes", 600),
            ("Every 1 minutes", 60),
            ("every 1 minute", 60),
            ("every 5 minutes", 300),
            ("every 30 seconds", 30),
            ("every 2 hours", 7200),
            ("every 3 days", 259_200),
        ];
        for (expr, seconds) in table {
            let schedule = parse(expr).unwrap();
            assert_eq!(schedule.interval_seconds(), seconds, "{expr}");
            let first = schedule.next_after(ts("2026-07-18T12:34:56Z"));
            let second = schedule.next_after(first);
            let third = schedule.next_after(second);
            assert_eq!((second - first).get_seconds(), seconds, "{expr}");
            assert_eq!((third - second).get_seconds(), seconds, "{expr}");
        }
    }

    #[test]
    fn occurrences_are_wall_clock_aligned() {
        // Epoch anchoring == wall-clock alignment for real-world intervals (README).
        let schedule = parse("Every 10 minutes").unwrap();
        assert_eq!(
            schedule.next_after(ts("2026-07-18T12:34:56Z")),
            ts("2026-07-18T12:40:00Z")
        );
        let schedule = parse("every 30 seconds").unwrap();
        assert_eq!(
            schedule.next_after(ts("2026-07-18T12:34:56Z")),
            ts("2026-07-18T12:35:00Z")
        );
    }

    #[test]
    fn next_after_is_strictly_after() {
        let schedule = parse("every 1 minutes").unwrap();
        // Exactly on a boundary: the next occurrence is the following one.
        assert_eq!(
            schedule.next_after(ts("2026-07-18T12:35:00Z")),
            ts("2026-07-18T12:36:00Z")
        );
        // Sub-second past a boundary still lands on the next boundary.
        assert_eq!(
            schedule.next_after(ts("2026-07-18T12:35:00.5Z")),
            ts("2026-07-18T12:36:00Z")
        );
        // Pre-epoch timestamps behave (div_euclid, not integer division).
        assert_eq!(
            schedule.next_after(ts("1969-12-31T23:59:30Z")),
            ts("1970-01-01T00:00:00Z")
        );
    }

    #[test]
    fn display_and_try_from_round_trip() {
        let schedule: Schedule = String::from("Every 10 minutes").try_into().unwrap();
        assert_eq!(schedule.to_string(), "Every 10 minutes");
        let err = Schedule::try_from(String::from("banana")).unwrap_err();
        assert!(err.to_string().contains("banana"));
    }

    #[test]
    fn errors_name_the_original_expression() {
        let table = [
            ("", "expression is empty"),
            ("   ", "expression is empty"),
            ("banana", "expected \"every N seconds|minutes|hours|days\""),
            ("every weekday at 09:00", "expected \"every"),
            ("at 10:00 am", "expected \"every"),
            ("every -5 minutes", "\"-5\" is not a positive integer"),
            ("every 1.5 minutes", "\"1.5\" is not a positive integer"),
            ("every 0 minutes", "interval must be at least 1"),
            ("every 5 fortnights", "unknown unit \"fortnights\""),
            ("every 99999999999 days", "exceeds the 10-year maximum"),
            (
                "every 99999999999999999999 seconds",
                "not a positive integer",
            ),
        ];
        for (expr, expected_detail) in table {
            let err = parse(expr).unwrap_err();
            let message = err.to_string();
            assert!(
                message.contains(expected_detail),
                "{expr:?} => {message}, expected detail {expected_detail:?}"
            );
            assert!(
                message.contains(&format!("\"{expr}\"")),
                "{expr:?} error must quote the original expression, got: {message}"
            );
        }
    }

    #[test]
    fn fuzzish_parse_never_panics() {
        // Deterministic xorshift; no randomness APIs needed. Every generated string must
        // produce Ok or Err — never a panic — and normalize must stay idempotent.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..2000 {
            let len = (next() % 24) as usize;
            let s: String = (0..len)
                .map(|_| char::from((0x20 + (next() % 0x5F)) as u8)) // printable ASCII
                .collect();
            let _ = parse(&s);
            assert_eq!(normalize(&normalize(&s)), normalize(&s));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn sleep_until_next_wakes_at_next_occurrence() {
        let schedule = parse("every 1 seconds").unwrap();
        let before = Timestamp::now();
        let woke_for = sleep_until_next(&schedule).await;
        assert_eq!(woke_for, schedule.next_after(before));
    }
}
