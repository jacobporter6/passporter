//! Short encrypted notices for native subscribers (design §6.6).
//!
//! The dispatcher collects, per device, which agents woke it since its
//! last message ([`Batch`]): a new dialog or a finished turn. When the
//! device's message goes out, [`notices`] turns the batch into [`Notice`]s
//! from what the hub holds *then* (an agent that moved on since is left
//! out), for subscriptions that asked for them
//! ([`protocol::PushPayload::Notice`]). Ping subscriptions get the bare
//! ping as before.
//!
//! A finished turn's *Decide* line arrives later, with its TL;DR. It never
//! joins a batch (where it could displace a dialog's notice, or spend the
//! device's rate limit on a message no ping subscription gets): the
//! dispatcher sends it on its own, to notice subscriptions only
//! ([`decide_notice`]).
//!
//! Every text is agent output: cleaned of control and invisible characters
//! and bounded ([`one_line`]), so a notification cannot be made to hide or
//! reorder text. The bounds are in characters; [`message`] then drops the
//! oldest notices until the message fits the push body limit in bytes
//! (wide characters could otherwise push it over, and lose the ping too).
//!
//! Android Auto announces a notice only when it says `announce`: every
//! blocked agent, and finished ones as [`CarAnnounce`] says.

use protocol::{AgentId, AgentStatus, MAX_NOTICES, Notice, NoticeState, PushMessage};
use serde::Deserialize;

use super::ece::MAX_PUSH_PLAINTEXT;
use super::transitions::Change;
use crate::one_line::one_line;
use crate::pending::parsed_at;
use crate::registry::Hub;

/// Most characters of a notice's agent name.
pub(crate) const MAX_NAME_CHARS: usize = 60;

/// Most characters of a notice's line (what a blocked agent asks, or a
/// finished turn's headline).
pub(crate) const MAX_LINE_CHARS: usize = 160;

/// When Android Auto announces an agent that finished its turn (`[push]
/// car_announce_done`). Blocked agents are always announced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CarAnnounce {
    /// Only once the turn's TL;DR has a *Decide* line: something for you
    /// to decide, not just news. The default.
    #[default]
    Decide,
    /// Every finished turn that pings, and its *Decide* line when it comes.
    Always,
    /// Never: finished turns still reach the phone, silently in the car.
    Never,
}

/// What woke the dispatcher for an agent: news for every subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wake {
    /// It is at a new dialog.
    Blocked,
    /// It finished a turn (one that pings, see [`super::QuietDone`]).
    Finished,
}

/// What a notice is about.
enum News<'a> {
    Woke(Wake),
    /// The finished turn's TL;DR came with this *Decide* line.
    Decide(&'a str),
}

/// What a device's next message covers, oldest first, one entry per agent
/// (the newest wins: a dialog after a finished turn, or the reverse).
#[derive(Debug, Default)]
pub(crate) struct Batch {
    woke: Vec<(AgentId, Wake)>,
}

impl Batch {
    /// Adds `wake` of `agent`, replacing what the batch said of it.
    pub(crate) fn add(&mut self, agent: AgentId, wake: Wake) {
        self.woke.retain(|(known, _)| *known != agent);
        self.woke.push((agent, wake));
    }

    /// Whether nothing woke it.
    pub(crate) fn is_empty(&self) -> bool {
        self.woke.is_empty()
    }

    /// Drops the news that lapsed by now in `hub`
    /// ([`Change::lapsed`]), logging each; whether any is left. If none
    /// is, the message is withdrawn (logged too): ids and reasons only.
    pub(crate) fn keep_lasting(&mut self, hub: &Hub) -> bool {
        let had_news = !self.woke.is_empty();
        self.woke.retain(|(agent, wake)| {
            let change = match wake {
                Wake::Blocked => Change::Blocked(agent.clone()),
                Wake::Finished => Change::Finished(agent.clone()),
            };
            let lapse = change.lapsed(hub.agent_in(agent.pane()).as_ref());
            if let Some(lapse) = lapse {
                tracing::info!(%agent, reason = lapse.reason(), "ping reason lapsed");
            }
            lapse.is_none()
        });
        if had_news && self.woke.is_empty() {
            tracing::info!("ping withdrawn: nothing left to tell the user");
        }
        !self.woke.is_empty()
    }
}

/// The notices of `batch`, from `hub` as it is now: at most
/// [`MAX_NOTICES`], the newest. An agent that is gone, or left the state
/// its news was about, is left out.
pub(crate) fn notices(batch: &Batch, hub: &Hub, announce: CarAnnounce) -> Vec<Notice> {
    let mut notices: Vec<Notice> = batch
        .woke
        .iter()
        .filter_map(|(id, wake)| notice(id, &News::Woke(*wake), hub, announce))
        .collect();
    let extra = notices.len().saturating_sub(MAX_NOTICES);
    notices.drain(..extra);
    notices
}

