//! The TL;DR card's model (design §6.4): what the card shows for the
//! agent's latest finished turn (or a turn the user asked to summarise),
//! given the cached copy and each read of `GET /v1/agents/:id/tldr`.
//!
//! The last TL;DR is cached in `IndexedDB` ([`LatestTldrKey`]) so the card
//! paints straight away, and offline, with "as of HH:MM" until the bridge
//! confirms it. A failed read changes nothing: the cached card stays.
//!
//! Pure, so the rules are tested on the host. The view is
//! [`super::tldr_view`].

use protocol::{AgentId, AgentStatus, Seq, Timestamp, Tldr};

use crate::api::{ApiError, TldrRead};
use crate::store::{Snapshot, StoreKey, StoreName};

/// The latest TL;DR the app has seen for one agent, whichever turn it was
/// for (the endpoint serves the latest finished turn's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestTldrKey(pub AgentId);

impl StoreKey for LatestTldrKey {
    type Value = Snapshot<Tldr>;
    const STORE: StoreName = StoreName::Snapshots;
    fn encode(&self) -> String {
        format!("tldr/{}/latest", self.0)
    }
}

/// What the app knows about the agent's TL;DR.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CardState {
    /// Nothing yet: no cached copy, no answer from the bridge.
    #[default]
    Unknown,
    /// The cached copy, not (yet) confirmed by the bridge.
    Cached(Snapshot<Tldr>),
    /// The bridge's current answer.
    Fresh(Snapshot<Tldr>),
    /// The bridge is summarizing the latest turn.
    Summarizing,
    /// The bridge has no TL;DR for the latest turn (a short turn, or not a
    /// Claude agent).
    Empty,
    /// The bridge is making another turn's TL;DR on request, or this
    /// device asked for too many lately (`429`): ask again later.
    Busy,
}

/// What to do with the cached copy after a read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheOp {
    /// Store this as the latest TL;DR.
    Put(Snapshot<Tldr>),
    /// Forget the cached TL;DR: it no longer describes the latest turn.
    Delete,
    /// Leave it as it is.
    Keep,
}

/// What the card shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Card {
    /// A slim "summarizing" placeholder.
    Summarizing,
    /// A slim "busy, try again" note.
    Busy,
    /// A slim "no TL;DR" note, for a turn the user asked to summarise.
    Unavailable,
    /// The summary. Its age is [`CardState::as_of`], kept apart so a
    /// confirmed copy of the same summary is the same card.
    Summary {
        /// Its sections, in display order.
        sections: Vec<Section>,
        /// The text read aloud.
        speech: String,
    },
}

impl CardState {
    /// Adopts the cached copy, unless the bridge has already answered.
    #[must_use]
    pub fn with_cached(self, cached: Option<Snapshot<Tldr>>) -> Self {
        match (self, cached) {
            (Self::Unknown, Some(snapshot)) => Self::Cached(snapshot),
            (state, _) => state,
        }
    }

    /// The state after a read made at `now`, and what to do with the cache.
    /// A failed read (offline, bridge down) keeps what is shown.
    #[must_use]
    pub fn with_read(&self, read: Result<TldrRead, ApiError>, now: Timestamp) -> (Self, CacheOp) {
        match read {
            Ok(TldrRead::Ready(tldr)) => {
                let snapshot = Snapshot {
                    fetched_at: now,
                    value: tldr,
                };
                (Self::Fresh(snapshot.clone()), CacheOp::Put(snapshot))
            }
            // The cached copy may be the previous turn's: keep it for
            // offline use, but don't show it as current.
            Ok(TldrRead::Generating) => (Self::Summarizing, CacheOp::Keep),
            Ok(TldrRead::None) => (Self::Empty, CacheOp::Delete),
            Err(ApiError::Status { status: 429 }) => (Self::Busy, CacheOp::Keep),
            Err(_) => (self.clone(), CacheOp::Keep),
        }
    }

    /// Whether the bridge is still summarizing (so it is worth asking
    /// again soon). Not when it was busy: its answer carries a wait the
    /// app cannot read, so a busy card asks again only when another
    /// TL;DR is announced (the slot is likely free then).
    #[must_use]
    pub const fn is_summarizing(&self) -> bool {
        matches!(self, Self::Summarizing)
    }

