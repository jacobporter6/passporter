//! RFC 3339 record timestamps to [`Timestamp`].

use protocol::Timestamp;

/// Parses the timestamps Claude Code writes (`2026-11-07T09:15:02.123Z`;
/// numeric offsets are accepted too). Returns `None` for anything else,
/// including instants before the unix epoch: a missing timestamp is
/// harmless, a wrong one is not.
pub(crate) fn parse_rfc3339(s: &str) -> Option<Timestamp> {
    let b = s.as_bytes();
    let (date, rest) = (b.get(..10)?, b.get(10..)?);
    if date.get(4) != Some(&b'-') || date.get(7) != Some(&b'-') {
        return None;
    }
    let year = digits(date.get(0..4)?)?;
    let month = digits(date.get(5..7)?)?;
    let day = digits(date.get(8..10)?)?;

    if !matches!(rest.first(), Some(b'T' | b't' | b' ')) || rest.get(3) != Some(&b':') {
        return None;
    }
    let hour = digits(rest.get(1..3)?)?;
    let minute = digits(rest.get(4..6)?)?;
    if rest.get(6) != Some(&b':') {
        return None;
    }
    let second = digits(rest.get(7..9)?)?;
    let mut rest = rest.get(9..)?;

    let mut millis = 0;
    if rest.first() == Some(&b'.') {
        let frac_len = rest
            .iter()
            .skip(1)
            .take_while(|c| c.is_ascii_digit())
            .count();
        if frac_len == 0 {
            return None;
        }
        let frac = rest.get(1..=frac_len)?;
        // Normalise to exactly three digits: truncate or right-pad.
        for i in 0..3 {
            millis = millis * 10 + frac.get(i).map_or(0, |d| i64::from(d - b'0'));
        }
        rest = rest.get(frac_len + 1..)?;
    }

    let offset_secs = match rest {
        b"Z" | b"z" => 0,
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] => {
            let secs = digits(&[*h1, *h2])? * 3600 + digits(&[*m1, *m2])? * 60;
            if *sign == b'+' { secs } else { -secs }
        }
        _ => return None,
    };

    if !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs;
    let millis = secs.checked_mul(1000)?.checked_add(millis)?;
    u64::try_from(millis).ok().map(Timestamp::from_unix_millis)
}

fn digits(b: &[u8]) -> Option<i64> {
    b.iter().try_fold(0_i64, |acc, &c| {
        c.is_ascii_digit().then(|| acc * 10 + i64::from(c - b'0'))
    })
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn millis(s: &str) -> Option<u64> {
        parse_rfc3339(s).map(Timestamp::unix_millis)
    }

    #[test]
    fn parses_claude_timestamps() {
        assert_eq!(millis("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(millis("2026-11-07T09:15:02.123Z"), Some(1_794_042_902_123));
        assert_eq!(millis("2024-02-29T23:59:59.9Z"), Some(1_709_251_199_900));
        assert_eq!(
            millis("2024-02-29T23:59:59.123456Z"),
            Some(1_709_251_199_123)
        );
        assert_eq!(
            millis("2026-11-07T10:15:02.123+01:00"),
            millis("2026-11-07T09:15:02.123Z")
        );
        assert_eq!(
            millis("2026-11-07T04:15:02-05:00"),
            millis("2026-11-07T09:15:02Z")
        );
    }

    #[test]
    fn rejects_garbage() {
        for s in [
            "",
            "yesterday",
            "2026-11-07",
            "2026-11-07T09:15:02",
            "2026-13-07T09:15:02Z",
            "2025-02-29T09:15:02Z",
            "2026-11-07T24:00:00Z",
            "2026-11-07T09:15:02.Z",
            "2026-11-07T09:15:02Zjunk",
            "1969-12-31T23:59:59Z",
            "2026-1a-07T09:15:02Z",
        ] {
            assert_eq!(millis(s), None, "{s:?}");
        }
    }
}
