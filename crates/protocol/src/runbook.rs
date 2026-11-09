//! Runbooks: named, preconfigured operations the phone may run, by tap or
//! by voice (`GET /v1/runbooks`, `POST /v1/runbooks/:name/run`).
//!
//! A runbook's command line and working directory live only in the bridge
//! config (`[[runbooks]]`). The phone names a runbook and nothing else: the
//! run request has no field that could carry an argument, a path or a
//! flag, and the bridge never takes one from speech. The list the bridge
//! returns does carry each runbook's exact `argv` and `cwd`, so the phone
//! can show precisely what a confirm will run. Wire shape of the request
//! (inbound, strict; the name is in the path):
//!
//! ```json
//! {
//!   "idempotencyKey": "9ca09978-ef89-46fd-9a85-118b84bee7b4",
//!   "origin": "app",
//!   "createdAt": 1762500000000
//! }
//! ```
//!
//! A run's progress ([`RunbookRun`]) arrives on the event stream as
//! `runbook_updated` and in the list's `run` field.
//!
//! # Outbox contract
//!
//! As for spawns ([`crate::spawn`]): no response means retry the same key
//! (the bridge dedupes, so a retry never runs it twice); any 2xx is final
//! for the key; a refusal that [`is_transient`](RunbookRefusal::is_transient)
//! may be retried with the same key, any other is final.

use serde::{Deserialize, Serialize};

use crate::action::Origin;
use crate::error::ValidationError;
use crate::ids::IdempotencyKey;
use crate::macros::validated_string;
use crate::time::Timestamp;

/// Longest [`RunbookName`], in characters.
pub const MAX_RUNBOOK_NAME_CHARS: usize = 48;

/// Most output lines a [`RunbookRun`] carries.
pub const MAX_RUN_LINES: usize = 8;

fn check_runbook_name(value: &str) -> Result<(), ValidationError> {
    const FIELD: &str = "runbook";
    if value.trim().is_empty() {
        return Err(ValidationError::Empty { field: FIELD });
    }
    let chars = value.chars().count();
    if chars > MAX_RUNBOOK_NAME_CHARS {
        return Err(ValidationError::TooLong {
            field: FIELD,
            max: MAX_RUNBOOK_NAME_CHARS,
            actual: chars,
        });
    }
    let padded = value.trim() != value;
    // Only dots (`.`, `..`) would be a relative path segment in the URL.
    let dots = value.chars().all(|c| c == '.');
    if padded || dots || value.chars().any(|c| c.is_control() || c == '/') {
        return Err(ValidationError::InvalidCharacter { field: FIELD });
    }
    Ok(())
}

validated_string!(
    /// A runbook's name as configured, e.g. `Deploy bridge`: 1 to
    /// [`MAX_RUNBOOK_NAME_CHARS`] characters, not only dots, no control
    /// characters, no
    /// `/` (it is a path segment) and no surrounding whitespace.
    RunbookName,
    check_runbook_name
);

/// The body of `POST /v1/runbooks/:name/run`. Inbound: strict. There is
/// deliberately nowhere to put an argument.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunbookRunRequest {
    /// Generated on the device when the user confirms; a retry reuses it.
    pub idempotency_key: IdempotencyKey,
    /// Where the request came from. The bridge refuses `car` outright.
    pub origin: Origin,
    /// When the user confirmed (device clock).
    pub created_at: Timestamp,
}

/// One runbook, as `GET /v1/runbooks` lists it. Outbound: tolerant (see
/// [`RunbookList`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Runbook {
    /// What the phone shows, sends and matches speech against.
    pub name: RunbookName,
    /// Other phrases that offer it when dictated.
    pub aliases: Vec<String>,
    /// The exact command the bridge runs, executed directly (no shell).
    pub argv: Vec<String>,
    /// The directory it runs in.
    pub cwd: String,
    /// Whether a tap asks before running (default on). Speech always asks.
    pub confirm: bool,
    /// The latest run since the bridge started, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<RunbookRun>,
}

