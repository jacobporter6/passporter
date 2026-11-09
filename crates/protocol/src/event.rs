//! Messages on `WS /v1/stream`. Outbound: tolerant.

use core::fmt;

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::agent::Agent;
use crate::headline::is_false;
use crate::ids::AgentId;
use crate::loose::{
    Slot, Tagged, read_lenient, read_slot, read_tagged, take, take_lenient, take_or_default,
};
use crate::pending::PendingInput;
use crate::runbook::{RunWire, RunbookRun};
use crate::status_line::StatusLine;
use crate::thread::{ThreadCursor, ThreadEpoch, ThreadItem, TurnIndex};
use crate::tldr::Tldr;
use crate::tolerant::Lenient;

/// One event on the live stream, tagged by `"type"`.
///
/// `Deserialize` is hand-written so the event is read in one pass instead
/// of being buffered until the tag is known (see [`crate::loose`]): each
/// field name has the same type in every variant, so a field is decoded by
/// its name. Like the derived decoder, it ignores fields the variant
/// doesn't have, wherever they come.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AgentEvent {
    /// The full agent list; sent first on connect and after a resync.
    Snapshot {
        /// Every agent. Entries this app cannot parse are skipped rather
        /// than failing the whole snapshot.
        agents: Vec<Agent>,
    },
    /// An agent appeared or changed (state, title, unread, ...).
    AgentUpdated {
        /// Its new state. The bridge always sends one; `None` means this app
        /// could not parse it (e.g. a newer id format), and the event degrades
        /// to a hint to refetch `GET /v1/agents`.
        #[serde(skip_serializing_if = "Option::is_none")]
        agent: Option<Agent>,
    },
    /// An agent's pane closed or its session ended.
    AgentRemoved {
        /// Which agent.
        agent_id: AgentId,
    },
    /// Entries of an agent's thread were added or changed. Upsert them by
    /// cursor (see [`crate::thread`]).
    ThreadItems {
        /// Which agent.
        agent_id: AgentId,
        /// The numbering the cursors belong to; on a change, drop the cached
        /// thread and refetch it.
        epoch: ThreadEpoch,
        /// The new or changed entries, in cursor order.
        items: Vec<ThreadItem>,
        /// What to pass as `since` on the next fetch.
        resume_cursor: ThreadCursor,
    },
    /// An agent's pending input appeared, changed or was answered.
    PendingChanged {
        /// Which agent.
        agent_id: AgentId,
        /// The new pending input, or `None` once it is resolved. A value this
        /// app cannot parse also degrades to `None` rather than dropping the
        /// event; the agent's own status (still `blocked`) keeps the keypad
        /// fallback reachable, and the app can refetch
        /// `GET /v1/agents/:id/pending`.
        pending: Option<PendingInput>,
    },
    /// A TL;DR finished generating for a turn.
    TldrReady {
        /// Which agent.
        agent_id: AgentId,
        /// The turn summarized; matches [`ThreadItem::turn`].
        turn: TurnIndex,
        /// The summary.
        tldr: Tldr,
        /// Made on request from the turn so far, while it is in progress:
        /// the turn's TL;DR once it finishes may differ. Absent on the
        /// wire when false.
        #[serde(skip_serializing_if = "is_false")]
        provisional: bool,
    },
    /// A headline finished generating for a turn. The agent's next
    /// `agent_updated` carries it as its card headline.
    HeadlineReady {
        /// Which agent.
        agent_id: AgentId,
        /// The turn; matches [`ThreadItem::turn`].
        turn: TurnIndex,
        /// The headline text (at most
        /// [`MAX_HEADLINE_CHARS`](crate::MAX_HEADLINE_CHARS) characters).
        headline: String,
        /// Made from the turn so far while it is in progress (see
        /// [`TurnHeadline::provisional`](crate::TurnHeadline::provisional)).
        /// Absent on the wire when false.
        #[serde(skip_serializing_if = "is_false")]
        provisional: bool,
    },
    /// A working agent's status line changed. The agent's next
    /// `agent_updated` carries it too.
    StatusLineReady {
        /// Which agent.
        agent_id: AgentId,
        /// The new line.
        status_line: StatusLine,
    },
    /// A runbook run started, printed more output or ended.
    RunbookUpdated {
        /// The run as it is now. The bridge always sends one; `None` means
        /// this app could not parse it, and the event degrades to a hint to
        /// refetch `GET /v1/runbooks`.
        #[serde(skip_serializing_if = "Option::is_none")]
        run: Option<RunbookRun>,
    },
    /// An event this app predates; ignore it.
    Unknown,
}

