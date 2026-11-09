//! A working agent's live status line (design §6.1, §6.4): what it is
//! doing now, made by the bridge from the tail of the turn in progress and
//! carried on the agent (`Agent::status_line`), so it updates with the
//! agent list and the stream.
//!
//! Shown with its age ("2 min ago") under the agent's name in triage
//! ([`StatusLineText`]) and at the top of the agent screen
//! ([`StatusLineBar`]), where a tap asks the bridge for a fresh one
//! (`POST /v1/agents/:id/status`, rate-limited on both ends). Model output
//! about untrusted agent output: rendered as a text node only.

use core::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use protocol::{Agent, AgentId, AgentKind, AgentStatus, StatusLine, Timestamp};

use crate::api::StatusRefresh;
use crate::session;
use crate::ui::icons::{self, Icon};
use crate::ui::use_app;

const MINUTE: u64 = 60_000;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

/// How long a tap waits for its line before the bar takes taps again.
const REFRESH_WAIT: Duration = Duration::from_secs(60);

/// How long the bar rests after "tapped too soon": the bridge's own tap
/// interval. (The app's transport does not expose `Retry-After`.)
const TOO_SOON_WAIT: Duration = Duration::from_secs(30);

/// How long the bar rests after "usage limit": the bridge pauses for up to
/// 30 minutes, so asking every half minute would only be refused.
const PAUSED_WAIT: Duration = Duration::from_mins(5);

/// The age of a line made at `at`, against `now`: `just now`, `1 min ago`,
/// `14 min ago`, `2 h ago`, `3 d ago`. A clock ahead of the phone's reads
/// as just now.
#[must_use]
pub fn age_label(at: Timestamp, now: Timestamp) -> String {
    match at.millis_until(now) {
        m if m < MINUTE => "just now".to_owned(),
        m if m < HOUR => format!("{} min ago", m / MINUTE),
        m if m < DAY => format!("{} h ago", m / HOUR),
        m => format!("{} d ago", m / DAY),
    }
}

/// The line worth showing for `agent`: only while it works, and only if
/// it has visible text.
#[must_use]
pub fn shown_line(agent: &Agent) -> Option<StatusLine> {
    agent
        .status_line
        .clone()
        .filter(|line| agent.status == AgentStatus::Working && !line.text.trim().is_empty())
}

/// The status line and its age, for a triage card.
#[component]
pub fn StatusLineText(line: StatusLine) -> impl IntoView {
    let app = use_app();
    let at = line.at;
    view! {
        <span class="agent-status-line">
            <span class="status-line-text">{line.text}</span>
            <span class="status-line-age">{move || age_label(at, app.now.get())}</span>
        </span>
    }
}

/// Why taps on the bar rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rest {
    /// Tapped too recently (the bridge's per-agent interval).
    TooSoon,
    /// The account is at its usage limit: nothing is made for a while.
    Paused,
}

/// Where a tap on the bar stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    /// A tap asks for a fresh line.
    Ready,
    /// Asked; waiting for a line newer than the one shown then (`None`:
    /// there was none).
    Waiting(Option<Timestamp>),
    /// The bridge said later; taps rest a while.
    Resting(Rest),
}

impl Refresh {
    /// What a new `line` means for a pending tap: a line newer than the
    /// one shown when it was made answers it.
    #[must_use]
    pub fn on_line(self, line: Option<&StatusLine>) -> Self {
        match (self, line) {
            (Self::Waiting(before), Some(line)) if before.is_none_or(|at| line.at > at) => {
                Self::Ready
            }
            (state, _) => state,
        }
    }

    /// The state after the bridge answered a tap (`None`: the tap failed).
    #[must_use]
    pub const fn on_answer(self, answer: Option<StatusRefresh>) -> Self {
        match answer {
            Some(StatusRefresh::Queued) => self,
            Some(StatusRefresh::TooSoon) => Self::Resting(Rest::TooSoon),
            Some(StatusRefresh::Paused) => Self::Resting(Rest::Paused),
            Some(StatusRefresh::Current | StatusRefresh::Unavailable) | None => Self::Ready,
        }
    }