/// The message of `ping` and `notices`, dropping notices (finished ones
/// first, oldest first) until its plaintext fits [`MAX_PUSH_PLAINTEXT`]
/// bytes. The ping alone always fits.
pub(crate) fn message(ping: String, mut notices: Vec<Notice>) -> PushMessage {
    let size = |message: &PushMessage| serde_json::to_vec(message).map_or(usize::MAX, |v| v.len());
    let mut message = PushMessage {
        ping,
        notices: Vec::new(),
    };
    while !notices.is_empty() {
        message.notices = notices;
        if size(&message) <= MAX_PUSH_PLAINTEXT {
            return message;
        }
        notices = std::mem::take(&mut message.notices);
        // A dialog's notice is the one the car can answer: finished ones go
        // first, oldest first, then the oldest dialogs.
        let drop = notices
            .iter()
            .position(|notice| notice.state != NoticeState::Blocked)
            .unwrap_or(0);
        notices.remove(drop);
    }
    message.notices = notices;
    message
}

/// The notice of `id`'s *Decide* line, unless it is no longer finished
/// (back at work, or at a dialog: that dialog's notice is what matters).
pub(crate) fn decide_notice(
    id: &AgentId,
    decide: &str,
    hub: &Hub,
    announce: CarAnnounce,
) -> Option<Notice> {
    notice(id, &News::Decide(decide), hub, announce)
}

