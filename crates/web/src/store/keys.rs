//! Typed keys for the local store. Each key type fixes its object store and
//! its value type, so a snapshot can't be read back as the wrong thing.
//!
//! Encodings are plain strings. [`AgentId`] never contains `/`, so the
//! snapshot keys are unambiguous; outbox keys sort chronologically as
//! strings, which is `IndexedDB` key order.

use core::fmt;

use protocol::{
    Action, Agent, AgentId, IdempotencyKey, PendingInput, ThreadItem, Timestamp, Tldr, TurnIndex,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// An object store of the app's database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreName {
    /// The read cache: last-known agents, threads, pending inputs, TL;DRs
    /// (design §6.5). Safe to clear.
    Snapshots,
    /// Durable user decisions and their delivery state (design §6.5, see
    /// [`crate::outbox`]). Never cleared wholesale: it holds things the user
    /// said.
    Outbox,
    /// Small bookkeeping values, e.g. the pasted token.
    Meta,
}

impl StoreName {
    /// Every store, for schema creation.
    pub const ALL: [Self; 3] = [Self::Snapshots, Self::Outbox, Self::Meta];

    /// The `IndexedDB` object store name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Snapshots => "snapshots",
            Self::Outbox => "outbox",
            Self::Meta => "meta",
        }
    }
}

/// A key with a fixed store and value type.
pub trait StoreKey {
    /// What is stored under the key.
    type Value: Serialize + DeserializeOwned;
    /// Which object store holds it.
    const STORE: StoreName;
    /// The key as stored.
    fn encode(&self) -> String;
}

/// A cached server value and when it was fetched, for "as of 14:02".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot<T> {
    /// When the value was fetched (device clock).
    pub fetched_at: Timestamp,
    /// The value.
    pub value: T,
}

/// The agent list (`GET /v1/agents`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentsKey;

impl StoreKey for AgentsKey {
    type Value = Snapshot<Vec<Agent>>;
    const STORE: StoreName = StoreName::Snapshots;
    fn encode(&self) -> String {
        "agents".to_owned()
    }
}

/// The bridge bearer token pasted on the Connect screen (MVP auth, until
/// device keys). Stored as the raw string and validated on load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenKey;

impl StoreKey for TokenKey {
    type Value = String;
    const STORE: StoreName = StoreName::Meta;
    fn encode(&self) -> String {
        "bridgeToken".to_owned()
    }
}

/// One agent's thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadKey(pub AgentId);

impl StoreKey for ThreadKey {
    type Value = Snapshot<Vec<ThreadItem>>;
    const STORE: StoreName = StoreName::Snapshots;
    fn encode(&self) -> String {
        format!("thread/{}", self.0)
    }
}

/// What one agent is asking, if anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingKey(pub AgentId);

impl StoreKey for PendingKey {
    type Value = Snapshot<Option<PendingInput>>;
    const STORE: StoreName = StoreName::Snapshots;
    fn encode(&self) -> String {
        format!("pending/{}", self.0)
    }
}

/// The TL;DR of one turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TldrKey(pub AgentId, pub TurnIndex);

impl StoreKey for TldrKey {
    type Value = Snapshot<Tldr>;
    const STORE: StoreName = StoreName::Snapshots;
    fn encode(&self) -> String {
        format!("tldr/{}/{}", self.0, self.1.0)
    }
}

/// One outbox entry: when the user decided, a per-device sequence number,
/// then the idempotency key.
///
/// Encoded as `<created_at>~<seq>~<idempotency key>`, both numbers 20
/// zero-padded digits, so string order (the `IndexedDB` key order) is
/// creation order and the outbox flushes FIFO with a plain key scan. The
/// sequence strictly increases within a tab (see
/// [`crate::outbox::service`]), so entries made in the same millisecond
/// keep the order they were made in; the idempotency key keeps keys from
/// different tabs apart.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct OutboxKey {
    /// When the user decided.
    pub created_at: Timestamp,
    /// Per-device sequence number.
    pub seq: u64,
    /// The action's idempotency key.
    pub idempotency_key: IdempotencyKey,
}

impl OutboxKey {
    /// The key for `action`, with a freshly allocated `seq`.
    #[must_use]
    pub fn for_action(action: &Action, seq: u64) -> Self {
        Self {
            created_at: action.created_at,
            seq,
            idempotency_key: action.idempotency_key.clone(),
        }
    }