    /// Whether a card for a turn asked for by number still wants an
    /// answer: once it has one, it is not read again (a read of the turn
    /// in progress could make another provisional TL;DR).
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        matches!(self, Self::Unknown | Self::Summarizing | Self::Busy)
    }

    /// The card to show, if any; `asked`: for a turn the user asked to
    /// summarise, which says so when there is no TL;DR rather than
    /// showing nothing.
    #[must_use]
    pub fn card(&self, asked: bool) -> Option<Card> {
        let snapshot = match self {
            Self::Empty if asked => return Some(Card::Unavailable),
            Self::Unknown | Self::Empty => return None,
            Self::Summarizing => return Some(Card::Summarizing),
            Self::Busy => return Some(Card::Busy),
            Self::Cached(snapshot) | Self::Fresh(snapshot) => snapshot,
        };
        let sections = sections(&snapshot.value);
        if sections.is_empty() {
            return None;
        }
        Some(Card::Summary {
            speech: speech(&sections),
            sections,
        })
    }

    /// When the shown summary was fetched, if it is a cached copy the
    /// bridge has not confirmed (shown as "as of HH:MM").
    #[must_use]
    pub const fn as_of(&self) -> Option<Timestamp> {
        match self {
            Self::Cached(snapshot) => Some(snapshot.fetched_at),
            _ => None,
        }
    }
}

/// The agent seq the card asks the bridge again on: the seq while the
/// agent is ready (idle or done), else the `previous` one. While it works
/// or waits for input its latest finished turn cannot change, so the many
/// seq bumps then need no read.
#[must_use]
pub fn finished_seq(
    previous: Option<Seq>,
    status: Option<AgentStatus>,
    seq: Option<Seq>,
) -> Option<Seq> {
    if status.is_some_and(AgentStatus::is_ready) {
        seq
    } else {
        previous
    }
}

/// One part of the summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionKind {
    /// Did it work? One sentence.
    Outcome,
    /// Files and areas touched.
    Changed,
    /// What the agent needs from the user.
    Decide,
    /// What it flagged or skipped.
    Risks,
}

impl SectionKind {
    /// The heading shown (and spoken).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Outcome => "Outcome",
            Self::Changed => "Changed",
            Self::Decide => "Decide",
            Self::Risks => "Risks",
        }
    }

    /// The CSS modifier class.
    #[must_use]
    pub const fn class(self) -> &'static str {
        match self {
            Self::Outcome => "tldr-section tldr-outcome",
            Self::Changed => "tldr-section tldr-changed",
            Self::Decide => "tldr-section tldr-decide",
            Self::Risks => "tldr-section tldr-risks",
        }
    }
}

/// A section and its lines (one for Outcome and Decide, bullets for the
/// rest).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// Which part.
    pub kind: SectionKind,
    /// Its text, trimmed, blank lines dropped. Untrusted: render as text.
    pub lines: Vec<String>,
}

/// The non-empty sections of `tldr`, in the design's order: Outcome,
/// Changed, Decide, Risks.
#[must_use]
pub fn sections(tldr: &Tldr) -> Vec<Section> {
    let clean = |lines: &mut dyn Iterator<Item = &String>| -> Vec<String> {
        lines
            .map(|line| line.trim())
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect()
    };
    [
        (
            SectionKind::Outcome,
            clean(&mut core::iter::once(&tldr.outcome)),
        ),
        (SectionKind::Changed, clean(&mut tldr.changed.iter())),
        (SectionKind::Decide, clean(&mut tldr.decide.iter())),
        (SectionKind::Risks, clean(&mut tldr.risks.iter())),
    ]
    .into_iter()
    .filter(|(_, lines)| !lines.is_empty())
    .map(|(kind, lines)| Section { kind, lines })
    .collect()
}

