//! The `[[runbooks]]` tables: named operations the phone may run (see
//! [`crate::runbooks`]). Everything a run executes comes from here: the
//! phone only ever names a runbook.
//!
//! ```toml
//! [[runbooks]]
//! name = "Deploy bridge"              # what the phone shows and sends
//! aliases = ["deploy the bridge"]     # more phrases that offer it by voice
//! cwd = "/home/me/git/passporter"     # absolute
//! argv = ["deploy/bridge/setup.sh"]   # executed directly, no shell
//! confirm = true                      # default; a tap asks first (speech always does)
//! timeout_secs = 600                  # default; killed after this long
//! ```

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use protocol::{RunbookName, normalise_phrase};
use serde::Deserialize;

use super::ParseError;

/// One configured runbook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunbookConfig {
    /// What the phone shows and sends (the path segment of its run route).
    pub name: RunbookName,
    /// More phrases that offer it when dictated (matched on the phone).
    pub aliases: Vec<String>,
    /// Where it runs: an absolute directory.
    pub cwd: PathBuf,
    /// The program and its arguments, executed directly (no shell). A
    /// relative program with a `/` is resolved against [`Self::cwd`].
    pub argv: Vec<String>,
    /// Whether a tap asks before running; speech always asks.
    pub confirm: bool,
    /// How long a run may take before it is killed.
    pub timeout: Duration,
}

/// Most `[[runbooks]]` a config may have.
const MAX_RUNBOOKS: usize = 32;
/// Most aliases per runbook.
const MAX_ALIASES: usize = 8;
/// Longest alias, in characters.
const MAX_ALIAS_CHARS: usize = 80;
/// Most `argv` elements.
const MAX_ARGS: usize = 64;
/// Longest `argv` element, in bytes.
const MAX_ARG_BYTES: usize = 4096;
/// The default `timeout_secs`.
const DEFAULT_TIMEOUT_SECS: u64 = 600;
/// The longest `timeout_secs`: a day.
const MAX_TIMEOUT_SECS: u64 = 24 * 60 * 60;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawRunbook {
    name: RunbookName,
    #[serde(default)]
    aliases: Vec<String>,
    cwd: PathBuf,
    argv: Vec<String>,
    confirm: Option<bool>,
    timeout_secs: Option<u64>,
}

/// Checks every runbook, keeping config order.
pub(super) fn validate(raw: Vec<RawRunbook>) -> Result<Vec<RunbookConfig>, ParseError> {
    if raw.len() > MAX_RUNBOOKS {
        return Err(ParseError::TooManyRunbooks(MAX_RUNBOOKS));
    }
    // Names and aliases share one namespace, compared as the phone
    // matches speech (`normalise_phrase`): one spoken phrase must never
    // offer two runbooks, and every phrase must be sayable.
    let mut phrases = BTreeSet::new();
    let mut runbooks = Vec::with_capacity(raw.len());
    for runbook in raw {
        let bad = |problem: &'static str| ParseError::Runbook {
            name: runbook.name.to_string(),
            problem,
        };
        if !runbook.cwd.is_absolute() {
            return Err(bad("cwd must be an absolute path"));
        }
        if runbook
            .argv
            .first()
            .is_none_or(|program| program.trim().is_empty())
        {
            return Err(bad("argv must start with the program to run"));
        }
        if runbook.argv.len() > MAX_ARGS
            || runbook
                .argv
                .iter()
                .any(|arg| arg.len() > MAX_ARG_BYTES || arg.contains('\0'))
        {
            return Err(bad("argv is too long or holds a NUL byte"));
        }
        if runbook.aliases.len() > MAX_ALIASES
            || runbook.aliases.iter().any(|alias| {
                alias.trim().is_empty()
                    || alias.chars().count() > MAX_ALIAS_CHARS
                    || alias.chars().any(char::is_control)
            })
        {
            return Err(bad(
                "aliases must be at most 8 printable phrases of 1 to 80 characters",
            ));
        }
        let timeout_secs = runbook.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
        if !(1..=MAX_TIMEOUT_SECS).contains(&timeout_secs) {
            return Err(ParseError::OutOfRange {
                key: "runbooks.timeout_secs",
                min: 1,
                max: MAX_TIMEOUT_SECS,
            });
        }
        for phrase in
            std::iter::once(runbook.name.as_str()).chain(runbook.aliases.iter().map(String::as_str))
        {
            let spoken = normalise_phrase(phrase);
            if spoken.is_empty() {
                return Err(ParseError::RunbookPhraseUnsayable(phrase.to_owned()));
            }
            if !phrases.insert(spoken) {
                return Err(ParseError::RunbookPhraseTaken(phrase.to_owned()));
            }
        }
        runbooks.push(RunbookConfig {
            name: runbook.name,
            aliases: runbook.aliases,
            cwd: runbook.cwd,
            argv: runbook.argv,
            confirm: runbook.confirm.unwrap_or(true),
            timeout: Duration::from_secs(timeout_secs),
        });
    }
    Ok(runbooks)
}

