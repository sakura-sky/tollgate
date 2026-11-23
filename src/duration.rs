// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Andrew Stevens

//! Duration settings, such as `TOLLGATE_RELOAD__INTERVAL=15s`.
//!
//! A duration is a whole number followed by one unit: `ms`, `s`, `m`, `h` or
//! `d`. Surrounding whitespace is ignored. Nothing else is accepted: no
//! fractions, no signs, no compound forms such as `1m30s`, and no other unit
//! names. `0s` is zero: `reload.interval` reads it as "never reload" and
//! `retention.window` as "keep everything", while `database.acquire_timeout`
//! refuses it.
//!
//! The format is closed so that a value has one reading. The five units cover
//! every value Tollgate documents or ships, and each means what it meant under
//! the parser this replaced; that parser also took compound values, fractions
//! and many more unit names, including `M` (months) beside `m` (minutes). An
//! `ms` value stops at `u64::MAX / 1_000_000` milliseconds (about 584 years),
//! the previous parser's ceiling, so nothing it refused is accepted now.
//!
//! Used through serde: `#[serde(with = "crate::duration")]`.

use std::fmt;
use std::time::Duration;

use serde::{Deserializer, Serializer, de};

const UNITS: &str = "ms, s, m, h or d";

/// The largest `ms` value accepted: the previous parser counted milliseconds
/// in nanoseconds, in a `u64`.
const MAX_MS: u64 = u64::MAX / 1_000_000;

/// Why a duration setting was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Empty,
    /// Not a whole number followed by a unit.
    Malformed(String),
    /// A number with no unit after it.
    MissingUnit(String),
    UnknownUnit {
        input: String,
        unit: String,
    },
    /// The number does not fit a `u64`, the duration does not fit a
    /// `Duration`, or an `ms` value is above `u64::MAX / 1_000_000`, which a
    /// `Duration` could hold but the previous parser refused.
    TooLarge(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Each message ends with its advice in brackets, because figment
            // appends " for key ..." straight after it.
            Self::Empty => write!(
                f,
                "empty duration (use a whole number and one unit, such as 15s)"
            ),
            Self::Malformed(s) => write!(
                f,
                "{s:?} is not a duration (use a whole number and one unit: {UNITS}, such as 15s or 90d)"
            ),
            Self::MissingUnit(s) => {
                write!(f, "{s:?} has no unit (add one of {UNITS}, such as {s}s)")
            }
            Self::UnknownUnit { input, unit } => {
                write!(f, "{input:?} has an unknown unit {unit:?} (use {UNITS})")
            }
            Self::TooLarge(s) => write!(f, "{s:?} is too large for a duration"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse a duration setting.
///
/// # Errors
/// Returns a [`ParseError`] for anything other than a whole number followed by
/// one of the units above.
pub fn parse(input: &str) -> Result<Duration, ParseError> {
    let s = input.trim();
    if s.is_empty() {
        return Err(ParseError::Empty);
    }
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (digits, unit) = s.split_at(split);
    if digits.is_empty() || unit.contains(|c: char| !c.is_ascii_alphabetic()) {
        return Err(ParseError::Malformed(s.to_owned()));
    }
    if unit.is_empty() {
        return Err(ParseError::MissingUnit(s.to_owned()));
    }
    // Only digits remain, so the one way this fails is overflow.
    let n: u64 = digits
        .parse()
        .map_err(|_| ParseError::TooLarge(s.to_owned()))?;
    let secs = |per: u64| {
        n.checked_mul(per)
            .map(Duration::from_secs)
            .ok_or_else(|| ParseError::TooLarge(s.to_owned()))
    };
    match unit {
        // The same ceiling as the parser this replaced: about 584 years.
        "ms" if n <= MAX_MS => Ok(Duration::from_millis(n)),
        "ms" => Err(ParseError::TooLarge(s.to_owned())),
        "s" => secs(1),
        "m" => secs(60),
        "h" => secs(60 * 60),
        "d" => secs(24 * 60 * 60),
        _ => Err(ParseError::UnknownUnit {
            input: s.to_owned(),
            unit: unit.to_owned(),
        }),
    }
}

/// Format a duration in the form [`parse`] reads back exactly: whole seconds as
/// `Ns`, anything else as `Nms`. `None` if that is impossible: a duration with
/// a fraction of a millisecond, or one that is not whole seconds and is above
/// the `ms` ceiling (`u64::MAX / 1_000_000` milliseconds).
#[must_use]
pub fn format(d: Duration) -> Option<String> {
    if d.subsec_nanos() == 0 {
        return Some(format!("{}s", d.as_secs()));
    }
    if !d.subsec_nanos().is_multiple_of(1_000_000) {
        return None;
    }
    let ms = u64::try_from(d.as_millis()).ok()?;
    (ms <= MAX_MS).then(|| format!("{ms}ms"))
}

/// serde `deserialize_with`. Accepts strings only, as the previous parser did:
/// an unquoted `15` in TOML, or `=15` in the environment, is a number and is
/// refused. The config loader hands such a number to this visitor, so the
/// message is the same "has no unit" one as for the string `"15"`.
///
/// # Errors
/// Returns the deserialiser's error carrying the [`ParseError`] message.
pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    struct V;
    impl de::Visitor<'_> for V {
        type Value = Duration;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                f,
                "a duration (a whole number and one unit, such as 15s or 90d)"
            )
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<Duration, E> {
            parse(v).map_err(E::custom)
        }
        // A number, such as `TOLLGATE_RELOAD__INTERVAL=0`: refused, with
        // the unit it needs.
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Duration, E> {
            Err(E::custom(ParseError::MissingUnit(v.to_string())))
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Duration, E> {
            Err(E::custom(if v < 0 {
                ParseError::Malformed(v.to_string())
            } else {
                ParseError::MissingUnit(v.to_string())
            }))
        }
    }
    d.deserialize_str(V)
}