    /// Parses an encoded key; `None` if it is not one this app wrote.
    #[must_use]
    pub fn decode(encoded: &str) -> Option<Self> {
        let mut parts = encoded.splitn(3, '~');
        let (millis, seq, key) = (parts.next()?, parts.next()?, parts.next()?);
        Some(Self {
            created_at: Timestamp::from_unix_millis(padded_number(millis)?),
            seq: padded_number(seq)?,
            idempotency_key: IdempotencyKey::new(key).ok()?,
        })
    }
}

/// Parses exactly 20 ASCII digits.
fn padded_number(digits: &str) -> Option<u64> {
    if digits.len() == 20 && digits.bytes().all(|b| b.is_ascii_digit()) {
        digits.parse().ok()
    } else {
        None
    }
}

impl fmt::Display for OutboxKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:020}~{:020}~{}",
            self.created_at.unix_millis(),
            self.seq,
            self.idempotency_key
        )
    }
}

impl StoreKey for OutboxKey {
    /// The entry as a [`crate::outbox::record`].
    type Value = String;
    const STORE: StoreName = StoreName::Outbox;
    fn encode(&self) -> String {
        self.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const K1: &str = "9ca09978-ef89-46fd-9a85-118b84bee7b4";
    const K2: &str = "0ca09978-ef89-46fd-9a85-118b84bee7b4";

    fn outbox(millis: u64, seq: u64, key: &str) -> OutboxKey {
        OutboxKey {
            created_at: Timestamp::from_unix_millis(millis),
            seq,
            idempotency_key: IdempotencyKey::new(key).unwrap(),
        }
    }

    fn agent() -> AgentId {
        "w6:p1~abc".parse().unwrap()
    }

    #[test]
    fn snapshot_keys_are_distinct_and_stable() {
        let keys = [
            AgentsKey.encode(),
            ThreadKey(agent()).encode(),
            PendingKey(agent()).encode(),
            TldrKey(agent(), TurnIndex(2)).encode(),
        ];
        assert_eq!(
            keys,
            [
                "agents",
                "thread/w6:p1~abc",
                "pending/w6:p1~abc",
                "tldr/w6:p1~abc/2"
            ]
        );
    }

    #[test]
    fn outbox_key_round_trips() {
        let key = outbox(1_762_500_000_000, 7, K1);
        let encoded = key.encode();
        assert_eq!(
            encoded,
            format!("00000001762500000000~00000000000000000007~{K1}")
        );
        assert_eq!(OutboxKey::decode(&encoded), Some(key));
        let max = outbox(u64::MAX, u64::MAX, K1);
        assert_eq!(OutboxKey::decode(&max.encode()), Some(max));
    }

    #[test]
    fn outbox_string_order_is_creation_order() {
        let mut keys = [
            outbox(10, 3, K1),
            outbox(9, 1, K1),
            outbox(1_000, 4, K2),
            // Same millisecond: the sequence decides, not the random key.
            outbox(10, 2, K1),
        ];
        let mut encoded: Vec<String> = keys.iter().map(StoreKey::encode).collect();
        encoded.sort();
        keys.sort();
        let decoded: Vec<OutboxKey> = encoded
            .iter()
            .map(|e| OutboxKey::decode(e).unwrap())
            .collect();
        assert_eq!(decoded, keys);
        let seqs: Vec<u64> = decoded.iter().map(|k| k.seq).collect();
        assert_eq!(seqs, [1, 2, 3, 4]);
    }

    #[test]
    fn foreign_outbox_keys_are_rejected() {
        let zero = "00000000000000000000";
        for bad in [
            String::new(),
            "agents".into(),
            format!("1~1~{K1}"),
            format!("{zero}~{K1}"),
            format!("0000000000000000000x~{zero}~{K1}"),
            format!("{zero}~{zero}~NOT-A-UUID"),
            format!("{zero}~{zero}~{K1}~extra"),
        ] {
            assert_eq!(OutboxKey::decode(&bad), None, "{bad:?}");
        }
    }

    #[test]
    fn snapshot_values_round_trip_as_json() {
        let snap = Snapshot {
            fetched_at: Timestamp::from_unix_millis(5),
            value: Vec::<Agent>::new(),
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert_eq!(json, r#"{"fetchedAt":5,"value":[]}"#);
        assert_eq!(
            serde_json::from_str::<<AgentsKey as StoreKey>::Value>(&json).unwrap(),
            snap
        );
    }
}