/// The variant a `type` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Snapshot,
    AgentUpdated,
    AgentRemoved,
    ThreadItems,
    PendingChanged,
    TldrReady,
    HeadlineReady,
    StatusLineReady,
    RunbookUpdated,
    Unknown,
}

impl Kind {
    fn of(tag: &str) -> Self {
        match tag {
            "snapshot" => Self::Snapshot,
            "agent_updated" => Self::AgentUpdated,
            "agent_removed" => Self::AgentRemoved,
            "thread_items" => Self::ThreadItems,
            "pending_changed" => Self::PendingChanged,
            "tldr_ready" => Self::TldrReady,
            "headline_ready" => Self::HeadlineReady,
            "status_line_ready" => Self::StatusLineReady,
            "runbook_updated" => Self::RunbookUpdated,
            _ => Self::Unknown,
        }
    }

    /// Whether the variant has `field`.
    fn has(self, field: Field) -> bool {
        use Field as F;
        match self {
            Self::Snapshot => field == F::Agents,
            Self::AgentUpdated => field == F::Agent,
            Self::AgentRemoved => field == F::AgentId,
            Self::ThreadItems => {
                matches!(field, F::AgentId | F::Epoch | F::Items | F::ResumeCursor)
            }
            Self::PendingChanged => matches!(field, F::AgentId | F::Pending),
            Self::TldrReady => matches!(field, F::AgentId | F::Turn | F::Tldr | F::Provisional),
            Self::HeadlineReady => {
                matches!(field, F::AgentId | F::Turn | F::Headline | F::Provisional)
            }
            Self::StatusLineReady => matches!(field, F::AgentId | F::StatusLine),
            Self::RunbookUpdated => field == F::Run,
            Self::Unknown => false,
        }
    }
}

/// A field some variant has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Agents,
    Agent,
    AgentId,
    Epoch,
    Items,
    ResumeCursor,
    Pending,
    Turn,
    Tldr,
    Headline,
    Provisional,
    StatusLine,
    Run,
}

impl Field {
    fn of(key: &str) -> Option<Self> {
        Some(match key {
            "agents" => Self::Agents,
            "agent" => Self::Agent,
            "agentId" => Self::AgentId,
            "epoch" => Self::Epoch,
            "items" => Self::Items,
            "resumeCursor" => Self::ResumeCursor,
            "pending" => Self::Pending,
            "turn" => Self::Turn,
            "tldr" => Self::Tldr,
            "headline" => Self::Headline,
            "provisional" => Self::Provisional,
            "statusLine" => Self::StatusLine,
            "run" => Self::Run,
            _ => return None,
        })
    }
}

/// Every variant's fields, until the event ends.
#[derive(Default)]
struct Fields {
    agents: Option<Slot<Vec<Lenient<Agent>>>>,
    /// Lenient by design: an unparseable agent (`null` included) is `None`.
    agent: Option<Slot<Lenient<Agent>>>,
    agent_id: Option<Slot<AgentId>>,
    epoch: Option<Slot<ThreadEpoch>>,
    items: Option<Slot<Vec<ThreadItem>>>,
    resume_cursor: Option<Slot<ThreadCursor>>,
    /// Lenient by design, like `agent`.
    pending: Option<Slot<Lenient<PendingInput>>>,
    turn: Option<Slot<TurnIndex>>,
    tldr: Option<Slot<Tldr>>,
    headline: Option<Slot<String>>,
    provisional: Option<Slot<bool>>,
    status_line: Option<Slot<StatusLine>>,
    /// Tolerant by design: a run this app cannot parse is `None`.
    run: Option<Slot<RunWire>>,
}

