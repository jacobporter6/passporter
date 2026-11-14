//! The "New agent" sheet's logic, free of the DOM so it is tested on the
//! host: what is picked, the request it makes, when a retry reuses the
//! idempotency key, and what each bridge answer means for the sheet.

use core::fmt;

use protocol::{
    AgentId, IdempotencyKey, ProjectName, PromptText, SpawnKind, SpawnOptions, SpawnRequest,
    SpawnResult, Timestamp, ValidationError,
};

use crate::api::ApiError;

/// What the user has picked so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Form {
    /// The project, from the bridge's allowlist.
    pub project: Option<ProjectName>,
    /// The agent kind.
    pub kind: Option<SpawnKind>,
    /// The first prompt, as typed or dictated; blank means none.
    pub prompt: String,
}

impl Form {
    /// A fresh form for `options`: a lone project is picked already (one
    /// tap fewer), and so is the first kind the bridge allows.
    #[must_use]
    pub fn for_options(options: &SpawnOptions) -> Self {
        Self {
            project: match options.projects.as_slice() {
                [only] => Some(only.clone()),
                _ => None,
            },
            kind: options.kinds.first().copied(),
            prompt: String::new(),
        }
    }

    /// The first prompt, `None` when blank.
    fn prompt(&self) -> Result<Option<PromptText>, FormError> {
        let text = self.prompt.trim();
        if text.is_empty() {
            return Ok(None);
        }
        PromptText::new(text).map(Some).map_err(|e| match e {
            ValidationError::TooLong { .. } => FormError::TooLong,
            other => FormError::Prompt(other.to_string()),
        })
    }
}

/// Why the form cannot be sent yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormError {
    /// No project picked.
    NoProject,
    /// No kind picked (the bridge allows none).
    NoKind,
    /// The prompt failed validation.
    Prompt(String),
    /// The prompt is over 16 KiB (`protocol::text::MAX_PROMPT_BYTES`, UTF-8 bytes, so fewer
    /// characters when they are accented, non-Latin or emoji).
    TooLong,
    /// The browser could not make a random idempotency key.
    NoRandom,
}

impl fmt::Display for FormError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoProject => f.write_str("Pick a project."),
            Self::NoKind => f.write_str("The bridge allows no agent kinds."),
            Self::Prompt(why) => write!(f, "Can't send this prompt: {why}"),
            Self::TooLong => f.write_str(
                "The prompt is over 16 KiB (accents and emoji take more room); shorten it.",
            ),
            Self::NoRandom => f.write_str("This browser can't make a request id."),
        }
    }
}

/// The request the sheet sends, remembered so that sending the same form
/// again after no answer (or a transient refusal) repeats it byte for
/// byte, idempotency key and all: the bridge then replays instead of
/// starting a second agent.
#[derive(Debug, Clone, Default)]
pub struct Attempt {
    last: Option<SpawnRequest>,
}

impl Attempt {
    /// The request for `form`: the last one again if the form is
    /// unchanged, else a new one with `new_key` (called only then) at `now`.
    pub fn request(
        &mut self,
        form: &Form,
        new_key: impl FnOnce() -> Option<IdempotencyKey>,
        now: Timestamp,
    ) -> Result<SpawnRequest, FormError> {
        let project = form.project.clone().ok_or(FormError::NoProject)?;
        let kind = form.kind.ok_or(FormError::NoKind)?;
        let prompt = form.prompt()?;
        if let Some(last) = &self.last
            && last.project == project
            && last.kind == kind
            && last.prompt == prompt
        {
            return Ok(last.clone());
        }
        let idempotency_key = new_key().ok_or(FormError::NoRandom)?;
        let request = SpawnRequest {
            idempotency_key,
            project,
            kind,
            prompt,
            created_at: now,
        };
        self.last = Some(request.clone());
        Ok(request)
    }

    /// The bridge decided: the next send is a new request.
    pub fn settle(&mut self) {
        self.last = None;
    }

