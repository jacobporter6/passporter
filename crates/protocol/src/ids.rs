//! Identifier newtypes. Each one validates its format on construction and on
//! deserialization, so a value in hand is always well formed.

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::error::ValidationError;
use crate::macros::validated_string;

/// Maximum length, in bytes, of a [`PaneId`], [`SessionId`] or [`OptionId`].
pub const MAX_ID_LEN: usize = 128;

/// Separator between the pane and session parts of an [`AgentId`]. It is
/// URL-path safe and outside the identifier charset, so splitting is unambiguous.
pub const AGENT_ID_SEPARATOR: char = '~';

/// Checks the shared identifier charset: 1..=[`MAX_ID_LEN`] bytes of ASCII
/// letters, digits, `:`, `.`, `_` and `-`, and not made of dots alone (so
/// never `.` or `..` in a URL path). This covers herdr pane ids (`w6:p1`) and
/// session UUIDs, and is safe in a URL path segment.
fn check_ident(field: &'static str, value: &str) -> Result<(), ValidationError> {
    if value.is_empty() {
        return Err(ValidationError::Empty { field });
    }
    if value.len() > MAX_ID_LEN {
        return Err(ValidationError::TooLong {
            field,
            max: MAX_ID_LEN,
            actual: value.len(),
        });
    }
    let charset_ok = value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'.' | b'_' | b'-'));
    let only_dots = value.bytes().all(|b| b == b'.');
    if charset_ok && !only_dots {
        Ok(())
    } else {
        Err(ValidationError::InvalidCharacter { field })
    }
}

/// Checks a canonical lowercase hyphenated UUID (`8-4-4-4-12` hex digits).
/// One spelling only, so idempotency dedupe is plain string equality.
fn check_uuid(field: &'static str, value: &str) -> Result<(), ValidationError> {
    let bytes = value.as_bytes();
    let ok = bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, &b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
        });
    if ok {
        Ok(())
    } else {
        Err(ValidationError::InvalidUuid { field })
    }
}

validated_string!(
    /// A herdr pane id, e.g. `w6:p1`.
    PaneId,
    |v| check_ident("paneId", v)
);

validated_string!(
    /// An agent session id as reported by herdr (`agent_session.value`); for
    /// Claude this is the transcript's session UUID.
    ///
    /// Only herdr sessions with `kind == "id"` whose value fits the identifier
    /// charset are carried. Any other session (a path, a value with `/`, space
    /// or `~`, another kind) yields a session-less [`AgentId`]; the stale
    /// guard then relies on `expectedSeq` (and `pendingId`) alone.
    SessionId,
    |v| check_ident("sessionId", v)
);

validated_string!(
    /// The id of one option of a [`PendingInput`](crate::PendingInput),
    /// echoed back by a `choose` action. Unique within its pending input.
    OptionId,
    |v| check_ident("optionId", v)
);

validated_string!(
    /// Bridge-assigned id of one [`PendingInput`](crate::PendingInput)
    /// (e.g. the hook's `tool_use_id`, or a counter). Opaque to clients.
    ///
    /// A dialog can be replaced by another without herdr's `state_change_seq`
    /// moving, so `choose` (and optionally `keys`) echo the id they answer;
    /// the stale guard compares it as well as `expectedSeq`, and a mismatch
    /// is a 409 stale.
    PendingId,
    |v| check_ident("pendingId", v)
);

validated_string!(
    /// A client-generated UUID that makes an [`Action`](crate::Action)
    /// idempotent: replaying it returns the original result and never sends
    /// twice (design §5). Must be lowercase, as `crypto.randomUUID()` produces.
    IdempotencyKey,
    |v| check_uuid("idempotencyKey", v)
);

validated_string!(
    /// The id of an uploaded audio clip (`POST /v1/audio`), a lowercase UUID.
    AudioId,
    |v| check_uuid("audioId", v)
);

validated_string!(
    /// The id of an uploaded image (`POST /v1/attachments`), a lowercase
    /// UUID the phone picks: it is also the upload's idempotency key.
    AttachmentId,
    |v| check_uuid("attachmentId", v)
);

/// Identifies one agent: a pane, plus the agent session running in it when
/// herdr reports one. Both must still match when an action executes, so an
/// action recorded for one session can never land in a successor session in
/// the same pane.
///
/// On the wire (and in URL paths) it is the string `<pane>~<session>`, e.g.
/// `w6:p1~9ca09978-ef89-46fd-9a85-118b84bee7b4`, or just `<pane>` for a pane
/// with no session (non-Claude agents, a Claude pane still starting up).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AgentId {
    pane: PaneId,
    session: Option<SessionId>,
}

impl AgentId {
    /// Builds an id from its validated parts.
    #[must_use]
    pub fn new(pane: PaneId, session: Option<SessionId>) -> Self {
        Self { pane, session }
    }

    /// The pane part.
    #[must_use]
    pub fn pane(&self) -> &PaneId {
        &self.pane
    }

    /// The agent-session part, if the pane has one.
    #[must_use]
    pub fn session(&self) -> Option<&SessionId> {
        self.session.as_ref()
    }
}

impl TryFrom<String> for AgentId {
    type Error = ValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl core::str::FromStr for AgentId {
    type Err = ValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.split_once(AGENT_ID_SEPARATOR) {
            Some((pane, session)) => Ok(Self::new(
                PaneId::new(pane)?,
                Some(SessionId::new(session)?),
            )),
            None => Ok(Self::new(PaneId::new(value)?, None)),
        }
    }
}

impl From<AgentId> for String {
    fn from(value: AgentId) -> Self {
        value.to_string()
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.session {
            Some(session) => write!(f, "{}{AGENT_ID_SEPARATOR}{session}", self.pane),
            None => write!(f, "{}", self.pane),
        }
    }
}

