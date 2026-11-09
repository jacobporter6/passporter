//! Human formatting for times: "4m", "2h 5m", "as of 14:02".

use protocol::Timestamp;

const SECOND: u64 = 1_000;
const MINUTE: u64 = 60 * SECOND;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

/// A compact duration for "time in state": `now`, `42s`, `4m`, `2h 5m`,
/// `3d`. Coarse on purpose; a card ticks once a second at most.
#[must_use]
pub fn compact_duration(millis: u64) -> String {
    match millis {
        m if m < 5 * SECOND => "now".to_owned(),
        m if m < MINUTE => format!("{}s", m / SECOND),
        m if m < HOUR => format!("{}m", m / MINUTE),
        m if m < DAY => {
            let minutes = (m % HOUR) / MINUTE;
            if minutes == 0 {
                format!("{}h", m / HOUR)
            } else {
                format!("{}h {minutes}m", m / HOUR)
            }
        }
        m => format!("{}d", m / DAY),
    }
}

/// How long ago `since` was, compactly, against `now`.
#[must_use]
pub fn time_in_state(since: Timestamp, now: Timestamp) -> String {
    compact_duration(since.millis_until(now))
}

/// A 24-hour wall-clock time, `HH:MM`.
#[must_use]
pub fn clock(hours: u32, minutes: u32) -> String {
    format!("{hours:02}:{minutes:02}")
}

/// `HH:MM` of `at` in the device's local time zone. Browser only.
#[must_use]
pub fn local_clock(at: Timestamp) -> String {
    #[allow(clippy::cast_precision_loss)] // exact below 2^53 ms (year 287 396)
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(at.unix_millis() as f64));
    clock(date.get_hours(), date.get_minutes())
}

/// The device clock now. Browser only.
#[must_use]
pub fn now() -> Timestamp {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // positive, < 2^53
    Timestamp::from_unix_millis(js_sys::Date::now() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_durations() {
        let cases = [
            (0, "now"),
            (4_999, "now"),
            (5_000, "5s"),
            (59_999, "59s"),
            (MINUTE, "1m"),
            (59 * MINUTE + 59 * SECOND, "59m"),
            (HOUR, "1h"),
            (2 * HOUR + 5 * MINUTE + 30 * SECOND, "2h 5m"),
            (DAY - 1, "23h 59m"),
            (3 * DAY + HOUR, "3d"),
        ];
        for (millis, want) in cases {
            assert_eq!(compact_duration(millis), want, "{millis}");
        }
    }

    #[test]
    fn clock_skew_reads_as_now() {
        let later = Timestamp::from_unix_millis(10_000);
        let earlier = Timestamp::from_unix_millis(1_000);
        assert_eq!(time_in_state(later, earlier), "now");
        assert_eq!(time_in_state(earlier, later), "9s");
    }

    #[test]
    fn clocks_are_zero_padded() {
        assert_eq!(clock(9, 5), "09:05");
        assert_eq!(clock(23, 59), "23:59");
    }
}