    /// The form of a request the bridge has not decided yet, if any: what
    /// a reopened sheet shows, so sending it again repeats that request
    /// (same key) instead of making a new one.
    #[must_use]
    pub fn pending_form(&self) -> Option<Form> {
        self.last.as_ref().map(|last| Form {
            project: Some(last.project.clone()),
            kind: Some(last.kind),
            prompt: last
                .prompt
                .as_ref()
                .map(|p| p.as_str().to_owned())
                .unwrap_or_default(),
        })
    }
}

/// The new agent to open, including a first prompt the bridge could not send.
/// Crosses the extras module boundary as JSON before navigation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Opened {
    /// The new agent.
    pub agent: AgentId,
    /// What to tell the user about delivery.
    pub note: Option<String>,
    /// Text to keep in the composer for explicit sending after the dialog.
    pub draft: Option<PromptText>,
}

/// What the sheet does with a bridge answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// The agent is running: open it, with `note` as a toast if set (the
    /// first prompt was not sent, or this was a replay).
    Open(Opened),
    /// Nothing decided: show `message`; sending again repeats the request.
    Retry(String),
    /// Decided without an agent to open: show `message`; the next send is
    /// a new request.
    Show(String),
}

impl Next {
    /// Whether the bridge decided the request (its key is spent).
    #[must_use]
    pub const fn is_final(&self) -> bool {
        !matches!(self, Self::Retry(_))
    }
}

/// Maps `POST /v1/agents`'s outcome to what the sheet does.
#[must_use]
pub fn next(outcome: Result<SpawnResult, ApiError>, prompt: Option<PromptText>) -> Next {
    match outcome {
        Ok(SpawnResult::Spawned {
            agent_id,
            prompt_error,
            ..
        }) => Next::Open(Opened {
            agent: agent_id,
            draft: prompt_error.as_ref().and(prompt),
            note: prompt_error
                .map(|why| "Agent started. Prompt kept in the composer: ".to_owned() + &why),
        }),
        Ok(SpawnResult::Refused { reason, message }) => {
            // The bridge always explains; a blank one gets a generic line.
            let shown = if message.is_empty() {
                "The bridge refused to start the agent.".to_owned()
            } else {
                message
            };
            if reason.is_transient() {
                Next::Retry(shown)
            } else {
                Next::Show(shown)
            }
        }
        Ok(SpawnResult::Failed { message }) => Next::Show(if message.is_empty() {
            "herdr couldn't start the agent.".to_owned()
        } else {
            message
        }),
        Ok(SpawnResult::Unknown) => Next::Show(
            "Unreadable answer from the bridge; the agent may be running. Reload to see."
                .to_owned(),
        ),
        // No answer, or a timeout in front of the handler (408, 504) that
        // says nothing about the spawn: it may be under way, so only the
        // same key is safe.
        Err(error)
            if error.is_retryable()
                || matches!(
                    error,
                    ApiError::Status {
                        status: REQUEST_TIMEOUT | GATEWAY_TIMEOUT
                    }
                ) =>
        {
            Next::Retry(
                "No answer from the bridge. Try again: it won't start a second agent.".to_owned(),
            )
        }
        // Unauthorized says to pair again, in its own words.
        Err(error) => Next::Show(format!("Couldn't start the agent: {error}")),
    }
}

/// HTTP 408: a timeout in front of the handler.
const REQUEST_TIMEOUT: u16 = 408;
/// HTTP 504: the edge gave up waiting for the bridge.
const GATEWAY_TIMEOUT: u16 = 504;

#[cfg(test)]
mod tests {
    use protocol::SpawnRefusal;

    use super::*;

    fn next(outcome: Result<SpawnResult, ApiError>) -> Next {
        super::next(outcome, None)
    }