/// The response of `GET /v1/runbooks`. Outbound: tolerant: runbooks whose
/// name this app cannot parse are skipped, missing fields default, and a
/// run it cannot parse is left out.
///
/// Every type here decodes through flat wire structs of plain strings
/// rather than serde's tagged and untagged enums: the same tolerance, at a
/// fraction of the WASM size (the app's budget).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ListWire")]
pub struct RunbookList {
    /// Every configured runbook, in config order; empty when none are.
    pub runbooks: Vec<Runbook>,
}

#[derive(Deserialize)]
struct ListWire {
    #[serde(default)]
    runbooks: Vec<RunbookWire>,
}

#[derive(Deserialize)]
struct RunbookWire {
    #[serde(default)]
    name: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    argv: Vec<String>,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    confirm: Option<bool>,
    #[serde(default)]
    run: Option<RunWire>,
}

impl From<ListWire> for RunbookList {
    fn from(wire: ListWire) -> Self {
        let runbooks = wire
            .runbooks
            .into_iter()
            .filter_map(|r| {
                Some(Runbook {
                    name: RunbookName::new(r.name).ok()?,
                    aliases: r.aliases,
                    argv: r.argv,
                    cwd: r.cwd,
                    confirm: r.confirm.unwrap_or(true),
                    run: r.run.and_then(RunWire::into_run),
                })
            })
            .collect();
        Self { runbooks }
    }
}

/// Where a run is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// Running in its herdr tab.
    Running,
    /// Finished; see [`RunbookRun::exit_code`].
    Exited,
    /// Killed after the runbook's `timeout_secs`.
    TimedOut,
    /// Never started, or the bridge lost track of it; see
    /// [`RunbookRun::message`].
    Failed,
    /// A state this app predates.
    Unknown,
}

impl RunState {
    /// A wire name back to its state, [`Self::Unknown`] for one this app
    /// predates.
    #[must_use]
    pub fn from_wire(name: &str) -> Self {
        match name {
            "running" => Self::Running,
            "exited" => Self::Exited,
            "timed_out" => Self::TimedOut,
            "failed" => Self::Failed,
            _ => Self::Unknown,
        }
    }
}

/// One run of a runbook. Outbound: tolerant (an unknown state is
/// [`RunState::Unknown`]); a run without a valid runbook name or start
/// time does not parse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "RunWire")]
pub struct RunbookRun {
    /// Which runbook.
    pub runbook: RunbookName,
    /// Where it is.
    pub state: RunState,
    /// The exit code once [`RunState::Exited`]; `None` when a signal ended
    /// it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The last lines of its herdr pane (at most [`MAX_RUN_LINES`]):
    /// untrusted output, shown as plain text.
    pub last_lines: Vec<String>,
    /// Why it failed, for [`RunState::Failed`] and [`RunState::TimedOut`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// When the bridge started it.
    pub started_at: Timestamp,
    /// When it ended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<Timestamp>,
}

impl RunbookRun {
    /// Whether it is over (anything but running).
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.state != RunState::Running
    }

    /// Whether it exited with code 0.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.state == RunState::Exited && self.exit_code == Some(0)
    }

    /// How it ended, in a few words ([`outcome`]).
    #[must_use]
    pub fn outcome(&self) -> String {
        outcome(self.state, self.exit_code)
    }
}

/// How a run in `state` with `exit_code` ended, in a few words: `exit 0`,
/// `exit 3`, `killed by a signal`, `timed out`, `failed` (or `running`).
/// One wording for the pane's last line, the audit log and the phone.
#[must_use]
pub fn outcome(state: RunState, exit_code: Option<i32>) -> String {
    match (state, exit_code) {
        (RunState::Running, _) => "running".to_owned(),
        (RunState::Exited, Some(code)) => format!("exit {code}"),
        (RunState::Exited, None) => "killed by a signal".to_owned(),
        (RunState::TimedOut, _) => "timed out".to_owned(),
        (RunState::Failed | RunState::Unknown, _) => "failed".to_owned(),
    }
}

