//! `--since` and `--until` values.

use std::str::FromStr;

use jiff::{SignedDuration, Timestamp};

/// A moment, given as an RFC 3339 time or a duration ago: `90s`, `30m`,
/// `6h`, `1h30m`, `2d`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct When(Timestamp);

impl When {
    pub const fn at(self) -> Timestamp {
        self.0
    }

    fn parse_at(text: &str, now: Timestamp) -> Result<Self, String> {
        if let Ok(at) = text.parse::<Timestamp>() {
            return Ok(Self(at));
        }
        // Days aren't a fixed length in general, but "2d ago" means 48 hours here.
        let ago = match text.strip_suffix('d').and_then(|days| days.parse::<i64>().ok()) {
            Some(days) => days.checked_mul(24).map(SignedDuration::from_hours),
            None => text.parse::<SignedDuration>().ok(),
        };
        ago.filter(|ago| ago.is_positive())
            .and_then(|ago| now.checked_sub(ago).ok())
            .map(Self)
            .ok_or_else(|| {
                format!("{text:?} is not a duration like 30m, 6h or 2d, or an RFC 3339 time")
            })
    }
}

impl FromStr for When {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        Self::parse_at(text, Timestamp::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_and_times() {
        let now: Timestamp = "2026-09-13T12:00:00Z".parse().unwrap();
        let at = |text: &str| When::parse_at(text, now).map(When::at);
        let time = |text: &str| text.parse::<Timestamp>().unwrap();
        assert_eq!(at("90m"), Ok(time("2026-09-13T10:30:00Z")));
        assert_eq!(at("6h"), Ok(time("2026-09-13T06:00:00Z")));
        assert_eq!(at("1h30m"), Ok(time("2026-09-13T10:30:00Z")));
        assert_eq!(at("2d"), Ok(time("2026-09-11T12:00:00Z")));
        assert_eq!(at("2026-09-12T08:00:00-05:00"), Ok(time("2026-09-12T13:00:00Z")));
        assert!(at("yesterday").is_err());
        assert!(at("-5m").is_err());
    }
}