impl Tagged for Fields {
    type Kind = Kind;
    type Field = Field;
    const TAG: &'static str = "type";

    fn kind(tag: &str) -> Kind {
        Kind::of(tag)
    }

    fn field(key: &str) -> Option<Field> {
        Field::of(key)
    }

    fn has(kind: Kind, field: Field) -> bool {
        kind.has(field)
    }

    fn read<'de, A: MapAccess<'de>>(
        &mut self,
        field: Field,
        map: &mut A,
        tag_known: bool,
    ) -> Result<(), A::Error> {
        match field {
            Field::Agents => read_slot(map, &mut self.agents, "agents", tag_known),
            Field::Agent => read_lenient(map, &mut self.agent, "agent", tag_known),
            Field::AgentId => read_slot(map, &mut self.agent_id, "agentId", tag_known),
            Field::Epoch => read_slot(map, &mut self.epoch, "epoch", tag_known),
            Field::Items => read_slot(map, &mut self.items, "items", tag_known),
            Field::ResumeCursor => {
                read_slot(map, &mut self.resume_cursor, "resumeCursor", tag_known)
            }
            Field::Pending => read_lenient(map, &mut self.pending, "pending", tag_known),
            Field::Turn => read_slot(map, &mut self.turn, "turn", tag_known),
            Field::Tldr => read_slot(map, &mut self.tldr, "tldr", tag_known),
            Field::Headline => read_slot(map, &mut self.headline, "headline", tag_known),
            Field::Provisional => read_slot(map, &mut self.provisional, "provisional", tag_known),
            Field::StatusLine => read_slot(map, &mut self.status_line, "statusLine", tag_known),
            Field::Run => read_slot(map, &mut self.run, "run", tag_known),
        }
    }
}

impl Fields {
    fn into_event<E: serde::de::Error>(self, kind: Kind) -> Result<AgentEvent, E> {
        Ok(match kind {
            Kind::Snapshot => AgentEvent::Snapshot {
                agents: take::<_, E>(self.agents, "agents")?
                    .into_iter()
                    .filter_map(Lenient::into_option)
                    .collect(),
            },
            Kind::AgentUpdated => AgentEvent::AgentUpdated {
                agent: take_lenient(self.agent, "agent")?,
            },
            Kind::AgentRemoved => AgentEvent::AgentRemoved {
                agent_id: take(self.agent_id, "agentId")?,
            },
            Kind::ThreadItems => AgentEvent::ThreadItems {
                agent_id: take(self.agent_id, "agentId")?,
                epoch: take(self.epoch, "epoch")?,
                items: take(self.items, "items")?,
                resume_cursor: take(self.resume_cursor, "resumeCursor")?,
            },
            Kind::PendingChanged => AgentEvent::PendingChanged {
                agent_id: take(self.agent_id, "agentId")?,
                pending: take_lenient(self.pending, "pending")?,
            },
            Kind::TldrReady => AgentEvent::TldrReady {
                agent_id: take(self.agent_id, "agentId")?,
                turn: take(self.turn, "turn")?,
                tldr: take(self.tldr, "tldr")?,
                provisional: take_or_default(self.provisional, "provisional")?,
            },
            Kind::HeadlineReady => AgentEvent::HeadlineReady {
                agent_id: take(self.agent_id, "agentId")?,
                turn: take(self.turn, "turn")?,
                headline: take(self.headline, "headline")?,
                provisional: take_or_default(self.provisional, "provisional")?,
            },
            Kind::StatusLineReady => AgentEvent::StatusLineReady {
                agent_id: take(self.agent_id, "agentId")?,
                status_line: take(self.status_line, "statusLine")?,
            },
            Kind::RunbookUpdated => AgentEvent::RunbookUpdated {
                run: take(self.run, "run")?.into_run(),
            },
            Kind::Unknown => AgentEvent::Unknown,
        })
    }
}