    #[test]
    fn a_trust_dialog_keeps_the_first_prompt_across_the_module_boundary() {
        for replayed in [false, true] {
            for prompt_error in [None, Some("Answer the folder trust dialog first.")] {
                let prompt =
                    PromptText::new("Fix the scrolling controls.\nCover it with tests.").unwrap();
                let Next::Open(opened) = super::next(
                    Ok(SpawnResult::Spawned {
                        agent_id: "w2:p1".parse().unwrap(),
                        replayed,
                        prompt_error: prompt_error.map(str::to_owned),
                    }),
                    Some(prompt.clone()),
                ) else {
                    panic!("opens");
                };
                assert_eq!(opened.draft, prompt_error.map(|_| prompt));
                let encoded = serde_json::to_string(&opened).unwrap();
                assert_eq!(serde_json::from_str::<Opened>(&encoded).unwrap(), opened);
            }
        }
    }

    fn name(s: &str) -> ProjectName {
        ProjectName::new(s).unwrap()
    }

    fn key(n: u8) -> IdempotencyKey {
        IdempotencyKey::new(format!("00000000-0000-4000-8000-0000000000{n:02x}")).unwrap()
    }

    fn options(projects: &[&str], kinds: &[SpawnKind]) -> SpawnOptions {
        SpawnOptions {
            projects: projects.iter().map(|p| name(p)).collect(),
            kinds: kinds.to_vec(),
        }
    }

    fn form(project: &str, prompt: &str) -> Form {
        Form {
            project: Some(name(project)),
            kind: Some(SpawnKind::Claude),
            prompt: prompt.to_owned(),
        }
    }

    const T0: Timestamp = Timestamp::from_unix_millis(1_000);
    const T1: Timestamp = Timestamp::from_unix_millis(2_000);

    #[test]
    fn a_lone_project_and_the_first_kind_are_picked_for_you() {
        let lone = Form::for_options(&options(&["web"], &SpawnKind::ALL));
        assert_eq!(lone.project, Some(name("web")));
        assert_eq!(lone.kind, Some(SpawnKind::Claude));
        let many = Form::for_options(&options(&["web", "api"], &[SpawnKind::Codex]));
        assert_eq!(many.project, None, "the user picks among several");
        assert_eq!(many.kind, Some(SpawnKind::Codex));
        assert_eq!(Form::for_options(&SpawnOptions::default()).kind, None);
    }

    #[test]
    fn the_form_needs_a_project_and_a_kind() {
        let mut attempt = Attempt::default();
        let none = Form::default();
        assert_eq!(
            attempt.request(&none, || Some(key(1)), T0),
            Err(FormError::NoProject)
        );
        let no_kind = Form {
            kind: None,
            ..form("web", "")
        };
        assert_eq!(
            attempt.request(&no_kind, || Some(key(1)), T0),
            Err(FormError::NoKind)
        );
        let bad = form("web", "hi\u{202E}there");
        assert!(matches!(
            attempt.request(&bad, || Some(key(1)), T0),
            Err(FormError::Prompt(_))
        ));
    }

    #[test]
    fn a_blank_prompt_is_no_prompt_and_text_is_trimmed() {
        let mut attempt = Attempt::default();
        let blank = attempt
            .request(&form("web", "  \n"), || Some(key(1)), T0)
            .unwrap();
        assert_eq!(blank.prompt, None);
        attempt.settle();
        let typed = attempt
            .request(&form("web", "  fix it \n"), || Some(key(2)), T0)
            .unwrap();
        assert_eq!(typed.prompt.map(String::from), Some("fix it".to_owned()));
    }

    #[test]
    fn sending_the_same_form_again_repeats_the_request() {
        let mut attempt = Attempt::default();
        let first = attempt
            .request(&form("web", "go"), || Some(key(1)), T0)
            .unwrap();
        // No answer: the retry is byte for byte the same, key and time.
        let retry = attempt
            .request(&form("web", "go"), || panic!("no new key for a retry"), T1)
            .unwrap();
        assert_eq!(retry, first);
        // An edit is a new request.
        let edited = attempt
            .request(&form("web", "go now"), || Some(key(2)), T1)
            .unwrap();
        assert_ne!(edited.idempotency_key, first.idempotency_key);
        assert_eq!(edited.created_at, T1);
        // Once the bridge decided, the same form is a new request too.
        attempt.settle();
        let again = attempt
            .request(&form("web", "go now"), || Some(key(3)), T1)
            .unwrap();
        assert_ne!(again.idempotency_key, edited.idempotency_key);
    }

