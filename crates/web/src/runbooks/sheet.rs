//! The Runbooks sheet's logic, free of the DOM so it is tested on the
//! host: what the page feeds the sheet, the request a confirm makes, when
//! a retry reuses its idempotency key, what each bridge answer means, and
//! how a run reads.

use protocol::{
    IdempotencyKey, Origin, RunState, RunbookName, RunbookResult, RunbookRun, RunbookRunRequest,
    Timestamp,
};
use serde::{Deserialize, Serialize};

use crate::api::ApiError;

/// What the page feeds the sheet ([`crate::extras`]), as JSON: what only
/// the page knows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feed {
    /// The runbook offered for a confirm, if any.
    pub offer: Option<OfferView>,
    /// The latest run of each runbook the stream has reported.
    pub runs: Vec<RunbookRun>,
    /// The stream's resync count: on a change, the sheet fetches the list
    /// again (the stream forgets its runs on a resync).
    pub resyncs: u64,
}

/// A runbook offered for a confirm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferView {
    /// The runbook.
    pub name: RunbookName,
    /// What was said, when speech offered it.
    pub said: Option<String>,
}

/// The last run request, remembered until the bridge decides it, so that
/// running the same runbook again after no answer repeats it, key and
/// all: the bridge then replays instead of running it twice.
#[derive(Debug, Clone, Default)]
pub struct Attempt {
    last: Option<(RunbookName, RunbookRunRequest)>,
}

/// How long an undecided request is repeated (same key) when the same
/// runbook is confirmed again: long enough for retries after "no answer",
/// short enough that a confirm much later asks for a fresh run.
pub const REUSE_KEY_MS: u64 = 2 * 60 * 1000;

impl Attempt {
    /// The request that runs `name`: the last one again if it was for the
    /// same runbook, is undecided and is under [`REUSE_KEY_MS`] old, else a
    /// new one with `new_key` (called only then) at `now`. Always from the app: the car never runs
    /// runbooks. `None` if no key could be made.
    pub fn request(
        &mut self,
        name: &RunbookName,
        new_key: impl FnOnce() -> Option<IdempotencyKey>,
        now: Timestamp,
    ) -> Option<RunbookRunRequest> {
        if let Some((last, request)) = &self.last
            && last == name
            && request.created_at.millis_until(now) < REUSE_KEY_MS
        {
            return Some(request.clone());
        }
        let request = RunbookRunRequest {
            idempotency_key: new_key()?,
            origin: Origin::App,
            created_at: now,
        };
        self.last = Some((name.clone(), request.clone()));
        Some(request)
    }

    /// The bridge decided: the next run is a new request.
    pub fn settle(&mut self) {
        self.last = None;
    }
}

/// What the sheet does with a bridge answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// The run started (or this replays one that did): follow it.
    Follow(RunbookRun),
    /// Nothing decided: show the message; confirming again repeats the
    /// request.
    Retry(String),
    /// Decided, nothing to follow: show the message.
    Show(String),
}

impl Next {
    /// Whether the bridge decided the request (its key is spent).
    #[must_use]
    pub const fn is_final(&self) -> bool {
        !matches!(self, Self::Retry(_))
    }
}

/// Maps `POST /v1/runbooks/:name/run`'s outcome to what the sheet does.
#[must_use]
pub fn next(outcome: Result<RunbookResult, ApiError>) -> Next {
    let or = |message: String, fallback: &str| {
        if message.is_empty() {
            fallback.to_owned()
        } else {
            message
        }
    };
    match outcome {
        Ok(RunbookResult::Started { run, .. }) => Next::Follow(run),
        Ok(RunbookResult::Refused { reason, message }) => {
            let shown = or(message, "The bridge refused to run it.");
            if reason.is_transient() {
                Next::Retry(shown)
            } else {
                Next::Show(shown)
            }
        }
        Ok(RunbookResult::Failed { message }) => {
            Next::Show(or(message, "herdr couldn't start the run."))
        }
        Ok(RunbookResult::Unknown) => Next::Show(
            "Unreadable answer from the bridge; the runbook may be running. Look in herdr."
                .to_owned(),
        ),
        Err(error)
            if error.is_retryable() || matches!(error, ApiError::Status { status: 408 | 504 }) =>
        {
            Next::Retry("No answer from the bridge. Try again: it won't run twice.".to_owned())
        }
        Err(error) => Next::Show(format!("Couldn't run it: {error}")),
    }
}

/// A run's state in a few words, for its status line: the protocol's
/// [`RunbookRun::outcome`] wording, after "Done" or "Failed".
#[must_use]
pub fn status(run: &RunbookRun) -> String {
    match run.state {
        RunState::Running => "Running…".to_owned(),
        RunState::Failed | RunState::Unknown => "Failed".to_owned(),
        _ if run.succeeded() => format!("Done · {}", run.outcome()),
        _ => format!("Failed · {}", run.outcome()),
    }
}