/// `argv` as people read it, the way a POSIX shell would take it: each
/// element separated by a space, and any element that is empty or holds
/// anything but letters, digits and `/._-+:@%,=` single-quoted (a `'`
/// inside becomes `'\''`). The phone's confirm and the pane's header both
/// show this, so they always agree.
#[must_use]
pub fn command_line(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            let plain = !arg.is_empty()
                && arg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "/._-+:@%,=".contains(c));
            if plain {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Words dropped before matching speech: hesitations and politeness that
/// dictation keeps but that never change which runbook is meant.
pub(crate) const FILLERS: [&str; 15] = [
    "um", "umm", "uh", "uhh", "er", "erm", "ah", "eh", "hmm", "hm", "mm", "please", "okay", "ok",
    "so",
];

/// A phrase normalised for matching speech against runbook names and
/// aliases: lower case, apostrophes removed (so "don't" is one word), any
/// other non-alphanumeric character a space, filler words ("um", "uh",
/// "please", "okay", "so", …) dropped, words joined by single spaces.
/// The phone matches with it, and the bridge refuses a config in which
/// two phrases normalise alike or one normalises to nothing.
#[must_use]
pub fn normalise_phrase(text: &str) -> String {
    let spaced: String = text
        .chars()
        .filter(|c| !matches!(c, '\'' | '’'))
        .flat_map(char::to_lowercase)
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    spaced
        .split_whitespace()
        .filter(|word| !FILLERS.contains(word))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The flat shape a run decodes through; never fails on a missing field.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunWire {
    #[serde(default)]
    runbook: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    last_lines: Vec<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    started_at: Option<Timestamp>,
    #[serde(default)]
    finished_at: Option<Timestamp>,
}

impl RunWire {
    /// The run, if it names a valid runbook and a start time.
    pub(crate) fn into_run(self) -> Option<RunbookRun> {
        RunbookRun::try_from(self).ok()
    }
}

impl TryFrom<RunWire> for RunbookRun {
    type Error = ValidationError;

    fn try_from(wire: RunWire) -> Result<Self, ValidationError> {
        Ok(Self {
            runbook: RunbookName::new(wire.runbook)?,
            state: RunState::from_wire(&wire.state),
            exit_code: wire.exit_code,
            last_lines: wire.last_lines,
            message: wire.message,
            started_at: wire
                .started_at
                .ok_or(ValidationError::Empty { field: "startedAt" })?,
            finished_at: wire.finished_at,
        })
    }
}

/// What happened to a [`RunbookRunRequest`], tagged by `"status"`.
/// Outbound: tolerant: a status this app predates, or a `started` whose
/// run it cannot parse, is [`RunbookResult::Unknown`]; a reason it
/// predates is [`RunbookRefusal::Other`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    from = "ResultWire"
)]
pub enum RunbookResult {
    /// The run started; follow it on the stream.
    Started {
        /// The run as it started.
        run: RunbookRun,
        /// True when this is the stored result of an earlier identical
        /// request: nothing was started this time.
        replayed: bool,
    },
    /// The bridge refused. Nothing ran.
    Refused {
        /// Machine-readable reason.
        reason: RunbookRefusal,
        /// Human-readable text.
        message: String,
    },
    /// herdr failed after the bridge decided to run it. Final for this
    /// key: look in herdr before trying again.
    Failed {
        /// Human-readable error.
        message: String,
    },
    /// A status this app predates; treat as failed.
    Unknown,
}

