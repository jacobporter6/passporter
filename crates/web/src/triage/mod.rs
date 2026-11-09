//! The triage screen (design §6.1): Blocked · Working · Done, live.
//!
//! The list comes from [`crate::live`]: the cached snapshot first (marked
//! "as of HH:MM"), then the stream. The segments sit just above the dock,
//! for one-thumb switching, with the `+` that starts a new agent
//! ([`crate::spawn`]) beside them; runbooks, rarer, sit in the header.

use leptos::prelude::*;
use protocol::{Agent, AgentStatus};

use crate::agent::status::{StatusLineText, shown_line};
pub use crate::live::agents::Segment;
use crate::live::agents::{counts, in_segment};
use crate::live::{Conn, use_live};
use crate::routes::AppRoute;
use crate::runbooks::RunbooksButton;
use crate::spawn::{NewAgentButton, NewAgentSheet};
use crate::ui::format::{local_clock, time_in_state};
use crate::ui::icons::{self, Icon};
use crate::ui::{EmptyState, PageHeader, use_app};

/// The triage screen.
#[component]
pub fn TriagePage() -> impl IntoView {
    let app = use_app();
    let live = use_live();
    let selected = RwSignal::new(None::<Segment>);
    let tallies = Memo::new(move |_| live.agents.with(|a| counts(a)));
    // Until the user picks one: the first non-empty segment, blocked first.
    let current = Signal::derive(move || {
        selected.get().unwrap_or_else(|| {
            let [blocked, working, _] = tallies.get();
            if blocked > 0 {
                Segment::Blocked
            } else if working > 0 {
                Segment::Working
            } else {
                Segment::Done
            }
        })
    });
    let list = Memo::new(move |_| live.agents.with(|a| in_segment(a, current.get())));
    let new_agent = RwSignal::new(false);

    let subtitle = Signal::derive(move || {
        if !app.paired.get() {
            return if app.token_loaded.get() {
                "Not connected".to_owned()
            } else {
                String::new()
            };
        }
        let total = live.agents.with(Vec::len);
        let agents = if total == 1 {
            "1 agent".to_owned()
        } else {
            format!("{total} agents")
        };
        match live.conn.get() {
            Conn::Live => agents,
            Conn::Off | Conn::Connecting | Conn::Retrying => match live.fresh_at.get() {
                Some(at) => format!("{agents} · as of {}", local_clock(at)),
                None => "Connecting…".to_owned(),
            },
        }
    });

    view! {
        <section class="page page-triage">
            <PageHeader title="Agents" subtitle=subtitle>
                <RunbooksButton shown=app.paired />
            </PageHeader>
            <ConnBadge />
            <Show when=move || app.token_loaded.get() && !app.paired.get()>
                <ConnectPrompt />
            </Show>
            <div
                class="segment-panel"
                role="tabpanel"
                id="triage-panel"
                aria-labelledby=move || format!("tab-{}", current.get().slug())
            >
                {move || {
                    let agents = list.get();
                    if agents.is_empty() {
                        segment_empty_state(current.get()).into_any()
                    } else {
                        view! {
                            <ul class="agent-list" role="list">
                                {agents.into_iter().map(|agent| view! { <li><AgentCard agent=agent /></li> }).collect_view()}
                            </ul>
                        }
                        .into_any()
                    }
                }}
            </div>
            <div class="triage-dock">
                <SegmentTabs current=current selected=selected tallies=tallies />
                <NewAgentButton open=new_agent shown=app.paired />
            </div>
            <NewAgentSheet open=new_agent />
        </section>
    }
}

/// "Reconnecting" pill while the stream is down but a token is present.
#[component]
fn ConnBadge() -> impl IntoView {
    let app = use_app();
    let live = use_live();
    let show = move || app.paired.get() && matches!(live.conn.get(), Conn::Retrying);
    view! {
        <Show when=show>
            <p class="conn-badge" role="status">
                <span class="conn-dot" aria-hidden="true"></span>
                "Reconnecting to the bridge…"
            </p>
        </Show>
    }
}

/// One agent on the triage list.
#[component]
fn AgentCard(agent: Agent) -> impl IntoView {
    let app = use_app();
    let since = agent.status_since;
    let elapsed = move || time_in_state(since, app.now.get());
    // The orchestrator's finished agents: another agent is waiting, not you.
    let quiet = agent.quiet_done && agent.status.is_ready();
    let place = match &agent.worktree {
        Some(worktree) if *worktree != agent.repo => format!("{} · {worktree}", agent.repo),
        _ => agent.repo.clone(),
    };
    let status = status_label(agent.status);
    let line = shown_line(&agent);
    let ask = shown_ask(&agent);
    let label = match (&line, &ask) {
        (Some(line), _) => format!("{}, {status}: {}, {place}", agent.title, line.text),
        (None, Some(ask)) => format!("{}, {status}: {ask}, {place}", agent.title),
        (None, None) => format!("{}, {status}, {place}", agent.title),
    };
    let state = state_slug(agent.status);
    view! {
        <a
            class=format!("agent-card agent-card-{state}")
            class:agent-card-unread=agent.unread
            class:agent-card-quiet=quiet
            href=AppRoute::Agent(agent.id.clone()).href()
            aria-label=label
        >
            <span class="agent-card-top">
                <span class="agent-title">{agent.title.clone()}</span>
                {agent.unread.then(|| view! { <span class="unread-dot" title="Unread"></span> })}
                <span class="agent-time mono">{elapsed}</span>
            </span>
            {match (line, ask) {
                (Some(line), _) => view! { <StatusLineText line=line /> }.into_any(),
                (None, Some(ask)) => view! { <span class="agent-headline agent-ask">{ask}</span> }.into_any(),
                (None, None) => agent.headline.clone().filter(|h| !h.trim().is_empty()).map(|h| view! {
                    <span class="agent-headline">{h}</span>
                }).into_any(),
            }}
            <span class="agent-card-foot">
                <StatusPill status=agent.status />
                <span class="agent-place">{place}</span>
            </span>
        </a>
    }
}

