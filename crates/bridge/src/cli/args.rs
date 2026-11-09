//! Command-line parsing for `passporter`.

use std::ffi::OsString;
use std::path::PathBuf;

use protocol::{DeviceId, DeviceName};

/// The usage text.
pub(crate) const USAGE: &str = "usage: passporter [--config-dir <dir>] <command>

commands:
  pair [--name <name>]       show a one-time pairing code (QR and text; 5 minutes, single use)
  devices list               list paired devices
  devices revoke <id>        revoke a device and close its open connections
  doctor                     check the deployment (modes, listen, auth, herdr, whisper, push, unit)";

/// What to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    Help,
    Pair { name: Option<DeviceName> },
    List,
    Revoke { id: DeviceId },
    Doctor,
}

/// The config dir (if given) and the command, or a usage error.
pub(crate) fn parse(
    args: impl Iterator<Item = OsString>,
) -> Result<(Option<PathBuf>, Command), String> {
    let mut config_dir = None;
    let mut words = Vec::new();
    let mut args = args;
    while let Some(arg) = args.next() {
        let Some(arg) = arg.to_str().map(str::to_owned) else {
            return Err("arguments must be UTF-8".to_owned());
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok((config_dir, Command::Help)),
            "--config-dir" => {
                let dir = args.next().ok_or("--config-dir needs a value")?;
                config_dir = Some(PathBuf::from(dir));
            }
            _ => words.push(arg),
        }
    }
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let command = match words.as_slice() {
        [] => return Err("missing command".to_owned()),
        ["pair"] => Command::Pair { name: None },
        ["pair", "--name", name] => Command::Pair {
            name: Some(DeviceName::new(*name).map_err(|e| e.to_string())?),
        },
        ["devices", "list"] => Command::List,
        ["doctor"] => Command::Doctor,
        ["devices", "revoke", id] => Command::Revoke {
            id: DeviceId::new(*id).map_err(|e| e.to_string())?,
        },
        other => return Err(format!("unexpected arguments: {}", other.join(" "))),
    };
    Ok((config_dir, command))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(args: &[&str]) -> Result<(Option<PathBuf>, Command), String> {
        parse(args.iter().map(OsString::from))
    }

    #[test]
    fn parses_every_command() {
        assert_eq!(
            parse_str(&["pair"]).expect("ok"),
            (None, Command::Pair { name: None })
        );
        let (dir, named) =
            parse_str(&["--config-dir", "/c", "pair", "--name", "Pixel"]).expect("ok");
        assert_eq!(dir, Some(PathBuf::from("/c")));
        assert!(matches!(named, Command::Pair { name: Some(n) } if n.as_str() == "Pixel"));
        assert_eq!(
            parse_str(&["devices", "list"]).expect("ok").1,
            Command::List
        );
        assert!(matches!(
            parse_str(&["devices", "revoke", "dev-0123456789abcdef"])
                .expect("ok")
                .1,
            Command::Revoke { .. }
        ));
        assert_eq!(parse_str(&["--help"]).expect("ok").1, Command::Help);
        assert_eq!(
            parse_str(&["--config-dir", "/c", "doctor"]).expect("ok"),
            (Some(PathBuf::from("/c")), Command::Doctor)
        );
    }

    #[test]
    fn rejects_junk() {
        for bad in [
            &[][..],
            &["devices"],
            &["devices", "revoke", "nope"],
            &["pair", "--name", "a\u{1b}b"],
            &["unpair"],
            &["doctor", "--fix"],
            &["--config-dir"],
        ] {
            assert!(parse_str(bad).is_err(), "{bad:?}");
        }
    }
}