fn notice(id: &AgentId, news: &News<'_>, hub: &Hub, announce: CarAnnounce) -> Option<Notice> {
    let agent = hub.agent_in(id.pane()).filter(|agent| agent.id == *id)?;
    let name = one_line(&agent.title, MAX_NAME_CHARS).unwrap_or_else(|| "Agent".to_owned());
    let headline = || {
        agent
            .headline
            .as_deref()
            .and_then(|line| one_line(line, MAX_LINE_CHARS))
    };
    let finished = |line, decide, announce| Notice {
        agent_id: id.clone(),
        name: name.clone(),
        state: NoticeState::Finished,
        line,
        decide,
        pending_id: None,
        announce,
    };
    match news {
        News::Woke(Wake::Blocked) => (agent.status == AgentStatus::Blocked).then(|| {
            // Only a parse of the dialog on screen now: an older one's id
            // would be refused as stale, so the car would rather not offer it.
            let dialog = hub
                .dialog(id)
                .filter(|(pending_id, _)| parsed_at(pending_id, agent.seq.0));
            let (pending_id, ask) = dialog.map_or((None, None), |(id, ask)| (Some(id), ask));
            Notice {
                agent_id: id.clone(),
                name: name.clone(),
                state: NoticeState::Blocked,
                line: ask.as_deref().and_then(|ask| one_line(ask, MAX_LINE_CHARS)),
                decide: None,
                pending_id,
                announce: true,
            }
        }),
        News::Woke(Wake::Finished) => agent
            .status
            .is_ready()
            .then(|| finished(headline(), None, announce == CarAnnounce::Always)),
        News::Decide(decide) => agent.status.is_ready().then(|| {
            finished(
                headline(),
                one_line(decide, MAX_LINE_CHARS),
                announce != CarAnnounce::Never,
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use protocol::{PendingId, PendingInput};

    use super::*;
    use crate::push::transitions::tests::agent;

    fn hub_with(agents: Vec<protocol::Agent>) -> Hub {
        let hub = Hub::new(16);
        for a in agents {
            hub.upsert(&herdr::PaneId::new(a.id.pane().as_str()), a);
        }
        hub
    }

    fn id(pane: &str) -> AgentId {
        pane.parse().expect("id")
    }

    #[test]
    fn blocked_agents_are_announced_with_their_ask_and_pending_id() {
        let hub = hub_with(vec![agent("w1:p1", AgentStatus::Blocked, 2)]);
        let mut pending = PendingInput::unparsed_dialog(PendingId::new("screen-2-ab").expect("id"));
        pending.prompt = "Run `rm -rf build`?".to_owned();
        hub.set_pending(&id("w1:p1"), Some(pending));
        let mut batch = Batch::default();
        batch.add(id("w1:p1"), Wake::Blocked);
        let notices = notices(&batch, &hub, CarAnnounce::Decide);
        assert_eq!(notices.len(), 1);
        let notice = &notices[0];
        assert_eq!(notice.state, NoticeState::Blocked);
        assert_eq!(notice.name, "title of w1:p1");
        assert_eq!(notice.line.as_deref(), Some("Run `rm -rf build`?"));
        assert_eq!(
            notice.pending_id.as_ref().map(PendingId::as_str),
            Some("screen-2-ab")
        );
        assert!(notice.announce);
    }

    #[test]
    fn a_parse_from_before_the_dialog_on_screen_is_not_offered() {
        // Blocked again at seq 5, the cached parse is still seq 3's.
        let hub = hub_with(vec![agent("w1:p1", AgentStatus::Blocked, 5)]);
        let mut old = PendingInput::unparsed_dialog(PendingId::new("screen-3-ab").expect("id"));
        old.prompt = "Run `ls`?".to_owned();
        hub.set_pending(&id("w1:p1"), Some(old));
        let mut batch = Batch::default();
        batch.add(id("w1:p1"), Wake::Blocked);
        let notice = notices(&batch, &hub, CarAnnounce::Decide).remove(0);
        assert_eq!(notice.pending_id, None, "nothing the car could answer");
        assert_eq!(notice.line, None, "nor the old dialog's question");
        assert!(notice.announce, "still: it waits for you");
    }

    #[test]
    fn finished_agents_are_announced_as_the_policy_says() {
        let mut done = agent("w1:p1", AgentStatus::Done, 3);
        done.headline = Some("Fixed the flaky test".to_owned());
        let hub = hub_with(vec![done]);
        let finished = |announce| {
            let mut batch = Batch::default();
            batch.add(id("w1:p1"), Wake::Finished);
            notices(&batch, &hub, announce).remove(0)
        };
        let decided = |announce| {
            decide_notice(&id("w1:p1"), "Merge PR #12?", &hub, announce).expect("notice")
        };
        // Decide (the default): silent until the Decide line comes.
        assert!(!finished(CarAnnounce::Decide).announce);
        let notice = decided(CarAnnounce::Decide);
        assert!(notice.announce);
        assert_eq!(notice.state, NoticeState::Finished);
        assert_eq!(notice.decide.as_deref(), Some("Merge PR #12?"));
        assert_eq!(notice.line.as_deref(), Some("Fixed the flaky test"));
        assert!(finished(CarAnnounce::Always).announce);
        assert!(decided(CarAnnounce::Always).announce);
        assert!(!finished(CarAnnounce::Never).announce);
        assert!(
            !decided(CarAnnounce::Never).announce,
            "still sent, silently"
        );
    }

    #[test]
    fn wide_characters_never_push_a_message_past_the_limit() {
        // Four notices at the character bounds, all four-byte characters,
        // plus the longest Decide line: past the byte budget.
        let emoji = |n: usize| "😀".repeat(n);
        let notice = |n: usize| Notice {
            agent_id: id(&format!("w1:p{n}")),
            name: emoji(MAX_NAME_CHARS),
            state: NoticeState::Finished,
            line: Some(emoji(MAX_LINE_CHARS)),
            decide: Some(emoji(MAX_LINE_CHARS)),
            pending_id: None,
            announce: true,
        };
        let all: Vec<Notice> = (1..=MAX_NOTICES).map(notice).collect();
        let ping = "0".repeat(32);
        let full = PushMessage {
            ping: ping.clone(),
            notices: all.clone(),
        };
        assert!(serde_json::to_vec(&full).expect("json").len() > MAX_PUSH_PLAINTEXT);
        let fitted = message(ping.clone(), all);
        assert!(serde_json::to_vec(&fitted).expect("json").len() <= MAX_PUSH_PLAINTEXT);
        assert!(!fitted.notices.is_empty(), "as many as fit");
        assert_eq!(
            fitted
                .notices
                .last()
                .map(|n| n.agent_id.to_string())
                .as_deref(),
            Some("w1:p4"),
            "the newest are kept, the oldest go first"
        );
        assert!(
            fitted
                .notices
                .iter()
                .all(|n| n.agent_id.to_string() != "w1:p1")
        );
        assert_eq!(message(ping, Vec::new()).notices, Vec::new());
    }

    #[test]
    fn a_decide_line_is_dropped_once_the_agent_moved_on() {
        let hub = hub_with(vec![agent("w1:p1", AgentStatus::Blocked, 4)]);
        assert_eq!(
            decide_notice(&id("w1:p1"), "Merge?", &hub, CarAnnounce::Decide),
            None,
            "back at a dialog: that dialog's notice is what counts"
        );
        // A dialog stays in the batch whatever else happens to its agent's
        // older turn: only a newer dialog or finish replaces it.
        let mut batch = Batch::default();
        batch.add(id("w1:p1"), Wake::Blocked);
        let blocked = notices(&batch, &hub, CarAnnounce::Decide);
        assert_eq!(blocked.len(), 1);
        assert!(blocked[0].announce);
    }

    #[test]
    fn agents_that_moved_on_are_left_out_and_the_newest_are_kept() {
        let mut agents: Vec<_> = (1..=6)
            .map(|n| agent(&format!("w1:p{n}"), AgentStatus::Blocked, 2))
            .collect();
        agents[0].status = AgentStatus::Working;
        let hub = hub_with(agents);
        let mut batch = Batch::default();
        for n in 1..=6 {
            batch.add(id(&format!("w1:p{n}")), Wake::Blocked);
        }
        batch.add(id("w9:p9"), Wake::Blocked);
        // A finished turn of an agent now blocked again is not news.
        batch.add(id("w1:p2"), Wake::Finished);
        assert!(!batch.is_empty());
        let names: Vec<String> = notices(&batch, &hub, CarAnnounce::Decide)
            .into_iter()
            .map(|notice| notice.agent_id.to_string())
            .collect();
        assert_eq!(names, ["w1:p3", "w1:p4", "w1:p5", "w1:p6"]);
        assert_eq!(names.len(), MAX_NOTICES);
    }
}