/// What a blocked agent is asking, for its card in place of the headline:
/// `None` unless it is blocked and the bridge knows a non-blank ask.
fn shown_ask(agent: &Agent) -> Option<String> {
    agent
        .ask
        .clone()
        .filter(|ask| agent.status == AgentStatus::Blocked && !ask.trim().is_empty())
}

/// The state's CSS name: `blocked`, `working`, `done` (idle too) or
/// `unknown`.
const fn state_slug(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Blocked => "blocked",
        AgentStatus::Working => "working",
        AgentStatus::Idle | AgentStatus::Done => "done",
        AgentStatus::Unknown => "unknown",
    }
}

/// A small status label in its state's colour; only blocked is filled.
#[component]
pub fn StatusPill(status: AgentStatus) -> impl IntoView {
    let slug = state_slug(status);
    view! {
        <span class=format!("pill pill-{slug}")>
            <span class="pill-dot" aria-hidden="true"></span>
            {status_label(status)}
        </span>
    }
}

/// The status as people say it.
#[must_use]
pub const fn status_label(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Blocked => "Needs you",
        AgentStatus::Working => "Working",
        AgentStatus::Idle => "Idle",
        AgentStatus::Done => "Done",
        AgentStatus::Unknown => "Unknown",
    }
}

fn segment_empty_state(segment: Segment) -> impl IntoView {
    match segment {
        Segment::Blocked => view! {
            <EmptyState icon=icons::HAND title="Nothing needs you" text="Agents waiting on a decision land here first." />
        },
        Segment::Working => view! {
            <EmptyState icon=icons::PULSE title="Nothing running" text="Agents mid-turn show up here with how long they've been at it." />
        },
        Segment::Done => view! {
            <EmptyState icon=icons::CHECK title="Nothing finished yet" text="Finished and idle agents wait here for their next prompt." />
        },
    }
}

/// The segmented control, pinned just above the dock for one-thumb use.
#[component]
fn SegmentTabs(
    current: Signal<Segment>,
    selected: RwSignal<Option<Segment>>,
    tallies: Memo<[usize; 3]>,
) -> impl IntoView {
    view! {
        <div class="segments" role="tablist" aria-label="Agent state">
            {Segment::ALL.into_iter().enumerate().map(|(index, segment)| {
                let count = move || tallies.get()[index];
                let has_any = move || count() != 0;
                view! {
                    <button
                        type="button"
                        role="tab"
                        id=format!("tab-{}", segment.slug())
                        class=format!("segment segment-{}", segment.slug())
                        aria-selected=move || (current.get() == segment).to_string()
                        aria-controls="triage-panel"
                        on:click=move |_| selected.set(Some(segment))
                    >
                        {segment.label()}
                        <Show when=has_any>
                            <span class="segment-count" aria-label=move || format!("{} agents", count())>{count}</span>
                        </Show>
                    </button>
                }
            }).collect_view()}
        </div>
    }
}

/// Shown when the device has no token.
#[component]
fn ConnectPrompt() -> impl IntoView {
    let app = use_app();
    let text = move || {
        if app.token_rejected.get() {
            "The bridge rejected the saved token. Paste a new one."
        } else {
            "Paste the token printed by the bridge setup to see your agents."
        }
    };
    view! {
        <a class="card card-link pair-prompt" href=AppRoute::Pair.href()>
            <span class="card-icon"><Icon paths=icons::KEY /></span>
            <span class="card-body">
                <span class="card-title">"Connect to your bridge"</span>
                <span class="card-text">{text}</span>
            </span>
            <span class="card-chevron"><Icon paths=icons::CHEVRON /></span>
        </a>
    }
}

#[cfg(test)]
mod tests {
    use protocol::{AgentKind, Seq, Timestamp};

    use super::*;

    fn agent(status: AgentStatus, ask: Option<&str>) -> Agent {
        Agent {
            id: "w1:p1".parse().expect("id"),
            title: "worker".to_owned(),
            repo: "passporter".to_owned(),
            worktree: None,
            status,
            seq: Seq(1),
            status_since: Timestamp::from_unix_millis(0),
            unread: false,
            kind: AgentKind::Claude,
            headline: Some("Added the parser".to_owned()),
            status_line: None,
            ask: ask.map(str::to_owned),
            quiet_done: false,
        }
    }

    #[test]
    fn only_a_blocked_agent_shows_a_non_blank_ask() {
        let ask = Some("Run `cargo test`?");
        assert_eq!(shown_ask(&agent(AgentStatus::Blocked, ask)).as_deref(), ask);
        assert_eq!(shown_ask(&agent(AgentStatus::Blocked, Some("  "))), None);
        assert_eq!(shown_ask(&agent(AgentStatus::Blocked, None)), None);
        assert_eq!(shown_ask(&agent(AgentStatus::Working, ask)), None);
    }
}