/// The summary as one text to read aloud: each section's heading, then its
/// lines as sentences.
#[must_use]
pub fn speech(sections: &[Section]) -> String {
    sections
        .iter()
        .map(|section| {
            let lines: Vec<String> = section.lines.iter().map(|l| sentence(l)).collect();
            format!("{}: {}", section.kind.label(), lines.join(" "))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `line` ending in sentence punctuation, so a voice pauses after it.
fn sentence(line: &str) -> String {
    if line.ends_with(['.', '!', '?']) {
        line.to_owned()
    } else {
        format!("{line}.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tldr() -> Tldr {
        Tldr {
            outcome: "Refactored the parser; tests pass.".into(),
            changed: vec![
                "parser.rs (+40 −12)".into(),
                "  ".into(),
                " lexer.rs ".into(),
            ],
            decide: Some("Merge now or wait for review?".into()),
            risks: vec!["No benchmark run".into()],
        }
    }

    fn snapshot(at: u64) -> Snapshot<Tldr> {
        Snapshot {
            fetched_at: Timestamp::from_unix_millis(at),
            value: tldr(),
        }
    }

    #[test]
    fn sections_follow_the_design_order_and_drop_blanks() {
        let got = sections(&tldr());
        let kinds: Vec<SectionKind> = got.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            [
                SectionKind::Outcome,
                SectionKind::Changed,
                SectionKind::Decide,
                SectionKind::Risks
            ]
        );
        assert_eq!(got[1].lines, ["parser.rs (+40 −12)", "lexer.rs"]);

        let bare = Tldr {
            outcome: "It worked.".into(),
            changed: Vec::new(),
            decide: Some("   ".into()),
            risks: Vec::new(),
        };
        let got = sections(&bare);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, SectionKind::Outcome);
    }

    #[test]
    fn speech_reads_each_heading_and_line() {
        assert_eq!(
            speech(&sections(&tldr())),
            "Outcome: Refactored the parser; tests pass. \
             Changed: parser.rs (+40 −12). lexer.rs. \
             Decide: Merge now or wait for review? \
             Risks: No benchmark run."
        );
    }

    #[test]
    fn a_cached_tldr_renders_offline_with_its_age() {
        let state = CardState::Unknown.with_cached(Some(snapshot(1_000)));
        // Offline: the read fails and the cached card stays.
        let (state, op) = state.with_read(
            Err(ApiError::Transport("offline".into())),
            Timestamp::from_unix_millis(2_000),
        );
        assert_eq!(op, CacheOp::Keep);
        let Some(Card::Summary { sections, .. }) = state.card(false) else {
            panic!("a summary card");
        };
        assert_eq!(state.as_of(), Some(Timestamp::from_unix_millis(1_000)));
        assert_eq!(sections[0].lines, ["Refactored the parser; tests pass."]);
    }

    #[test]
    fn a_fresh_read_is_cached_and_shown_without_an_age() {
        let (state, op) = CardState::Unknown
            .with_cached(Some(snapshot(1_000)))
            .with_read(
                Ok(TldrRead::Ready(tldr())),
                Timestamp::from_unix_millis(2_000),
            );
        assert_eq!(op, CacheOp::Put(snapshot(2_000)));
        assert_eq!(state, CardState::Fresh(snapshot(2_000)));
        assert_eq!(state.as_of(), None);
        // Confirming the cached summary keeps the same card: only its age
        // goes, so the view need not rebuild it.
        let cached = CardState::Unknown.with_cached(Some(snapshot(1_000)));
        assert_eq!(state.card(false), cached.card(false));
        // A cached copy loaded late never replaces the bridge's answer.
        assert_eq!(state.clone().with_cached(Some(snapshot(1))), state);
    }

    #[test]
    fn summarizing_and_none_replace_the_card() {
        let cached = CardState::Unknown.with_cached(Some(snapshot(1_000)));
        let (summarizing, op) =
            cached.with_read(Ok(TldrRead::Generating), Timestamp::from_unix_millis(2_000));
        assert_eq!(op, CacheOp::Keep);
        assert!(summarizing.is_summarizing());
        assert_eq!(summarizing.card(false), Some(Card::Summarizing));

        let (empty, op) =
            summarizing.with_read(Ok(TldrRead::None), Timestamp::from_unix_millis(3_000));
        assert_eq!(op, CacheOp::Delete);
        assert_eq!(empty.card(false), None);
        assert!(!empty.is_pending());
        assert_eq!(CardState::Unknown.card(false), None);
        assert!(CardState::Unknown.is_pending() && summarizing.is_pending());

        // Asked for while another turn is made: a note, asked again later.
        let (busy, op) = CardState::Unknown.with_read(
            Err(ApiError::Status { status: 429 }),
            Timestamp::from_unix_millis(4_000),
        );
        assert_eq!(op, CacheOp::Keep);
        assert_eq!(busy.card(false), Some(Card::Busy));
        assert!(!busy.is_summarizing(), "no timed retry while busy");
        assert_eq!(
            empty.card(true),
            Some(Card::Unavailable),
            "said, when asked"
        );
        assert!(busy.is_pending());
        let (fresh, _) = busy.with_read(
            Ok(TldrRead::Ready(tldr())),
            Timestamp::from_unix_millis(5_000),
        );
        assert!(!fresh.is_pending());
        assert_eq!(CardState::Unknown.with_cached(None), CardState::Unknown);
    }

    #[test]
    fn the_cache_key_is_per_agent_and_apart_from_turn_keys() {
        let agent: AgentId = "w6:p1~abc".parse().expect("agent id");
        assert_eq!(LatestTldrKey(agent).encode(), "tldr/w6:p1~abc/latest");
    }

    #[test]
    fn only_a_ready_agents_seq_asks_again() {
        let (idle, done) = (Some(AgentStatus::Idle), Some(AgentStatus::Done));
        let working = Some(AgentStatus::Working);
        let blocked = Some(AgentStatus::Blocked);
        let mut at = None;
        for (status, seq, expect) in [
            (idle, 3, Some(3)),
            // A new prompt, then a long turn of bumps: nothing to read.
            (working, 4, Some(3)),
            (working, 9, Some(3)),
            (blocked, 10, Some(3)),
            (working, 11, Some(3)),
            // The turn finished.
            (done, 12, Some(12)),
            // Unknown (not listed): keep what was.
            (None, 13, Some(12)),
        ] {
            at = finished_seq(at, status, Some(Seq(seq)));
            assert_eq!(at, expect.map(Seq), "{status:?} {seq}");
        }
    }
}