/// Which of two reports of a runbook's latest run to show: the later run;
/// for the same run, a finished report over a running one (a "running"
/// from before a disconnect must never hide how the run ended).
#[must_use]
pub fn fresher(a: Option<RunbookRun>, b: Option<RunbookRun>) -> Option<RunbookRun> {
    match (a, b) {
        (Some(a), Some(b)) => Some(
            if b.started_at > a.started_at || (b.started_at == a.started_at && b.is_finished()) {
                b
            } else {
                a
            },
        ),
        (a, b) => a.or(b),
    }
}

#[cfg(test)]
mod tests {
    use protocol::RunbookRefusal;

    use super::*;

    fn key(n: u8) -> IdempotencyKey {
        IdempotencyKey::new(format!("00000000-0000-4000-8000-0000000000{n:02x}")).unwrap()
    }

    fn name(s: &str) -> RunbookName {
        RunbookName::new(s).unwrap()
    }

    fn run(state: RunState, exit_code: Option<i32>) -> RunbookRun {
        RunbookRun {
            runbook: name("Deploy"),
            state,
            exit_code,
            last_lines: Vec::new(),
            message: None,
            started_at: Timestamp::from_unix_millis(1),
            finished_at: None,
        }
    }

    #[test]
    fn the_feed_crosses_the_module_boundary_intact() {
        let feed = Feed {
            offer: Some(OfferView {
                name: name("Deploy"),
                said: Some("deploy".to_owned()),
            }),
            runs: vec![run(RunState::Exited, Some(0))],
            resyncs: 3,
        };
        let json = serde_json::to_string(&feed).unwrap();
        assert_eq!(serde_json::from_str::<Feed>(&json).unwrap(), feed);
    }

    #[test]
    fn retries_reuse_the_key_until_decided() {
        let mut attempt = Attempt::default();
        let first = attempt
            .request(
                &name("Deploy"),
                || Some(key(1)),
                Timestamp::from_unix_millis(1),
            )
            .unwrap();
        assert_eq!(first.origin, Origin::App, "never the car");
        let again = attempt
            .request(
                &name("Deploy"),
                || panic!("no new key"),
                Timestamp::from_unix_millis(2),
            )
            .unwrap();
        assert_eq!(again, first);
        let other = attempt
            .request(
                &name("Other"),
                || Some(key(2)),
                Timestamp::from_unix_millis(3),
            )
            .unwrap();
        assert_eq!(other.idempotency_key, key(2));
        // Much later, the same runbook is a fresh run.
        let later = attempt
            .request(
                &name("Other"),
                || Some(key(4)),
                Timestamp::from_unix_millis(3 + REUSE_KEY_MS),
            )
            .unwrap();
        assert_eq!(later.idempotency_key, key(4));
        attempt.settle();
        let fresh = attempt
            .request(
                &name("Other"),
                || Some(key(3)),
                Timestamp::from_unix_millis(4),
            )
            .unwrap();
        assert_eq!(fresh.idempotency_key, key(3));
        assert!(
            Attempt::default()
                .request(&name("Deploy"), || None, Timestamp::from_unix_millis(5))
                .is_none()
        );
    }

    #[test]
    fn answers_map_to_follow_retry_or_show() {
        let started = run(RunState::Running, None);
        assert_eq!(
            next(Ok(RunbookResult::Started {
                run: started.clone(),
                replayed: true
            })),
            Next::Follow(started)
        );
        let refused = |reason| {
            next(Ok(RunbookResult::Refused {
                reason,
                message: String::new(),
            }))
        };
        assert!(matches!(
            refused(RunbookRefusal::RateLimited),
            Next::Retry(_)
        ));
        assert!(matches!(refused(RunbookRefusal::Busy), Next::Show(_)));
        assert!(matches!(
            refused(RunbookRefusal::CarForbidden),
            Next::Show(_)
        ));
        let lost = next(Err(ApiError::Transport("gone".into())));
        assert!(matches!(lost, Next::Retry(_)) && !lost.is_final());
        assert!(matches!(
            next(Err(ApiError::Status { status: 504 })),
            Next::Retry(_)
        ));
        assert!(next(Ok(RunbookResult::Unknown)).is_final());
    }

    #[test]
    fn statuses_read_plainly() {
        assert_eq!(status(&run(RunState::Running, None)), "Running…");
        assert_eq!(status(&run(RunState::Exited, Some(0))), "Done · exit 0");
        assert_eq!(status(&run(RunState::Exited, Some(2))), "Failed · exit 2");
        assert_eq!(status(&run(RunState::TimedOut, None)), "Failed · timed out");
        assert_eq!(status(&run(RunState::Failed, None)), "Failed");
    }

    #[test]
    fn a_finished_report_beats_a_stale_running_one() {
        let running = run(RunState::Running, None);
        let done = run(RunState::Exited, Some(0));
        assert_eq!(
            fresher(Some(running.clone()), Some(done.clone())),
            Some(done.clone())
        );
        assert_eq!(
            fresher(Some(done.clone()), Some(running.clone())),
            Some(done.clone())
        );
        let mut later = running.clone();
        later.started_at = Timestamp::from_unix_millis(9);
        assert_eq!(
            fresher(Some(done.clone()), Some(later.clone())),
            Some(later)
        );
        assert_eq!(fresher(None, Some(done.clone())), Some(done));
        assert_eq!(fresher(None, None), None);
    }
}