    /// How long this state lasts at most before taps open again.
    #[must_use]
    pub const fn lasts(self) -> Option<Duration> {
        match self {
            Self::Ready => None,
            Self::Waiting(_) => Some(REFRESH_WAIT),
            Self::Resting(Rest::TooSoon) => Some(TOO_SOON_WAIT),
            Self::Resting(Rest::Paused) => Some(PAUSED_WAIT),
        }
    }
}

/// Whether the bar belongs on `agent`'s screen: only a working Claude
/// agent can have a status line (other kinds have no transcript).
#[must_use]
pub fn may_have_line(agent: &Agent) -> bool {
    agent.status == AgentStatus::Working && agent.kind == AgentKind::Claude
}

/// The top of the agent screen while a Claude agent works: its status
/// line and age, tappable for a fresh one. Hidden once the bridge says no
/// line can come (it has no summarizer) while none is shown.
#[component]
pub fn StatusLineBar(id: AgentId, agent: Memo<Option<Agent>>) -> impl IntoView {
    let app = use_app();
    let eligible = Memo::new(move |_| agent.with(|a| a.as_ref().is_some_and(may_have_line)));
    let line = Memo::new(move |_| agent.with(|a| a.as_ref().and_then(shown_line)));
    let unavailable = RwSignal::new(false);
    let refresh = RwSignal::new(Refresh::Ready);
    // Bumped on every state a tap enters, so a timer from an earlier tap
    // never ends a later one.
    let round = StoredValue::new(0_u64);
    let enter = move |state: Refresh| {
        let current = round
            .try_update_value(|r| {
                *r = r.wrapping_add(1);
                *r
            })
            .unwrap_or_default();
        let _ = refresh.try_set(state);
        if let Some(wait) = state.lasts() {
            set_timeout(
                move || {
                    if round.try_get_value() == Some(current) {
                        let _ = refresh.try_set(Refresh::Ready);
                    }
                },
                wait,
            );
        }
    };
    // A new line answers a pending tap.
    Effect::new(move |_| {
        let current = line.get();
        let state = refresh.get_untracked();
        let next = state.on_line(current.as_ref());
        if next != state {
            enter(next);
        }
    });
    let tap = move |_| {
        if refresh.get_untracked() != Refresh::Ready || !app.paired.get_untracked() {
            return;
        }
        crate::platform::haptic_tick();
        let waiting = Refresh::Waiting(line.with_untracked(|l| l.as_ref().map(|l| l.at)));
        enter(waiting);
        let asked = round.get_value();
        let id = id.clone();
        spawn_local(async move {
            let answer = session::api(app).refresh_status(&id).await.ok();
            if round.try_get_value() != Some(asked) {
                return;
            }
            if answer == Some(StatusRefresh::Unavailable) {
                let _ = unavailable.try_set(true);
            }
            let next = waiting.on_answer(answer);
            if next != waiting {
                enter(next);
            }
        });
    };
    let shown = move || eligible.get() && (line.with(Option::is_some) || !unavailable.get());
    let text = move || {
        line.get().map_or_else(
            || "No status yet. Tap to ask what it's doing.".to_owned(),
            |l| l.text,
        )
    };
    let age = move || match refresh.get() {
        Refresh::Waiting(_) => "Updating…".to_owned(),
        Refresh::Resting(Rest::TooSoon) => "Try again shortly".to_owned(),
        Refresh::Resting(Rest::Paused) => "Paused: usage limit".to_owned(),
        Refresh::Ready => line
            .get()
            .map(|l| age_label(l.at, app.now.get()))
            .unwrap_or_default(),
    };
    view! {
        <Show when=shown>
            <button
                type="button"
                class="status-line-bar"
                class:status-line-empty=move || line.with(Option::is_none)
                aria-busy=move || matches!(refresh.get(), Refresh::Waiting(_)).to_string()
                aria-label=move || format!("Status: {}. Refresh status", text())
                on:click=tap.clone()
            >
                <span class="status-line-icon" class:status-line-spinning=move || matches!(refresh.get(), Refresh::Waiting(_))>
                    <Icon paths=icons::PULSE />
                </span>
                <span class="status-line-text">{text}</span>
                <span class="status-line-age" aria-live="polite">{age}</span>
            </button>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use protocol::Seq;

    use super::*;

    const T0: u64 = 1_762_500_000_000;

    fn ts(offset: u64) -> Timestamp {
        Timestamp::from_unix_millis(T0 + offset)
    }

    #[test]
    fn ages_read_like_people_say_them() {
        assert_eq!(age_label(ts(0), ts(59_999)), "just now");
        assert_eq!(
            age_label(ts(MINUTE), ts(0)),
            "just now",
            "a fast bridge clock"
        );
        assert_eq!(age_label(ts(0), ts(MINUTE)), "1 min ago");
        assert_eq!(age_label(ts(0), ts(2 * MINUTE + 30_000)), "2 min ago");
        assert_eq!(age_label(ts(0), ts(59 * MINUTE)), "59 min ago");
        assert_eq!(age_label(ts(0), ts(2 * HOUR + 5 * MINUTE)), "2 h ago");
        assert_eq!(age_label(ts(0), ts(3 * DAY)), "3 d ago");
    }

    fn agent(status: AgentStatus, text: Option<&str>) -> Agent {
        Agent {
            id: "w1:p1~abc".parse().unwrap(),
            title: "circus".into(),
            repo: "passporter".into(),
            worktree: None,
            status,
            seq: Seq(1),
            status_since: ts(0),
            unread: false,
            kind: AgentKind::Claude,
            headline: None,
            status_line: text.map(|text| StatusLine {
                text: text.into(),
                at: ts(0),
            }),
            ask: None,
            quiet_done: false,
        }
    }

    #[test]
    fn a_line_shows_only_while_working() {
        let line = shown_line(&agent(AgentStatus::Working, Some("Running tests")));
        assert_eq!(line.map(|l| l.text).as_deref(), Some("Running tests"));
        assert_eq!(
            shown_line(&agent(AgentStatus::Done, Some("Running tests"))),
            None
        );
        assert_eq!(shown_line(&agent(AgentStatus::Working, Some("  "))), None);
        assert_eq!(shown_line(&agent(AgentStatus::Working, None)), None);
    }

    fn line(at: u64) -> StatusLine {
        StatusLine {
            text: "x".into(),
            at: ts(at),
        }
    }

    #[test]
    fn a_tap_waits_for_a_newer_line() {
        let waiting = Refresh::Waiting(Some(ts(0)));
        assert_eq!(waiting.on_line(Some(&line(0))), waiting, "the same line");
        assert_eq!(waiting.on_line(None), waiting);
        assert_eq!(waiting.on_line(Some(&line(1))), Refresh::Ready);
        assert_eq!(
            Refresh::Waiting(None).on_line(Some(&line(0))),
            Refresh::Ready
        );
        let resting = Refresh::Resting(Rest::TooSoon);
        assert_eq!(resting.on_line(Some(&line(9))), resting);
    }

    #[test]
    fn the_bridges_answer_decides_what_a_tap_does_next() {
        let waiting = Refresh::Waiting(None);
        assert_eq!(waiting.on_answer(Some(StatusRefresh::Queued)), waiting);
        assert_eq!(
            waiting.on_answer(Some(StatusRefresh::Current)),
            Refresh::Ready
        );
        assert_eq!(
            waiting.on_answer(Some(StatusRefresh::Unavailable)),
            Refresh::Ready
        );
        assert_eq!(
            waiting.on_answer(Some(StatusRefresh::TooSoon)),
            Refresh::Resting(Rest::TooSoon)
        );
        assert_eq!(
            waiting.on_answer(Some(StatusRefresh::Paused)),
            Refresh::Resting(Rest::Paused)
        );
        assert_eq!(waiting.on_answer(None), Refresh::Ready, "a failed tap");
    }

    #[test]
    fn a_usage_limit_rests_taps_longer_than_tapping_too_soon() {
        assert_eq!(Refresh::Ready.lasts(), None);
        assert_eq!(Refresh::Waiting(None).lasts(), Some(REFRESH_WAIT));
        let too_soon = Refresh::Resting(Rest::TooSoon).lasts().unwrap();
        let paused = Refresh::Resting(Rest::Paused).lasts().unwrap();
        assert!(paused > too_soon);
    }

    #[test]
    fn only_working_claude_agents_have_a_bar() {
        assert!(may_have_line(&agent(AgentStatus::Working, None)));
        assert!(!may_have_line(&agent(AgentStatus::Blocked, None)));
        let mut codex = agent(AgentStatus::Working, None);
        codex.kind = AgentKind::Codex;
        assert!(!may_have_line(&codex));
    }
}
