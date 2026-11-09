//! What `passporter` prints.

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use bridge::auth::DeviceInfo;
use protocol::PairingCode;
use qrcode::QrCode;
use qrcode::render::unicode::Dense1x2;

/// The pairing screen: a terminal QR of the code, the code as text, and
/// how long it lasts. The QR is drawn light-on-dark (dark modules as
/// blanks), which is what scans on a dark terminal.
pub(crate) fn pairing(code: &PairingCode, expires_in_secs: u64) -> String {
    let mut out = String::new();
    if let Ok(qr) = QrCode::new(code.as_str()) {
        let image = qr
            .render::<Dense1x2>()
            .dark_color(Dense1x2::Light)
            .light_color(Dense1x2::Dark)
            .quiet_zone(true)
            .build();
        out.push_str(&image);
        out.push('\n');
    }
    let minutes = expires_in_secs.div_ceil(60);
    let _ = writeln!(
        out,
        "\nPairing code: {code}\n\nScan it on the phone's Connect screen, or type the code.\n\
         Valid for {minutes} minute(s), single use."
    );
    out
}

/// The device table.
pub(crate) fn devices(devices: &[DeviceInfo], now_millis: u64) -> String {
    if devices.is_empty() {
        return "No paired devices. Run `passporter pair` to add one.\n".to_owned();
    }
    let width = devices
        .iter()
        .map(|d| d.record.name.as_str().chars().count())
        .max()
        .unwrap_or(0)
        .max("NAME".len());
    let mut out = format!(
        "{:<20}  {:<width$}  {:<14}  SESSIONS\n",
        "ID", "NAME", "PAIRED"
    );
    for device in devices {
        let age = ago(now_millis.saturating_sub(device.record.paired_at.unix_millis()));
        let _ = writeln!(
            out,
            "{:<20}  {:<width$}  {:<14}  {}",
            device.record.id.as_str(),
            device.record.name.as_str(),
            age,
            device.sessions
        );
    }
    out
}

/// "5 min ago", "3 h ago", "2 d ago".
fn ago(millis: u64) -> String {
    let secs = millis / 1000;
    match secs {
        0..60 => "just now".to_owned(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86_400 => format!("{} h ago", secs / 3600),
        _ => format!("{} d ago", secs / 86_400),
    }
}

/// The wall clock, in unix milliseconds.
pub(crate) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use bridge::auth::DeviceRecord;
    use protocol::{DeviceId, DeviceName, PublicKey, Timestamp};

    use super::*;

    #[test]
    fn pairing_shows_the_qr_and_the_code() {
        let code = PairingCode::new("ABCD2345").expect("code");
        let text = pairing(&code, 300);
        assert!(text.contains("ABCD-2345"), "{text}");
        assert!(text.contains("5 minute(s)"), "{text}");
        assert!(text.contains('█') || text.contains('▀'), "{text}");
    }

    #[test]
    fn devices_render_as_a_table() {
        assert!(devices(&[], 0).contains("passporter pair"));
        let info = DeviceInfo {
            record: DeviceRecord {
                id: DeviceId::new("dev-0123456789abcdef").expect("id"),
                name: DeviceName::new("Pixel 8").expect("name"),
                public_key: PublicKey::new(format!("04{}", "ab".repeat(64))).expect("key"),
                paired_at: Timestamp::from_unix_millis(0),
            },
            sessions: 2,
        };
        let table = devices(&[info], 2 * 3_600_000);
        assert!(table.contains("dev-0123456789abcdef"), "{table}");
        assert!(table.contains("Pixel 8"), "{table}");
        assert!(table.contains("2 h ago"), "{table}");
    }
}