#[derive(Deserialize)]
struct ResultWire {
    #[serde(default)]
    status: String,
    #[serde(default)]
    run: Option<RunWire>,
    #[serde(default)]
    replayed: bool,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

impl From<ResultWire> for RunbookResult {
    fn from(wire: ResultWire) -> Self {
        let message = wire.message.unwrap_or_default();
        match wire.status.as_str() {
            "started" => match wire.run.and_then(RunWire::into_run) {
                Some(run) => Self::Started {
                    run,
                    replayed: wire.replayed,
                },
                None => Self::Unknown,
            },
            "refused" => Self::Refused {
                reason: wire
                    .reason
                    .as_deref()
                    .map_or(RunbookRefusal::Other, RunbookRefusal::from_wire),
                message,
            },
            "failed" => Self::Failed { message },
            _ => Self::Unknown,
        }
    }
}

/// Why a run was [`RunbookResult::Refused`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunbookRefusal {
    /// No runbook by that name. Permanent.
    UnknownRunbook,
    /// That runbook is running already: one run at a time. Permanent for
    /// this key; ask again once it ends.
    Busy,
    /// Runbooks never run from the car. Permanent.
    CarForbidden,
    /// Per-device rate limit hit. Transient: retry the same key later.
    RateLimited,
    /// The idempotency key was already used for a different request.
    IdempotencyConflict,
    /// The body failed validation (e.g. it tried to pass arguments).
    /// Permanent.
    InvalidRequest,
    /// A reason this app predates. Treated as permanent.
    Other,
}