    #[test]
    fn spawned_opens_the_agent_with_any_prompt_problem_as_a_note() {
        let spawned = |prompt_error: Option<&str>| {
            Ok(SpawnResult::Spawned {
                agent_id: "w2:p1".parse().unwrap(),
                replayed: false,
                prompt_error: prompt_error.map(str::to_owned),
            })
        };
        assert_eq!(
            next(spawned(None)),
            Next::Open(Opened {
                agent: "w2:p1".parse().unwrap(),
                note: None,
                draft: None,
            })
        );
        let Next::Open(Opened {
            note: Some(note), ..
        }) = next(spawned(Some("it asked first")))
        else {
            panic!("opens");
        };
        assert!(note.contains("it asked first"), "{note}");
    }

    #[test]
    fn refusals_retry_only_when_transient() {
        let refused = |reason, message: &str| {
            next(Ok(SpawnResult::Refused {
                reason,
                message: message.to_owned(),
            }))
        };
        assert_eq!(
            refused(SpawnRefusal::RateLimited, "slow down"),
            Next::Retry("slow down".to_owned())
        );
        let unknown = refused(SpawnRefusal::UnknownProject, "");
        assert!(unknown.is_final());
        assert_eq!(
            unknown,
            Next::Show("The bridge refused to start the agent.".to_owned())
        );
    }

    #[test]
    fn timeouts_in_front_of_the_bridge_keep_the_key() {
        for status in [408, 504] {
            let next = next(Err(ApiError::Status { status }));
            assert!(matches!(next, Next::Retry(_)), "{status}: {next:?}");
        }
        assert!(next(Err(ApiError::Status { status: 404 })).is_final());
    }

    #[test]
    fn a_reopened_sheet_shows_the_undecided_request() {
        let mut attempt = Attempt::default();
        assert_eq!(attempt.pending_form(), None);
        let sent = attempt
            .request(&form("web", " go "), || Some(key(1)), T0)
            .unwrap();
        let shown = attempt.pending_form().expect("undecided");
        assert_eq!(shown, form("web", "go"));
        // Sending what it shows repeats the request, key and all.
        let again = attempt
            .request(&shown, || panic!("no new key"), T1)
            .unwrap();
        assert_eq!(again, sent);
        attempt.settle();
        assert_eq!(attempt.pending_form(), None);
    }

    #[test]
    fn an_overlong_prompt_says_the_limit_is_in_bytes() {
        let mut attempt = Attempt::default();
        // Under 16,000 characters, over 16 KiB of UTF-8.
        let long = "é".repeat(9_000);
        let err = attempt
            .request(&form("web", &long), || Some(key(1)), T0)
            .unwrap_err();
        assert_eq!(err, FormError::TooLong);
        assert!(err.to_string().contains("16 KiB"), "{err}");
    }

    #[test]
    fn no_answer_retries_and_other_errors_are_final() {
        let offline = next(Err(ApiError::Transport("offline".into())));
        assert!(matches!(offline, Next::Retry(_)));
        assert!(!offline.is_final());
        assert!(matches!(
            next(Err(ApiError::Status { status: 503 })),
            Next::Retry(_)
        ));
        assert!(next(Err(ApiError::Unauthorized)).is_final());
        assert!(next(Ok(SpawnResult::Unknown)).is_final());
        let failed = next(Ok(SpawnResult::Failed {
            message: "herdr: timed out".into(),
        }));
        assert_eq!(failed, Next::Show("herdr: timed out".to_owned()));
    }
}