impl<'de> Deserialize<'de> for AgentEvent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(EventVisitor)
    }
}

struct EventVisitor;

impl<'de> Visitor<'de> for EventVisitor {
    type Value = AgentEvent;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a stream event")
    }

    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<AgentEvent, A::Error> {
        let (kind, fields) = read_tagged::<Fields, A>(map)?;
        fields.into_event(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_may_come_in_any_order() {
        let event: AgentEvent = serde_json::from_str(
            r#"{"headline":"Done.","turn":4,"agentId":"w1:p1","type":"headline_ready"}"#,
        )
        .unwrap();
        assert_eq!(
            event,
            AgentEvent::HeadlineReady {
                agent_id: "w1:p1".parse().unwrap(),
                turn: TurnIndex(4),
                headline: "Done.".into(),
                provisional: false,
            }
        );
    }

    #[test]
    fn fields_the_variant_lacks_are_ignored_after_the_tag() {
        let event: AgentEvent = serde_json::from_str(
            r#"{"type":"agent_removed","agentId":"w1:p1","turn":"soon","items":{"a":1}}"#,
        )
        .unwrap();
        assert_eq!(
            event,
            AgentEvent::AgentRemoved {
                agent_id: "w1:p1".parse().unwrap()
            }
        );
        let unknown: AgentEvent =
            serde_json::from_str(r#"{"type":"weather","agents":7,"agentId":[]}"#).unwrap();
        assert_eq!(unknown, AgentEvent::Unknown);
    }

    #[test]
    fn fields_the_variant_lacks_are_ignored_before_the_tag() {
        let event: AgentEvent = serde_json::from_str(
            r#"{"turn":"soon","items":{"a":1},"agentId":"w1:p1","type":"agent_removed"}"#,
        )
        .unwrap();
        assert_eq!(
            event,
            AgentEvent::AgentRemoved {
                agent_id: "w1:p1".parse().unwrap()
            }
        );
        let unknown: AgentEvent =
            serde_json::from_str(r#"{"agents":7,"tldr":[],"type":"weather"}"#).unwrap();
        assert_eq!(unknown, AgentEvent::Unknown);
        // A key repeated before the tag only counts if the variant has it.
        let repeated: AgentEvent =
            serde_json::from_str(r#"{"turn":1,"turn":2,"type":"weather"}"#).unwrap();
        assert_eq!(repeated, AgentEvent::Unknown);
        let repeated_agent: AgentEvent =
            serde_json::from_str(r#"{"agent":null,"agent":null,"type":"weather"}"#).unwrap();
        assert_eq!(repeated_agent, AgentEvent::Unknown);
        assert!(
            serde_json::from_str::<AgentEvent>(
                r#"{"agentId":"w1:p1","agentId":"w1:p2","type":"agent_removed"}"#
            )
            .is_err()
        );
        // A field the variant has must still be valid, wherever it comes.
        let bad = r#"{"turn":"soon","agentId":"w1:p1","headline":"h","type":"headline_ready"}"#;
        assert!(serde_json::from_str::<AgentEvent>(bad).is_err());
    }

    #[test]
    fn known_events_are_checked() {
        let parse = |json: &str| serde_json::from_str::<AgentEvent>(json);
        // Missing tag, missing field, wrong type, duplicate field, not a map.
        assert!(parse(r#"{"agentId":"w1:p1"}"#).is_err());
        assert!(parse(r#"{"type":"agent_removed"}"#).is_err());
        assert!(parse(r#"{"type":"tldr_ready","agentId":"w1:p1","turn":1,"tldr":5}"#).is_err());
        assert!(parse(r#"{"type":"agent_removed","agentId":"w1:p1","agentId":"w1:p2"}"#).is_err());
        assert!(parse(r#"{"type":"snapshot","type":"snapshot","agents":[]}"#).is_err());
        assert!(parse(r#"["snapshot",[]]"#).is_err());
    }

    #[test]
    fn runbook_runs_degrade_to_a_refetch_hint() {
        let event: AgentEvent = serde_json::from_str(
            r#"{"type":"runbook_updated","run":{"runbook":"Deploy","state":"exited",
                "exitCode":0,"lastLines":["ok"],"startedAt":1,"finishedAt":2}}"#,
        )
        .unwrap();
        let AgentEvent::RunbookUpdated { run: Some(run) } = event else {
            panic!("wrong variant: {event:?}");
        };
        assert!(run.succeeded());
        let bad: AgentEvent =
            serde_json::from_str(r#"{"type":"runbook_updated","run":{"runbook":"a/b"}}"#).unwrap();
        assert_eq!(bad, AgentEvent::RunbookUpdated { run: None });
    }

    #[test]
    fn unknown_events_are_tolerated() {
        let event: AgentEvent = serde_json::from_str(r#"{"type":"weather","sunny":true}"#).unwrap();
        assert_eq!(event, AgentEvent::Unknown);
    }

    #[test]
    fn resolved_pending_may_omit_the_field() {
        let event: AgentEvent = serde_json::from_str(
            r#"{"type":"pending_changed","agentId":"w6:p1~abc","pending":null}"#,
        )
        .unwrap();
        let AgentEvent::PendingChanged { pending, .. } = event else {
            panic!("wrong variant: {event:?}");
        };
        assert!(pending.is_none());
        let omitted: AgentEvent =
            serde_json::from_str(r#"{"type":"pending_changed","agentId":"w6:p1~abc"}"#).unwrap();
        assert!(matches!(
            omitted,
            AgentEvent::PendingChanged { pending: None, .. }
        ));
    }

    #[test]
    fn snapshot_skips_agents_it_cannot_parse() {
        let good = r#"{"id":"w1:p1","title":"t","repo":"r","status":"idle","seq":1,
            "statusSince":0,"kind":"pi"}"#;
        let bad_id = r#"{"id":"w1/p2","title":"t","repo":"r","status":"idle","seq":1,
            "statusSince":0,"kind":"pi"}"#;
        let json = format!(r#"{{"type":"snapshot","agents":[{good},{bad_id},{{"junk":1}}]}}"#);
        let AgentEvent::Snapshot { agents } = serde_json::from_str(&json).unwrap() else {
            panic!("wrong variant");
        };
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].id.to_string(), "w1:p1");
    }

    #[test]
    fn unparseable_nested_values_degrade_the_event() {
        let bad_agent = r#"{"type":"agent_updated","agent":{"id":"w1/p2","title":"t",
            "repo":"r","status":"idle","seq":1,"statusSince":0,"kind":"pi"}}"#;
        assert_eq!(
            serde_json::from_str::<AgentEvent>(bad_agent).unwrap(),
            AgentEvent::AgentUpdated { agent: None }
        );
        let bad_pending = r#"{"type":"pending_changed","agentId":"w1:p1","pending":{"id":"p1",
            "kind":"permission","prompt":"?","source":"hook","options":[
            {"id":"a","label":"A","risk":"low"},{"id":"a","label":"B","risk":"low"}]}}"#;
        let AgentEvent::PendingChanged { agent_id, pending } =
            serde_json::from_str(bad_pending).unwrap()
        else {
            panic!("wrong variant");
        };
        assert_eq!(agent_id.to_string(), "w1:p1");
        assert_eq!(pending, None);
    }
}
