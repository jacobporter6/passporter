//! Reply suggestions (`GET /v1/agents/:id/suggestions`, design §6.2):
//! short replies to what the agent asks, shown as chips in the composer.
//!
//! A tap sends the suggestion as a prompt at once, like dictation (see
//! [`Tap`]); the user corrects a wrong tap with a follow-up. The bridge
//! already validates them; they are checked again here ([`sanitize`])
//! because they are model output about untrusted agent output.
//!
//! They are for a question at the end of a turn
//! (`GET /v1/agents/:id/suggestions`), read when the agent is idle or
//! done, again on every status or seq change, and polled while the bridge
//! generates. A blocked agent gets none: herdr refuses prompts while an
//! agent is blocked, so its dialog is answered with options or keys.

use leptos::prelude::*;
use leptos::task::spawn_local;
use protocol::{AgentId, AgentStatus, MAX_SUGGESTIONS, PromptText, Seq};

use crate::api::{ApiError, SuggestionsRead};
use crate::session;
use crate::ui::AppState;
use crate::voice::phase::append_transcript;

/// How long to wait before asking again while the bridge generates, at
/// first.
const POLL: core::time::Duration = core::time::Duration::from_secs(3);

/// How many quick asks before slowing down.
const QUICK_POLLS: u32 = 20;

/// How long to wait between asks after that, and after a failed ask: the
/// job may be queued behind TL;DRs, or the network flaky, and the chips
/// must still turn up once the bridge has them.
const SLOW_POLL: core::time::Duration = core::time::Duration::from_secs(15);

/// When to ask again after the `polls`-th ask got `read`: while the
/// bridge is not done yet (`202`: generating, or holding back after a
/// failed try or the usage limit until a later ask retries) or the ask
/// failed for a reason worth retrying, quickly at first, then slowly, for
/// as long as the agent stays as it is (the caller stops on any change);
/// never once the answer is in (`200`), or the ask cannot succeed (e.g.
/// the token was rejected).
#[must_use]
pub fn next_poll(
    polls: u32,
    read: &Result<SuggestionsRead, ApiError>,
) -> Option<core::time::Duration> {
    let quick = if polls < QUICK_POLLS { POLL } else { SLOW_POLL };
    match read {
        Ok(SuggestionsRead::Generating) => Some(quick),
        Err(error) if error.is_retryable() => Some(SLOW_POLL),
        Ok(SuggestionsRead::Ready(_)) | Err(_) => None,
    }
}

/// The suggestions worth showing from what the bridge sent: those passing
/// the bridge's own rule ([`protocol::suggestion`]: single-line, short,
/// valid text, never elevated), without duplicates, as the prompts a tap
/// sends; at most [`MAX_SUGGESTIONS`].
#[must_use]
pub fn sanitize(raw: Vec<String>) -> Vec<PromptText> {
    let mut out: Vec<PromptText> = Vec::new();
    for candidate in raw {
        let Some(text) = protocol::suggestion(&candidate)
            .ok()
            .and_then(|text| PromptText::new(String::from(text)).ok())
        else {
            continue;
        };
        if out
            .iter()
            .any(|s| s.as_str().eq_ignore_ascii_case(text.as_str()))
        {
            continue;
        }
        out.push(text);
        if out.len() == MAX_SUGGESTIONS {
            break;
        }
    }
    out
}

/// What a tap on a suggested reply does. A suggestion is a whole reply, so
/// it is sent on its own: a draft typed in the composer is left as it is
/// (sending the two together would rarely read as one message). One reply
/// per question: once a suggestion from the shown set was sent, the other
/// chips (and the same one again) do nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tap {
    /// Send it as a prompt now.
    Send(PromptText),
    /// Not sent: the agent waits at a dialog, and the bridge refuses a
    /// prompt then (as for dictation). The composer's text becomes this:
    /// the suggestion after what is typed, once however often it is tapped.
    AtDialog(String),
    /// A suggestion from this set was already sent (say, a double tap).
    Spent,
}

impl Tap {
    /// What tapping `suggestion` does, with the composer holding `draft`;
    /// `at_dialog` while the agent waits at a dialog, `spent` once one of
    /// the shown suggestions was sent.
    #[must_use]
    pub fn of(draft: &str, suggestion: &PromptText, at_dialog: bool, spent: bool) -> Self {
        let text = suggestion.as_str();
        if spent {
            Self::Spent
        } else if !at_dialog {
            Self::Send(suggestion.clone())
        } else if draft.trim_end().ends_with(text) {
            Self::AtDialog(draft.to_owned())
        } else {
            Self::AtDialog(append_transcript(draft, text))
        }
    }
}

/// Whether the agent may have ended its turn on a question: it is idle or
/// done.
#[must_use]
pub const fn may_ask(status: Option<AgentStatus>) -> bool {
    matches!(status, Some(AgentStatus::Idle | AgentStatus::Done))
}

