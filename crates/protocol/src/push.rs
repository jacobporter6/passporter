//! Web Push subscription management (`/v1/push/*`, design §6.6). Outbound:
//! tolerant.
//!
//! The subscription itself (`POST /v1/push/subscriptions`) is the browser's
//! `PushSubscription.toJSON()`, which the bridge validates on its side, plus
//! an optional `"payload"` ([`PushPayload`]).
//!
//! # What a push message says
//!
//! Every message is encrypted end to end to the subscription (RFC 8291), so
//! the push service only ever sees ciphertext. Inside is a [`PushMessage`]:
//!
//! - **`ping`** subscriptions (the default; the PWA's Web Push) get only
//!   `{"ping": "<32 hex>"}` and fetch the content over the authenticated
//!   API when they wake.
//! - **`notice`** subscriptions (the native app's `UnifiedPush` endpoint)
//!   also get `notices`: per agent, its name, state and one line, enough to
//!   post a notification and for Android Auto to read it aloud. Doze often
//!   blocks the network fetch after a wake, so the content has to travel in
//!   the message itself.

use serde::{Deserialize, Serialize};

use crate::ids::{AgentId, PendingId};

/// Most notices in one [`PushMessage`]; the most recent changes win.
pub const MAX_NOTICES: usize = 4;

/// What a subscription asks to receive. Inbound (part of the subscribe
/// body), so strict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushPayload {
    /// A content-free wake ping: the PWA (Web Push).
    #[default]
    Ping,
    /// The ping plus short encrypted [`Notice`]s: the native app.
    Notice,
}

impl PushPayload {
    /// True for [`PushPayload::Ping`], the default left off the wire.
    #[must_use]
    pub const fn is_ping(&self) -> bool {
        matches!(self, Self::Ping)
    }
}

/// The plaintext of a push message. Outbound: tolerant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushMessage {
    /// 128 random bits as hex, fresh per message: meaningless, but lets a
    /// client drop a duplicate.
    pub ping: String,
    /// What changed, for [`PushPayload::Notice`] subscriptions only; never
    /// present for a ping subscription. At most [`MAX_NOTICES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<Notice>,
}

/// One agent's news, short enough to read aloud. Every text is untrusted
/// agent output, cleaned of control and invisible characters and bounded by
/// the bridge: display it as plain text, never as markup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notice {
    /// Which agent; the conversation to post into.
    pub agent_id: AgentId,
    /// The agent's name (its card title).
    pub name: String,
    /// Why it is news.
    pub state: NoticeState,
    /// One line: what a blocked agent asks, or a finished turn's headline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
    /// The finished turn's TL;DR *Decide* line: what it needs from you.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decide: Option<String>,
    /// The blocked agent's pending input, to send back with a car reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_id: Option<PendingId>,
    /// Whether Android Auto should announce it. Blocked agents always are;
    /// finished ones only as the bridge's announcement policy says (by
    /// default only once their TL;DR has a *Decide* line). Absent: false.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub announce: bool,
}

/// Why a [`Notice`] was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeState {
    /// The agent is at a dialog and waits for an answer.
    Blocked,
    /// The agent finished its turn.
    Finished,
    /// A state this app predates.
    #[serde(other)]
    Unknown,
}

/// `GET /v1/push/key`: the bridge's VAPID public key, the PWA's
/// `applicationServerKey`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushKey {
    /// The uncompressed P-256 public key, base64url without padding.
    pub vapid_public_key: String,
}

/// `GET /v1/push/subscriptions`: what the bridge holds for this device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushSubscriptions {
    /// One entry per subscription.
    pub subscriptions: Vec<PushEndpoint>,
}

/// A subscription as the API shows it: never its keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushEndpoint {
    /// The push service URL.
    pub endpoint: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shapes() {
        let key = PushKey {
            vapid_public_key: "BAAA".into(),
        };
        assert_eq!(
            serde_json::to_value(&key).unwrap(),
            serde_json::json!({ "vapidPublicKey": "BAAA" })
        );
        let list: PushSubscriptions =
            serde_json::from_str(r#"{"subscriptions":[{"endpoint":"https://e/1"}]}"#).unwrap();
        assert_eq!(list.subscriptions[0].endpoint, "https://e/1");
    }

    #[test]
    fn a_ping_is_only_a_ping_and_notices_are_tolerant() {
        let ping = PushMessage {
            ping: "ab".into(),
            notices: Vec::new(),
        };
        assert_eq!(
            serde_json::to_value(&ping).unwrap(),
            serde_json::json!({ "ping": "ab" })
        );
        let message: PushMessage = serde_json::from_str(
            r#"{"ping":"ab","extra":1,"notices":[{"agentId":"w1:p1","name":"n",
                "state":"thinking","announce":true,"mood":"?"}]}"#,
        )
        .unwrap();
        assert_eq!(message.notices[0].state, NoticeState::Unknown);
        assert!(message.notices[0].announce);
        assert_eq!(PushPayload::default(), PushPayload::Ping);
        assert!(serde_json::from_str::<PushPayload>(r#""content""#).is_err());
    }
}
