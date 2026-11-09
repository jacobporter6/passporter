//! Terminal mode on the phone: the commands an agent's latest message asks
//! the user to run (`GET /v1/agents/:id/commands`), as a strip of chips
//! above the composer, and the confirm that runs one.
//!
//! The strip lives in the app ([`CommandStrip`]): it lists every candidate
//! the bridge found, in message order, decoys included, so the user picks.
//! A tap opens the confirm, drawn by the terminal WASM, loaded on the first
//! tap like an extras surface ([`view`]; [`Surface::Command`]): it shows
//! the exact command and offers "Run here (!)" and "Run in new terminal",
//! the one the agent's wording suggests first. Nothing runs without that
//! second tap. After a new-terminal run it offers "Watch": the terminal's
//! last lines and a few keys.
//!
//! The confirm's feed is a [`Feed`] as JSON: the agent, whether it is at
//! its prompt, and the tapped command. When a new message no longer has
//! that command (by hash), the feed says so and the confirm says it is
//! gone rather than run a different one; the bridge checks again anyway.

pub mod view;

use leptos::prelude::*;
use leptos::task::spawn_local;
use protocol::{
    AgentId, AgentStatus, CommandCandidate, CommandHash, CommandIntent, CommandList, CommandResult,
    CommandRunRequest, CommandSource, IdempotencyKey, Origin, RunTarget, Seq, Terminal, Timestamp,
};
use serde::{Deserialize, Serialize};

use crate::api::ApiError;
use crate::extras::Surface;
use crate::extras::page::Extra;
use crate::session;
use crate::ui::{AppState, use_app};

/// What the confirm is fed: the agent, whether it is at its prompt, and
/// the tapped command while its latest message still has it. Flat, plain
/// fields: the cheapest shape to cross the module boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feed {
    /// The agent whose message it is.
    pub agent: AgentId,
    /// Whether the agent is at its prompt (idle or done), so a command can
    /// be typed into it.
    pub ready: bool,
    /// The tapped command; `None` once a newer message no longer has it.
    pub command: Option<Chosen>,
}

/// A tapped command, as the confirm needs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chosen {
    /// [`CommandCandidate::id`].
    pub id: u16,
    /// [`CommandCandidate::hash`].
    pub hash: CommandHash,
    /// [`CommandCandidate::text`]: what runs.
    pub text: String,
    /// [`CommandCandidate::intent`], by its wire name.
    pub intent: String,
}

impl Chosen {
    /// The confirm's copy of `candidate`.
    #[must_use]
    pub fn of(candidate: &CommandCandidate) -> Self {
        Self {
            id: candidate.id,
            hash: candidate.hash.clone(),
            text: candidate.text.clone(),
            intent: candidate.intent.as_str().to_owned(),
        }
    }

    /// The candidate back.
    #[must_use]
    pub fn candidate(&self) -> CommandCandidate {
        CommandCandidate {
            id: self.id,
            hash: self.hash.clone(),
            text: self.text.clone(),
            intent: CommandIntent::from_wire(&self.intent),
            source: CommandSource::Unknown,
        }
    }
}

/// The candidate `chosen` in `list`, while the latest message still has
/// it.
#[must_use]
pub fn find<'a>(list: &'a CommandList, chosen: &CommandHash) -> Option<&'a CommandCandidate> {
    list.candidates.iter().find(|c| c.hash == *chosen)
}

/// Whether an agent in `status` is at its prompt.
#[must_use]
pub const fn at_prompt(status: Option<AgentStatus>) -> bool {
    matches!(status, Some(AgentStatus::Idle | AgentStatus::Done))
}

/// The run buttons, the one the wording suggests first: a new terminal
/// first when the agent said so, else here.
#[must_use]
pub const fn order(intent: CommandIntent) -> [RunTarget; 2] {
    match intent {
        CommandIntent::NewTerminal => [RunTarget::NewTerminal, RunTarget::Here],
        CommandIntent::InSession | CommandIntent::Unspecified => {
            [RunTarget::Here, RunTarget::NewTerminal]
        }
    }
}

/// Why `candidate` cannot run in the agent's session now, if it cannot:
/// several lines never do, and nothing can be typed into an agent that is
/// not at its prompt.
#[must_use]
pub fn here_unavailable(candidate: &CommandCandidate, ready: bool) -> Option<&'static str> {
    if candidate.is_multiline() {
        Some("Several lines run only in a new terminal.")
    } else if !ready {
        Some("The agent is busy; run it here once it's at its prompt.")
    } else {
        None
    }
}

