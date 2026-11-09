//! Just enough DER to read a certificate's validity period (RFC 5280
//! §4.1.2.5), which neither rustls nor webpki exposes. The bridge refuses
//! a leaf outside it, and `passporter doctor` reports it.

use std::fmt;
use std::time::{Duration, SystemTime};

const SEQUENCE: u8 = 0x30;
const VERSION: u8 = 0xa0;
const UTC_TIME: u8 = 0x17;
const GENERALIZED_TIME: u8 = 0x18;

/// One end of a certificate's validity period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CertTime {
    /// The instant.
    pub at: SystemTime,
    /// Its calendar fields, UTC: year, month, day, hour, minute, second.
    fields: [u32; 6],
}

impl CertTime {
    /// How long until this time, seen from `now`; `None` once it has passed.
    #[must_use]
    pub fn remaining(&self, now: SystemTime) -> Option<Duration> {
        self.at.duration_since(now).ok()
    }
}

/// When a certificate is valid: from `not_before` to `not_after`, inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Validity {
    /// The first valid instant.
    pub not_before: CertTime,
    /// The last valid instant.
    pub not_after: CertTime,
}

impl fmt::Display for CertTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [year, month, day, hour, minute, second] = self.fields;
        write!(
            f,
            "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC"
        )
    }
}

/// One DER element of `input`: its tag, its contents and what follows.
fn element(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let width = usize::from(first & 0x7f);
        if width == 0 || width > 4 || rest.len() < width {
            return None;
        }
        let (bytes, rest) = rest.split_at(width);
        let len = bytes
            .iter()
            .fold(0usize, |len, &byte| (len << 8) | usize::from(byte));
        (len, rest)
    };
    (rest.len() >= len).then(|| {
        let (contents, rest) = rest.split_at(len);
        (tag, contents, rest)
    })
}

/// The contents of the element at the start of `input` if it has `tag`.
fn expect(input: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
    element(input).and_then(|(found, contents, rest)| (found == tag).then_some((contents, rest)))
}

/// The validity period of a DER certificate, `None` if it does not parse.
pub(super) fn validity(cert: &[u8]) -> Option<Validity> {
    let (cert, _) = expect(cert, SEQUENCE)?;
    let (tbs, _) = expect(cert, SEQUENCE)?;
    // version (optional, explicitly tagged), serialNumber, signature, issuer.
    let mut rest = expect(tbs, VERSION).map_or(tbs, |(_, rest)| rest);
    for _ in 0..3 {
        rest = element(rest)?.2;
    }
    let (validity, _) = expect(rest, SEQUENCE)?;
    let (before_tag, before, rest) = element(validity)?;
    let (after_tag, after, _) = element(rest)?;
    Some(Validity {
        not_before: parse_time(before_tag, before)?,
        not_after: parse_time(after_tag, after)?,
    })
}

/// A `UTCTime` (`YYMMDDHHMMSSZ`) or `GeneralizedTime` (`YYYYMMDDHHMMSSZ`), the
/// two forms RFC 5280 allows.
fn parse_time(tag: u8, text: &[u8]) -> Option<CertTime> {
    let digits = text.strip_suffix(b"Z")?;
    if !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let number = |range: std::ops::Range<usize>| {
        digits
            .get(range)?
            .iter()
            .try_fold(0u32, |n, &d| Some(n * 10 + u32::from(d - b'0')))
    };
    let (year, rest) = match (tag, digits.len()) {
        (UTC_TIME, 12) => {
            let yy = number(0..2)?;
            (if yy < 50 { 2000 + yy } else { 1900 + yy }, 2)
        }
        (GENERALIZED_TIME, 14) => (number(0..4)?, 4),
        _ => return None,
    };
    let field = |i: usize| number(rest + 2 * i..rest + 2 * i + 2);
    let fields = [year, field(0)?, field(1)?, field(2)?, field(3)?, field(4)?];
    let [_, month, day, hour, minute, second] = fields;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = days_from_civil(i64::from(year), month, day);
    let secs = days * 86_400 + i64::from(hour * 3600 + minute * 60 + second);
    let at = SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(u64::try_from(secs).ok()?))?;
    Some(CertTime { at, fields })
}

/// Days from 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_time_forms_parse() {
        let utc = parse_time(UTC_TIME, b"270102030405Z").expect("utc");
        assert_eq!(utc.to_string(), "2027-01-02 03:04:05 UTC");
        let general = parse_time(GENERALIZED_TIME, b"20500101000000Z").expect("general");
        assert_eq!(general.to_string(), "2050-01-01 00:00:00 UTC");
        assert_eq!(
            general
                .at
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("after")
                .as_secs(),
            2_524_608_000
        );
        let last_century = parse_time(UTC_TIME, b"991231235959Z").expect("1999");
        assert_eq!(last_century.to_string(), "1999-12-31 23:59:59 UTC");
    }

    #[test]
    fn malformed_times_are_refused() {
        for (tag, text) in [
            (UTC_TIME, &b"2701020304Z"[..]),
            (UTC_TIME, b"271302030405Z"),
            (UTC_TIME, b"270102030405"),
            (GENERALIZED_TIME, b"270102030405Z"),
            (GENERALIZED_TIME, b"2027010203040+Z"),
            (0x04, b"270102030405Z"),
        ] {
            assert!(parse_time(tag, text).is_none(), "{text:?}");
        }
    }

    #[test]
    fn days_from_civil_matches_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(2026, 11, 8), 20_765);
    }

    #[test]
    fn truncated_der_is_refused() {
        assert!(validity(&[]).is_none());
        assert!(validity(&[SEQUENCE, 0x85, 1, 2, 3, 4, 5]).is_none());
        assert!(validity(&[SEQUENCE, 0x05, 1]).is_none());
    }
}