impl RunbookRefusal {
    /// The reason's wire name, as serde writes it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnknownRunbook => "unknown_runbook",
            Self::Busy => "busy",
            Self::CarForbidden => "car_forbidden",
            Self::RateLimited => "rate_limited",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::InvalidRequest => "invalid_request",
            Self::Other => "other",
        }
    }

    /// A wire name back to its reason, [`Self::Other`] for one this app
    /// predates.
    #[must_use]
    pub fn from_wire(name: &str) -> Self {
        [
            Self::UnknownRunbook,
            Self::Busy,
            Self::CarForbidden,
            Self::RateLimited,
            Self::IdempotencyConflict,
            Self::InvalidRequest,
        ]
        .into_iter()
        .find(|reason| reason.as_str() == name)
        .unwrap_or(Self::Other)
    }

    /// Whether the same idempotency key may be retried: the bridge does not
    /// record transient refusals against the key.
    #[must_use]
    pub const fn is_transient(self) -> bool {
        matches!(self, Self::RateLimited)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "9ca09978-ef89-46fd-9a85-118b84bee7b4";

    #[test]
    fn names_are_short_single_segments() {
        for ok in ["Deploy bridge", "d", "Restart whisper (CUDA)", "Ünïcode ✓"] {
            assert!(RunbookName::new(ok).is_ok(), "{ok}");
        }
        let long = "x".repeat(MAX_RUNBOOK_NAME_CHARS + 1);
        for bad in [
            "",
            "  ",
            " pad",
            "pad ",
            "a/b",
            "../x",
            "tab\there",
            ".",
            "..",
            "...",
            &long,
        ] {
            assert!(RunbookName::new(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn run_requests_cannot_carry_arguments() {
        let ok = format!(r#"{{"idempotencyKey":"{KEY}","origin":"app","createdAt":1}}"#);
        let parsed: RunbookRunRequest = serde_json::from_str(&ok).unwrap();
        assert_eq!(parsed.origin, Origin::App);
        for extra in [
            r#","argv":["rm","-rf","/"]"#,
            r#","args":["--force"]"#,
            r#","cwd":"/""#,
            r#","env":{"A":"1"}"#,
            r#","name":"Other""#,
        ] {
            let json =
                format!(r#"{{"idempotencyKey":"{KEY}","origin":"app","createdAt":1{extra}}}"#);
            assert!(
                serde_json::from_str::<RunbookRunRequest>(&json).is_err(),
                "{extra}"
            );
        }
        let no_origin = format!(r#"{{"idempotencyKey":"{KEY}","createdAt":1}}"#);
        assert!(serde_json::from_str::<RunbookRunRequest>(&no_origin).is_err());
    }

    #[test]
    fn lists_skip_what_this_app_cannot_parse() {
        let list: RunbookList = serde_json::from_str(
            r#"{"runbooks":[{"name":"Deploy","argv":["x"],"cwd":"/r"},{"name":"a/b"},
                {"name":"Up","run":{"runbook":"Up","state":"melting","startedAt":1}}]}"#,
        )
        .unwrap();
        let names: Vec<&str> = list.runbooks.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["Deploy", "Up"]);
        assert!(list.runbooks[0].confirm, "confirm defaults to on");
        let run = list.runbooks[1].run.as_ref().unwrap();
        assert_eq!(run.state, RunState::Unknown);
        assert!(run.is_finished());
        assert_eq!(
            serde_json::from_str::<RunbookList>("{}").unwrap(),
            RunbookList::default()
        );
    }

    #[test]
    fn results_are_tolerant() {
        assert_eq!(
            serde_json::from_str::<RunbookResult>(r#"{"status":"queued"}"#).unwrap(),
            RunbookResult::Unknown
        );
        assert_eq!(
            serde_json::from_str::<RunbookResult>(r#"{"status":"refused","reason":"moon"}"#)
                .unwrap(),
            RunbookResult::Refused {
                reason: RunbookRefusal::Other,
                message: String::new()
            }
        );
        let started: RunbookResult = serde_json::from_str(
            r#"{"status":"started","run":{"runbook":"Deploy","state":"running",
                "lastLines":[],"startedAt":5}}"#,
        )
        .unwrap();
        let RunbookResult::Started { run, replayed } = started else {
            panic!("wrong variant");
        };
        assert!(!replayed && !run.is_finished() && !run.succeeded());
    }

    #[test]
    fn wire_names_match_serde() {
        for reason in [
            RunbookRefusal::UnknownRunbook,
            RunbookRefusal::Busy,
            RunbookRefusal::CarForbidden,
            RunbookRefusal::RateLimited,
            RunbookRefusal::IdempotencyConflict,
            RunbookRefusal::InvalidRequest,
            RunbookRefusal::Other,
        ] {
            assert_eq!(serde_json::to_value(reason).unwrap(), reason.as_str());
            assert_eq!(RunbookRefusal::from_wire(reason.as_str()), reason);
            assert_eq!(reason.is_transient(), reason == RunbookRefusal::RateLimited);
        }
    }

    #[test]
    fn phrases_normalise_case_punctuation_and_fillers() {
        assert_eq!(
            normalise_phrase("  Deploy   the BRIDGE. "),
            "deploy the bridge"
        );
        assert_eq!(
            normalise_phrase("Um, deploy the bridge, please!"),
            "deploy the bridge"
        );
        assert_eq!(normalise_phrase("uh... okay so ship-it?"), "ship it");
        assert_eq!(normalise_phrase("Don't stop"), "dont stop");
        assert_eq!(normalise_phrase("Don’t stop"), "dont stop");
        assert_eq!(normalise_phrase("Ünïcode ÉCOLE"), "ünïcode école");
        assert_eq!(normalise_phrase("um uh please"), "");
    }

    #[test]
    fn command_lines_show_every_element_unambiguously() {
        let argv = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            command_line(&argv(&["deploy/bridge/setup.sh"])),
            "deploy/bridge/setup.sh"
        );
        assert_eq!(
            command_line(&argv(&["git", "commit", "-m", "a b", "", "it's", "$(x)"])),
            r"git commit -m 'a b' '' 'it'\''s' '$(x)'"
        );
    }

    #[test]
    fn outcomes_read_the_same_everywhere() {
        assert_eq!(outcome(RunState::Exited, Some(0)), "exit 0");
        assert_eq!(outcome(RunState::Exited, Some(3)), "exit 3");
        assert_eq!(outcome(RunState::Exited, None), "killed by a signal");
        assert_eq!(outcome(RunState::TimedOut, None), "timed out");
        assert_eq!(outcome(RunState::Failed, None), "failed");
        assert_eq!(outcome(RunState::Running, None), "running");
    }

    #[test]
    fn success_is_exit_zero() {
        let mut run = RunbookRun {
            runbook: RunbookName::new("Deploy").unwrap(),
            state: RunState::Exited,
            exit_code: Some(0),
            last_lines: Vec::new(),
            message: None,
            started_at: Timestamp::from_unix_millis(1),
            finished_at: Some(Timestamp::from_unix_millis(2)),
        };
        assert!(run.succeeded());
        run.exit_code = Some(1);
        assert!(!run.succeeded() && run.is_finished());
    }
}