/// A chip's badge for `intent`: `!` for in-session, `new` for a new
/// terminal, none when the agent didn't say.
#[must_use]
pub const fn badge(intent: CommandIntent) -> Option<&'static str> {
    match intent {
        CommandIntent::InSession => Some("!"),
        CommandIntent::NewTerminal => Some("new"),
        CommandIntent::Unspecified => None,
    }
}

/// What the agent's wording says, in words, for the confirm.
#[must_use]
pub const fn suggests(intent: CommandIntent) -> &'static str {
    match intent {
        CommandIntent::InSession => "The agent asks for it in its session, so it sees the output.",
        CommandIntent::NewTerminal => "The agent asks for it in a terminal of its own.",
        CommandIntent::Unspecified => "The agent didn't say where to run it.",
    }
}

/// How long an undecided request is repeated (same key) when the same
/// command is confirmed again for the same place.
pub const REUSE_KEY_MS: u64 = 2 * 60 * 1000;

/// The last run request, remembered until the bridge decides it, so that a
/// confirm after "no answer" repeats it, key and all: the bridge then
/// replays instead of running it twice.
#[derive(Debug, Clone, Default)]
pub struct Attempt {
    last: Option<(AgentId, CommandRunRequest)>,
}

impl Attempt {
    /// The request that runs `candidate` of `agent` at `target`: the last
    /// one again if it was for the same, is undecided and is under
    /// [`REUSE_KEY_MS`] old, else a new one with `new_key` (called only
    /// then) at `now`. Always from the app: the car never runs commands.
    /// `None` if no key could be made.
    pub fn request(
        &mut self,
        agent: &AgentId,
        candidate: &CommandCandidate,
        target: RunTarget,
        new_key: impl FnOnce() -> Option<IdempotencyKey>,
        now: Timestamp,
    ) -> Option<CommandRunRequest> {
        if let Some((last_agent, last)) = &self.last
            && last_agent == agent
            && last.candidate == candidate.id
            && last.hash == candidate.hash
            && last.target == target
            && last.created_at.millis_until(now) < REUSE_KEY_MS
        {
            return Some(last.clone());
        }
        let request = CommandRunRequest {
            idempotency_key: new_key()?,
            origin: Origin::App,
            created_at: now,
            candidate: candidate.id,
            hash: candidate.hash.clone(),
            target,
        };
        self.last = Some((agent.clone(), request.clone()));
        Some(request)
    }

    /// The bridge decided: the next run is a new request.
    pub fn settle(&mut self) {
        self.last = None;
    }
}

/// What the confirm does with a bridge answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// Typed into the agent: done.
    Sent,
    /// Running in a new terminal: offer to watch it.
    Started(Terminal),
    /// Nothing decided: show the message; confirming again repeats the
    /// request.
    Retry(String),
    /// Decided, nothing ran (or herdr failed): show the message.
    Show(String),
}

impl Next {
    /// Whether the bridge decided the request (its key is spent).
    #[must_use]
    pub const fn is_final(&self) -> bool {
        !matches!(self, Self::Retry(_))
    }
}