/// The end-of-turn suggestions for agent `id`, kept current for this
/// component's lifetime. Cleared whenever the agent's status or seq
/// changes, then read again if it is idle or done.
pub fn suggestions_feed(
    app: AppState,
    id: AgentId,
    status: Signal<Option<AgentStatus>>,
    listed_seq: Signal<Option<Seq>>,
) -> RwSignal<Vec<PromptText>> {
    let shown = RwSignal::new(Vec::<PromptText>::new());
    // Bumped on every change, so answers about an older state are dropped.
    let round = StoredValue::new(0_u64);
    Effect::new(move |_| {
        let status = status.get();
        let seq = listed_seq.get();
        let current = round
            .try_update_value(|r| {
                *r = r.wrapping_add(1);
                *r
            })
            .unwrap_or_default();
        shown.set(Vec::new());
        if may_ask(status) && seq.is_some() && app.paired.get_untracked() {
            read(app, id.clone(), shown, round, current, 0);
        }
    });
    shown
}

fn read(
    app: AppState,
    id: AgentId,
    shown: RwSignal<Vec<PromptText>>,
    round: StoredValue<u64>,
    current: u64,
    polls: u32,
) {
    let is_current = move || round.try_get_value() == Some(current);
    spawn_local(async move {
        let result = session::api(app).suggestions(id.clone()).await;
        if !is_current() {
            return;
        }
        let again = next_poll(polls, &result);
        if let Ok(SuggestionsRead::Ready(raw)) = result {
            let _ = shown.try_set(sanitize(raw));
        }
        if let Some(wait) = again {
            set_timeout(
                move || {
                    if is_current() {
                        read(app, id, shown, round, current, polls.saturating_add(1));
                    }
                },
                wait,
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|&s| s.to_owned()).collect()
    }

    #[test]
    fn only_short_plain_replies_are_shown() {
        let long = "x".repeat(protocol::MAX_SUGGESTION_CHARS + 1);
        let got = sanitize(strings(&[
            "/clear",
            "!rm -rf ~",
            "#remember this",
            "ok\u{1b}[2J",
            "fine\u{202E}",
            "zero\u{200B}width",
            &long,
            "   ",
            "Yes,\n  go ahead",
            "yes, go AHEAD",
            "Run the tests first",
            "Stop here",
            "One too many",
        ]));
        let got: Vec<&str> = got.iter().map(PromptText::as_str).collect();
        assert_eq!(got, ["Yes, go ahead", "Run the tests first", "Stop here"]);
        assert_eq!(
            sanitize(strings(&[&"é".repeat(protocol::MAX_SUGGESTION_CHARS)])).len(),
            1
        );
    }

    fn prompt(text: &str) -> PromptText {
        PromptText::new(text).unwrap()
    }

    #[test]
    fn a_tap_sends_the_suggestion_alone() {
        let yes = prompt("Yes, go ahead");
        assert_eq!(Tap::of("", &yes, false, false), Tap::Send(yes.clone()));
        // A typed draft is not sent with it (it stays in the composer).
        assert_eq!(
            Tap::of("Also bump the version", &yes, false, false),
            Tap::Send(yes.clone())
        );
    }

    #[test]
    fn a_tap_at_a_dialog_only_fills_the_composer_once() {
        let yes = prompt("Yes, go ahead");
        assert_eq!(
            Tap::of("", &yes, true, false),
            Tap::AtDialog("Yes, go ahead".into())
        );
        assert_eq!(
            Tap::of("Thanks!", &yes, true, false),
            Tap::AtDialog("Thanks! Yes, go ahead".into())
        );
        // Tapped again: not appended twice.
        assert_eq!(
            Tap::of("Thanks! Yes, go ahead\n", &yes, true, false),
            Tap::AtDialog("Thanks! Yes, go ahead\n".into())
        );
    }

    #[test]
    fn a_double_tap_sends_once() {
        let yes = prompt("Yes, go ahead");
        assert_eq!(Tap::of("", &yes, false, true), Tap::Spent);
        assert_eq!(Tap::of("draft", &prompt("No"), true, true), Tap::Spent);
    }

    #[test]
    fn polling_slows_down_but_never_gives_up_while_it_may_succeed() {
        let generating = Ok(SuggestionsRead::Generating);
        assert_eq!(next_poll(0, &generating), Some(POLL));
        assert_eq!(next_poll(QUICK_POLLS, &generating), Some(SLOW_POLL));
        assert_eq!(next_poll(10_000, &generating), Some(SLOW_POLL));
        // A flaky network is retried; an answer or a rejected token is final.
        assert_eq!(
            next_poll(0, &Err(ApiError::Transport("offline".into()))),
            Some(SLOW_POLL)
        );
        assert_eq!(
            next_poll(0, &Err(ApiError::Status { status: 503 })),
            Some(SLOW_POLL)
        );
        assert_eq!(next_poll(0, &Err(ApiError::Unauthorized)), None);
        assert_eq!(next_poll(0, &Ok(SuggestionsRead::Ready(vec![]))), None);
    }

    #[test]
    fn only_an_agent_at_its_prompt_is_polled() {
        assert!(may_ask(Some(AgentStatus::Idle)));
        assert!(may_ask(Some(AgentStatus::Done)));
        // Blocked: herdr refuses prompts, so no suggestions.
        assert!(!may_ask(Some(AgentStatus::Blocked)));
        assert!(!may_ask(Some(AgentStatus::Working)));
        assert!(!may_ask(None));
    }
}