/// serde `serialize_with`. The config defaults pass through this and back
/// through [`deserialize`] on every load, so the two must agree exactly.
///
/// # Errors
/// Fails where [`format`] returns `None`: a duration with a fraction of a
/// millisecond, or one that is not whole seconds and is above the `ms`
/// ceiling. No setting can express either.
pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
    match format(*d) {
        Some(text) => s.serialize_str(&text),
        None => Err(serde::ser::Error::custom(format!(
            "{d:?} cannot be written as a duration setting (whole seconds, or whole milliseconds up to {MAX_MS}ms)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 60 * 60;

    /// Every duration value Tollgate documents or ships, with its meaning
    /// written out by hand rather than computed by the parser under test.
    #[test]
    fn every_shipped_value_parses_to_its_documented_meaning() {
        for (text, want) in [
            ("60s", Duration::from_secs(60)),
            ("15s", Duration::from_secs(15)),
            ("5s", Duration::from_secs(5)),
            ("600s", Duration::from_secs(600)),
            ("90d", Duration::from_secs(90 * DAY)),
            ("0s", Duration::ZERO),
            ("1ms", Duration::from_millis(1)),
            ("2147483647ms", Duration::from_millis(2_147_483_647)),
        ] {
            assert_eq!(parse(text), Ok(want), "{text:?}");
        }
    }

    #[test]
    fn each_unit_and_whitespace() {
        assert_eq!(parse("10m"), Ok(Duration::from_secs(600)));
        assert_eq!(parse("2h"), Ok(Duration::from_secs(7_200)));
        assert_eq!(parse("1d"), Ok(Duration::from_secs(DAY)));
        assert_eq!(parse(" 15s\n"), Ok(Duration::from_secs(15)));
        assert_eq!(parse("007s"), Ok(Duration::from_secs(7)));
        assert_eq!(parse("0ms"), Ok(Duration::ZERO));
        assert_eq!(parse("0d"), Ok(Duration::ZERO));
    }

    #[test]
    fn refusals_name_the_problem() {
        assert_eq!(parse(""), Err(ParseError::Empty));
        assert_eq!(parse("   "), Err(ParseError::Empty));
        assert!(matches!(parse("15"), Err(ParseError::MissingUnit(_))));
        for bad in [
            "1m30s",
            "1h 30m",
            "1.5h",
            "-1s",
            "+1s",
            "s",
            "1 s",
            "1_000s",
            "1e3s",
            "\u{ff11}s",
        ] {
            assert!(
                matches!(parse(bad), Err(ParseError::Malformed(_))),
                "{bad:?}"
            );
        }
        for (bad, unit) in [
            ("5min", "min"),
            ("10sec", "sec"),
            ("1w", "w"),
            ("1M", "M"),
            ("1y", "y"),
            ("1us", "us"),
            ("1ns", "ns"),
            ("1S", "S"),
            ("1D", "D"),
            ("90days", "days"),
        ] {
            assert_eq!(
                parse(bad),
                Err(ParseError::UnknownUnit {
                    input: bad.to_owned(),
                    unit: unit.to_owned()
                }),
                "{bad:?}"
            );
        }
        let msg = parse("1m30s").unwrap_err().to_string();
        assert!(msg.contains("ms, s, m, h or d"), "{msg}");
    }

    #[test]
    fn overflow_is_refused_not_wrapped() {
        let max = u64::MAX;
        assert_eq!(parse(&format!("{max}s")), Ok(Duration::from_secs(max)));
        let max_ms = max / 1_000_000;
        assert_eq!(
            parse(&format!("{max_ms}ms")),
            Ok(Duration::from_millis(max_ms))
        );
        assert!(matches!(
            parse(&format!("{}ms", max_ms + 1)),
            Err(ParseError::TooLarge(_))
        ));
        assert!(matches!(
            parse(&format!("{max}m")),
            Err(ParseError::TooLarge(_))
        ));
        assert!(matches!(
            parse(&format!("{}d", max / DAY + 1)),
            Err(ParseError::TooLarge(_))
        ));
        assert_eq!(
            parse(&format!("{}d", max / DAY)),
            Ok(Duration::from_secs(max / DAY * DAY))
        );
        assert!(matches!(
            parse("18446744073709551616s"),
            Err(ParseError::TooLarge(_))
        ));
    }

    /// The config defaults are serialised and parsed back on every load, so a
    /// formatted value must come back unchanged. Control: a fraction of a
    /// millisecond cannot be expressed and is refused rather than rounded.
    #[test]
    fn format_round_trips() {
        for d in [
            Duration::ZERO,
            Duration::from_millis(1),
            Duration::from_millis(1_500),
            Duration::from_secs(15),
            Duration::from_secs(90 * DAY),
            Duration::from_secs(u64::MAX),
            Duration::from_millis(u64::MAX / 1_000_000),
        ] {
            let text = format(d).expect("representable");
            assert_eq!(parse(&text), Ok(d), "{text}");
        }
        assert_eq!(format(Duration::from_micros(1_500)), None);
        assert_eq!(
            format(Duration::from_secs(u64::MAX) + Duration::from_millis(1)),
            None
        );
        assert_eq!(
            format(Duration::from_millis(u64::MAX / 1_000_000 + 1)),
            None
        );
    }

    #[test]
    fn serde_accepts_strings_and_refuses_numbers() {
        #[derive(serde::Deserialize, serde::Serialize, Debug, PartialEq)]
        struct S {
            #[serde(with = "super")]
            d: Duration,
        }
        let v: S = serde_json::from_str(r#"{"d":"90d"}"#).unwrap();
        assert_eq!(v.d, Duration::from_secs(90 * DAY));
        assert_eq!(serde_json::to_string(&v).unwrap(), r#"{"d":"7776000s"}"#);
        // serde_json refuses a number before the visitor sees it; the
        // config loader's numbers reach the visitor (tested in config.rs).
        let err = serde_json::from_str::<S>(r#"{"d":15}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("expected a duration"), "{err}");
        let err = serde_json::from_str::<S>(r#"{"d":"1m30s"}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("ms, s, m, h or d"), "{err}");
        // Whole milliseconds above the ceiling cannot be written out either.
        let err = serde_json::to_string(&S {
            d: Duration::from_millis(MAX_MS + 1),
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("up to 18446744073709ms"), "{err}");
        let err = serde_json::to_string(&S {
            d: Duration::from_micros(1_500),
        })
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("cannot be written as a duration setting"),
            "{err}"
        );
    }
}