/// Maps a run's outcome to what the confirm does.
#[must_use]
pub fn next(outcome: Result<CommandResult, ApiError>) -> Next {
    let or = |message: String, fallback: &str| {
        if message.is_empty() {
            fallback.to_owned()
        } else {
            message
        }
    };
    match outcome {
        Ok(CommandResult::Sent { .. }) => Next::Sent,
        Ok(CommandResult::Started { terminal, .. }) => Next::Started(terminal),
        Ok(CommandResult::Refused { reason, message }) => {
            let shown = or(message, "The bridge refused to run it.");
            if reason.is_transient() {
                Next::Retry(shown)
            } else {
                Next::Show(shown)
            }
        }
        Ok(CommandResult::Failed { message }) => Next::Show(or(message, "herdr couldn't run it.")),
        Ok(CommandResult::Unknown) => Next::Show(
            "Unreadable answer from the bridge; the command may have run. Look in herdr."
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

/// A chip's label: the command's first line, with `…` when there are more.
#[must_use]
pub fn label(candidate: &CommandCandidate) -> String {
    let mut lines = candidate.text.lines();
    let first = lines.next().unwrap_or_default();
    if lines.next().is_some() {
        format!("{first} …")
    } else {
        first.to_owned()
    }
}

/// The commands of agent `id`'s latest message, read when the agent view
/// opens and again on every status or seq change (cleared meanwhile, so a
/// chip never outlives its message for long; the bridge checks again on
/// every run).
pub fn commands_feed(
    app: AppState,
    id: AgentId,
    status: Signal<Option<AgentStatus>>,
    listed_seq: Signal<Option<Seq>>,
) -> RwSignal<CommandList> {
    let list = RwSignal::new(CommandList::default());
    let round = StoredValue::new(0_u64);
    Effect::new(move |_| {
        status.track();
        listed_seq.track();
        let current = round
            .try_update_value(|r| {
                *r = r.wrapping_add(1);
                *r
            })
            .unwrap_or_default();
        list.set(CommandList::default());
        if !app.paired.get_untracked() {
            return;
        }
        let id = id.clone();
        spawn_local(async move {
            let read = session::api(app).commands(id).await;
            if round.try_get_value() == Some(current)
                && let Ok(read) = read
            {
                let _ = list.try_set(read);
            }
        });
    });
    list
}

/// "Commands in this message": a chip per candidate, and the confirm for
/// the one tapped.
#[component]
pub fn CommandStrip(
    id: AgentId,
    status: Signal<Option<AgentStatus>>,
    listed_seq: Signal<Option<Seq>>,
) -> impl IntoView {
    let app = use_app();
    let list = commands_feed(app, id.clone(), status, listed_seq);
    let chosen = RwSignal::new(None::<CommandHash>);
    let asking = Memo::new(move |_| chosen.with(Option::is_some));
    let feed = Signal::derive(move || {
        let Some(chosen) = chosen.get() else {
            return String::new();
        };
        let command = list.with(|l| find(l, &chosen).map(Chosen::of));
        let feed = Feed {
            agent: id.clone(),
            ready: at_prompt(status.get()),
            command,
        };
        serde_json::to_string(&feed).unwrap_or_default()
    });
    view! {
        {move || {
            let candidates = list.with(|l| l.candidates.clone());
            (!candidates.is_empty()).then(|| view! {
                <div class="command-strip" role="group" aria-label="Commands in this message">
                    <span class="command-strip-label" aria-hidden="true">"Run"</span>
                    {candidates.into_iter().map(|candidate| {
                        let text = label(&candidate);
                        let hash = candidate.hash.clone();
                        view! {
                            <button
                                type="button"
                                class="command-chip"
                                title=candidate.text.clone()
                                on:click=move |_| chosen.set(Some(hash.clone()))
                            >
                                {badge(candidate.intent).map(|b| view! { <span class="command-badge">{b}</span> })}
                                <code class="command-chip-text">{text}</code>
                            </button>
                        }
                    }).collect_view()}
                </div>
            })
        }}
        {move || asking.get().then(|| view! {
            <Extra surface=Surface::Command feed=feed on_close=Callback::new(move |()| chosen.set(None)) />
        })}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";

    fn candidate(text: &str, intent: CommandIntent) -> CommandCandidate {
        CommandCandidate {
            id: 0,
            hash: CommandHash::new(HASH).unwrap(),
            text: text.into(),
            intent,
            source: CommandSource::Inline,
        }
    }

    fn key(n: u8) -> IdempotencyKey {
        IdempotencyKey::new(format!("00000000-0000-4000-8000-0000000000{n:02x}")).unwrap()
    }

    #[test]
    fn the_suggested_place_comes_first() {
        assert_eq!(
            order(CommandIntent::NewTerminal),
            [RunTarget::NewTerminal, RunTarget::Here]
        );
        assert_eq!(
            order(CommandIntent::InSession),
            [RunTarget::Here, RunTarget::NewTerminal]
        );
        assert_eq!(
            order(CommandIntent::Unspecified),
            [RunTarget::Here, RunTarget::NewTerminal]
        );
    }

    #[test]
    fn here_needs_one_line_and_an_agent_at_its_prompt() {
        let one = candidate("gcloud auth login", CommandIntent::InSession);
        assert_eq!(here_unavailable(&one, true), None);
        assert!(here_unavailable(&one, false).is_some());
        let block = candidate("npm ci\nnpm test", CommandIntent::Unspecified);
        assert!(here_unavailable(&block, true).is_some());
        assert!(at_prompt(Some(AgentStatus::Idle)) && at_prompt(Some(AgentStatus::Done)));
        assert!(!at_prompt(Some(AgentStatus::Working)) && !at_prompt(Some(AgentStatus::Blocked)));
    }

    #[test]
    fn chips_show_the_first_line_and_a_badge() {
        assert_eq!(
            label(&candidate("npm ci\nnpm test", CommandIntent::Unspecified)),
            "npm ci …"
        );
        assert_eq!(
            label(&candidate("make", CommandIntent::Unspecified)),
            "make"
        );
        assert_eq!(badge(CommandIntent::InSession), Some("!"));
        assert_eq!(badge(CommandIntent::NewTerminal), Some("new"));
        assert_eq!(badge(CommandIntent::Unspecified), None);
    }

    #[test]
    fn the_feed_crosses_the_module_boundary_and_candidates_are_found_by_hash() {
        let make = candidate("make", CommandIntent::NewTerminal);
        let list = CommandList {
            candidates: vec![make.clone()],
            omitted: 0,
        };
        let chosen = CommandHash::new(HASH).unwrap();
        let feed = Feed {
            agent: "w1:p2".parse().unwrap(),
            ready: true,
            command: find(&list, &chosen).map(Chosen::of),
        };
        let json = serde_json::to_string(&feed).unwrap();
        let back: Feed = serde_json::from_str(&json).unwrap();
        assert_eq!(back, feed);
        let again = back.command.unwrap().candidate();
        assert_eq!(
            (again.id, &again.hash, &again.text),
            (make.id, &make.hash, &make.text)
        );
        assert_eq!(again.intent, CommandIntent::NewTerminal);
        // A new message: the hash no longer matches anything.
        let moved = CommandHash::new("ffffffffffffffffffffffffffffffff").unwrap();
        assert_eq!(find(&list, &moved), None);
    }

    #[test]
    fn an_undecided_request_is_repeated_with_its_key() {
        let agent: AgentId = "w1:p2".parse().unwrap();
        let make = candidate("make", CommandIntent::InSession);
        let mut attempt = Attempt::default();
        let at = Timestamp::from_unix_millis(1_000);
        let first = attempt
            .request(&agent, &make, RunTarget::Here, || Some(key(1)), at)
            .unwrap();
        let again = attempt
            .request(&agent, &make, RunTarget::Here, || Some(key(2)), at)
            .unwrap();
        assert_eq!(again.idempotency_key, first.idempotency_key);
        assert_eq!(again.origin, Origin::App);
        // Another place is another request.
        let elsewhere = attempt
            .request(&agent, &make, RunTarget::NewTerminal, || Some(key(3)), at)
            .unwrap();
        assert_eq!(elsewhere.idempotency_key, key(3));
        // Once decided, a new key.
        attempt.settle();
        let later = attempt
            .request(&agent, &make, RunTarget::NewTerminal, || Some(key(4)), at)
            .unwrap();
        assert_eq!(later.idempotency_key, key(4));
    }

    #[test]
    fn answers_map_to_what_the_confirm_does() {
        assert_eq!(
            next(Ok(CommandResult::Sent { replayed: false })),
            Next::Sent
        );
        let terminal = Terminal {
            pane: protocol::PaneId::new("w9:p1").unwrap(),
            label: "run: api".into(),
        };
        assert_eq!(
            next(Ok(CommandResult::Started {
                terminal: terminal.clone(),
                replayed: true
            })),
            Next::Started(terminal)
        );
        let refused = |reason| {
            next(Ok(CommandResult::Refused {
                reason,
                message: String::new(),
            }))
        };
        assert!(matches!(
            refused(protocol::CommandRefusal::RateLimited),
            Next::Retry(_)
        ));
        assert!(matches!(
            refused(protocol::CommandRefusal::Stale),
            Next::Show(_)
        ));
        assert!(matches!(
            next(Err(ApiError::Transport("offline".into()))),
            Next::Retry(_)
        ));
        assert!(matches!(next(Err(ApiError::Unauthorized)), Next::Show(_)));
        assert!(!Next::Retry(String::new()).is_final());
        assert!(Next::Sent.is_final());
    }
}