/// herdr's `state_change_seq` for an agent. An action carries the value the
/// user saw when deciding; the bridge refuses it as stale if it has moved on.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Seq(pub u64);

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "9ca09978-ef89-46fd-9a85-118b84bee7b4";

    #[test]
    fn ident_accepts_herdr_shapes() {
        assert_eq!(PaneId::new("w6:p1").unwrap().as_str(), "w6:p1");
        assert!(SessionId::new(UUID).is_ok());
        assert!(OptionId::new("allow_once").is_ok());
        assert!(OptionId::new("a.b").is_ok());
        assert!(OptionId::new(".hidden").is_ok());
        assert!(OptionId::new("a".repeat(MAX_ID_LEN)).is_ok());
    }

    #[test]
    fn ident_rejects_empty() {
        assert_eq!(
            PaneId::new(""),
            Err(ValidationError::Empty { field: "paneId" })
        );
    }

    #[test]
    fn ident_rejects_too_long() {
        assert_eq!(
            OptionId::new("a".repeat(MAX_ID_LEN + 1)),
            Err(ValidationError::TooLong {
                field: "optionId",
                max: MAX_ID_LEN,
                actual: 129
            })
        );
    }

    #[test]
    fn ident_rejects_bad_characters() {
        for bad in ["a b", "a/b", "a~b", "é", "a\nb", "../x", "a%2f"] {
            assert_eq!(
                SessionId::new(bad),
                Err(ValidationError::InvalidCharacter { field: "sessionId" }),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn ident_rejects_dot_only() {
        for bad in [".", "..", "..."] {
            assert_eq!(
                PaneId::new(bad),
                Err(ValidationError::InvalidCharacter { field: "paneId" }),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn uuid_accepts_canonical_lowercase() {
        assert_eq!(IdempotencyKey::new(UUID).unwrap().as_str(), UUID);
        assert!(AudioId::new(UUID).is_ok());
    }

    #[test]
    fn uuid_rejects_other_spellings() {
        for bad in [
            "",
            "not-a-uuid",
            "9CA09978-EF89-46FD-9A85-118B84BEE7B4",
            "9ca09978ef8946fd9a85118b84bee7b4",
            "{9ca09978-ef89-46fd-9a85-118b84bee7b4}",
            "9ca09978-ef89-46fd-9a85-118b84bee7b",
            "9ca09978-ef89-46fd-9a85-118b84bee7b4a",
            "9ca09978-ef89-46fd-9a85_118b84bee7b4",
            "gca09978-ef89-46fd-9a85-118b84bee7b4",
        ] {
            assert_eq!(
                IdempotencyKey::new(bad),
                Err(ValidationError::InvalidUuid {
                    field: "idempotencyKey"
                }),
                "{bad:?}"
            );
        }
        assert_eq!(
            AudioId::new("x"),
            Err(ValidationError::InvalidUuid { field: "audioId" })
        );
    }

    #[test]
    fn agent_id_round_trips_through_string() {
        let raw = format!("w6:p1~{UUID}");
        let id: AgentId = raw.parse().unwrap();
        assert_eq!(id.pane().as_str(), "w6:p1");
        assert_eq!(id.session().map(SessionId::as_str), Some(UUID));
        assert_eq!(id.to_string(), raw);
        assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{raw}\""));
        assert_eq!(
            serde_json::from_str::<AgentId>(&format!("\"{raw}\"")).unwrap(),
            id
        );
    }

    #[test]
    fn agent_id_without_session() {
        let id: AgentId = "w6:p2".parse().unwrap();
        assert_eq!(id.pane().as_str(), "w6:p2");
        assert_eq!(id.session(), None);
        assert_eq!(id.to_string(), "w6:p2");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"w6:p2\"");
        assert_eq!(serde_json::from_str::<AgentId>("\"w6:p2\"").unwrap(), id);
    }

    #[test]
    fn agent_id_rejects_malformed() {
        assert_eq!(
            "".parse::<AgentId>(),
            Err(ValidationError::Empty { field: "paneId" })
        );
        assert_eq!(
            "~abc".parse::<AgentId>(),
            Err(ValidationError::Empty { field: "paneId" })
        );
        assert_eq!(
            "w6:p1~".parse::<AgentId>(),
            Err(ValidationError::Empty { field: "sessionId" })
        );
        assert_eq!(
            "w6:p1~a~b".parse::<AgentId>(),
            Err(ValidationError::InvalidCharacter { field: "sessionId" })
        );
        assert_eq!(
            "w6/p1".parse::<AgentId>(),
            Err(ValidationError::InvalidCharacter { field: "paneId" })
        );
        assert!(serde_json::from_str::<AgentId>("\"..\"").is_err());
    }

    #[test]
    fn deserialization_validates() {
        assert!(serde_json::from_str::<IdempotencyKey>("\"abc\"").is_err());
        assert!(serde_json::from_str::<OptionId>("\"a b\"").is_err());
        assert!(serde_json::from_str::<OptionId>("\"ok\"").is_ok());
        assert!(serde_json::from_str::<PendingId>("\"toolu_01AbC\"").is_ok());
        assert!(serde_json::from_str::<PendingId>("\"..\"").is_err());
        assert_eq!(
            PendingId::new(""),
            Err(ValidationError::Empty { field: "pendingId" })
        );
    }

    #[test]
    fn seq_is_a_bare_number() {
        assert_eq!(serde_json::to_string(&Seq(136)).unwrap(), "136");
        assert_eq!(serde_json::from_str::<Seq>("136").unwrap(), Seq(136));
        assert!(serde_json::from_str::<Seq>("-1").is_err());
        assert_eq!(Seq(7).to_string(), "7");
    }
}