#[cfg(test)]
mod tests {
    use crate::config::Config;

    use super::*;

    #[test]
    fn parses_runbooks_in_order_with_defaults() {
        assert_eq!(Config::parse("").expect("parses").runbooks, []);
        let config = Config::parse(
            r#"
            [[runbooks]]
            name = "Deploy bridge"
            aliases = ["deploy the bridge"]
            cwd = "/home/me/git/passporter"
            argv = ["deploy/bridge/setup.sh"]
            confirm = true
            timeout_secs = 600

            [[runbooks]]
            name = "Uptime"
            cwd = "/"
            argv = ["uptime", "-p"]
            confirm = false
            "#,
        )
        .expect("parses");
        let names: Vec<&str> = config.runbooks.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["Deploy bridge", "Uptime"]);
        let deploy = &config.runbooks[0];
        assert_eq!(deploy.aliases, ["deploy the bridge"]);
        assert_eq!(deploy.argv, ["deploy/bridge/setup.sh"]);
        assert!(deploy.confirm);
        let uptime = &config.runbooks[1];
        assert!(!uptime.confirm);
        assert_eq!(uptime.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
    }

    #[test]
    fn runbook_values_are_checked() {
        let one = |body: &str| format!("[[runbooks]]\nname = \"R\"\n{body}");
        let cases = [
            (one("cwd = \"rel\"\nargv = [\"x\"]"), "relative cwd"),
            (one("cwd = \"/\"\nargv = []"), "empty argv"),
            (one("cwd = \"/\"\nargv = [\" \"]"), "blank program"),
            (one("cwd = \"/\""), "no argv"),
            (one("argv = [\"x\"]"), "no cwd"),
            (
                one("cwd = \"/\"\nargv = [\"x\"]\ntimeout_secs = 0"),
                "zero timeout",
            ),
            (
                one("cwd = \"/\"\nargv = [\"x\"]\naliases = [\"\"]"),
                "blank alias",
            ),
            (
                one("cwd = \"/\"\nargv = [\"x\"]\nshell = true"),
                "unknown key",
            ),
            (
                one("cwd = \"/\"\nargv = [\"x\"]\nenv = { A = \"1\" }"),
                "no env",
            ),
            (
                "[[runbooks]]\nname = \"a/b\"\ncwd = \"/\"\nargv = [\"x\"]".to_owned(),
                "bad name",
            ),
            (
                format!(
                    "{}\n[[runbooks]]\nname = \"S\"\ncwd = \"/\"\nargv = [\"y\"]\naliases = [\"r\"]",
                    one("cwd = \"/\"\nargv = [\"x\"]")
                ),
                "alias shadows a name",
            ),
            (
                format!(
                    "{}\n[[runbooks]]\nname = \"r!\"\ncwd = \"/\"\nargv = [\"y\"]",
                    one("cwd = \"/\"\nargv = [\"x\"]")
                ),
                "a name that sounds like another",
            ),
            (
                format!(
                    "{}\n[[runbooks]]\nname = \"S\"\ncwd = \"/\"\nargv = [\"y\"]\naliases = [\"um, R please\"]",
                    one("cwd = \"/\"\nargv = [\"x\"]")
                ),
                "an alias that is a name plus filler words",
            ),
            (
                one("cwd = \"/\"\nargv = [\"x\"]\naliases = [\"um, okay\"]"),
                "only filler words",
            ),
            (
                "[[runbooks]]\nname = \"...\"\ncwd = \"/\"\nargv = [\"x\"]".to_owned(),
                "dots only",
            ),
        ];
        for (text, what) in cases {
            assert!(Config::parse(&text).is_err(), "{what}");
        }
    }
}
