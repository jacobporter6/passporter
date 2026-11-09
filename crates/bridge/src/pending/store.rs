//! Recent permission requests by Claude session id, and the choice of the
//! one that describes the dialog on screen.
//!
//! A session can have several requests in flight (parallel tool calls,
//! subagents), and the order they reach the screen is not the order they
//! arrive. So the store keeps the last [`MAX_PER_SESSION`] of each session,
//! newest first, and [`HookStore::describe`] picks the one that
//! [describes](HookPermission::describes) the dialog: an exact match, or
//! else the only prefix match (when the parser cut the screen's text). An
//! old request therefore cannot describe another dialog, and nothing is
//! cleared on a turn ending.
//!
//! The chosen request is remembered for that dialog's pending id, so its
//! wording stays put while the dialog is open even if newer requests
//! evict it from the list. Memory is bounded by the per-session and
//! session caps.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard, PoisonError};

use protocol::{PendingId, SessionId};

use super::hook::{HookPermission, Match, Shown};

/// Requests kept per session.
const MAX_PER_SESSION: usize = 8;
/// Sessions kept; the least recently updated is evicted beyond it.
const MAX_SESSIONS: usize = 256;

/// The recent permission requests of each session.
#[derive(Debug, Default)]
pub(crate) struct HookStore {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    sessions: HashMap<SessionId, Session>,
    /// Update order, for eviction.
    serial: u64,
}

#[derive(Debug, Default)]
struct Session {
    /// Newest first.
    requests: VecDeque<HookPermission>,
    /// The request last chosen, and the pending id of the dialog it
    /// described.
    chosen: Option<(PendingId, HookPermission)>,
    updated: u64,
}

impl HookStore {
    /// Records a request for `session`.
    pub(crate) fn record(&self, session: SessionId, permission: HookPermission) {
        let mut state = self.lock();
        let State { sessions, serial } = &mut *state;
        *serial += 1;
        if !sessions.contains_key(&session) && sessions.len() >= MAX_SESSIONS {
            let oldest = sessions
                .iter()
                .min_by_key(|(_, entry)| entry.updated)
                .map(|(session, _)| session.clone());
            if let Some(oldest) = oldest {
                sessions.remove(&oldest);
            }
        }
        let entry = sessions.entry(session).or_default();
        entry.updated = *serial;
        entry.requests.retain(|request| request != &permission);
        entry.requests.push_front(permission);
        entry.requests.truncate(MAX_PER_SESSION);
    }

    /// The request of `session` that describes the dialog `id`, which
    /// shows `shown`, and how: the newest exact match, else the only
    /// prefix match, else the one chosen for `id` before. Only an exact
    /// match vouches for the whole dialog; the chosen-before fallback (a
    /// stored request is never cleared once answered) counts as a prefix
    /// match, because it is not checked against the screen again.
    pub(crate) fn describe(
        &self,
        session: &SessionId,
        id: &PendingId,
        shown: &Shown,
    ) -> Option<(HookPermission, Match)> {
        let mut state = self.lock();
        let entry = state.sessions.get_mut(session)?;
        let mut exact = None;
        let mut prefixes = Vec::new();
        for request in &entry.requests {
            match request.describes(shown) {
                Some(Match::Exact) => {
                    exact = Some(request);
                    break;
                }
                Some(Match::Prefix) => prefixes.push(request),
                None => {}
            }
        }
        let found = exact
            .map(|request| (request, Match::Exact))
            .or(match prefixes.as_slice() {
                [only] => Some((*only, Match::Prefix)),
                _ => None,
            });
        if let Some((found, matched)) = found {
            let found = found.clone();
            entry.chosen = Some((id.clone(), found.clone()));
            return Some((found, matched));
        }
        entry
            .chosen
            .as_ref()
            .filter(|(chosen_for, _)| chosen_for == id)
            .map(|(_, request)| (request.clone(), Match::Prefix))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use protocol::{OptionId, PendingInput, PendingKind, PendingOption, PendingOptions, Risk};

    use super::*;
    use crate::pending::hook::{HookEventKind, decode};

    fn session(name: &str) -> SessionId {
        SessionId::new(name).expect("valid")
    }

    fn id(name: &str) -> PendingId {
        PendingId::new(name).expect("valid")
    }

    fn permission(command: &str) -> HookPermission {
        let payload = serde_json::json!({
            "session_id": "s", "hook_event_name": "PermissionRequest",
            "tool_name": "Bash", "tool_input": {"command": command},
        });
        match decode(payload.to_string().as_bytes())
            .expect("decodes")
            .kind
        {
            HookEventKind::Permission(permission) => permission,
            HookEventKind::Ignored => unreachable!(),
        }
    }

    /// A parsed Bash dialog showing `detail`.
    fn shown(detail: &str) -> Shown {
        let yes = PendingOption {
            id: OptionId::new("yes").expect("id"),
            label: "Yes".to_owned(),
            risk: Risk::Low,
            accepts_feedback: false,
        };
        Shown::new(&PendingInput {
            id: id("screen-1-000000000000"),
            kind: PendingKind::Permission,
            prompt: "Do you want to proceed?".to_owned(),
            detail: Some(detail.to_owned()),
            options: PendingOptions::new(vec![yes]).expect("options"),
            suggestions: Vec::new(),
            source: protocol::PendingSource::Unknown,
        })
    }

    /// How the stored request that describes `dialog` (showing `detail`)
    /// matched, if one does.
    fn matched(store: &HookStore, name: &str, dialog: &str, detail: &str) -> Option<Match> {
        store
            .describe(&session(name), &id(dialog), &shown(detail))
            .map(|(_, matched)| matched)
    }

    fn described(store: &HookStore, name: &str, dialog: &str, detail: &str) -> Option<String> {
        store
            .describe(&session(name), &id(dialog), &shown(detail))
            .map(|(request, _)| request.prompt)
    }

    #[test]
    fn the_matching_request_of_the_session_is_chosen() {
        let store = HookStore::default();
        store.record(session("a"), permission("ls"));
        store.record(session("a"), permission("pwd"));
        store.record(session("b"), permission("id"));
        assert_eq!(
            described(&store, "a", "d1", "ls").as_deref(),
            Some("Run `ls`?")
        );
        assert_eq!(
            described(&store, "a", "d2", "pwd").as_deref(),
            Some("Run `pwd`?")
        );
        assert_eq!(
            described(&store, "a", "d3", "id"),
            None,
            "another session's"
        );
        assert_eq!(described(&store, "c", "d4", "ls"), None);
    }

    #[test]
    fn a_prefix_match_must_be_the_only_one() {
        let store = HookStore::default();
        let long_a = format!("echo {}a", "x ".repeat(1500));
        let cut = format!("{}…", &long_a[..1999]);
        store.record(session("s"), permission(&long_a));
        assert!(described(&store, "s", "d1", &cut).is_some());
        assert_eq!(
            matched(&store, "s", "d1", &cut),
            Some(Match::Prefix),
            "a cut screen proves only the start"
        );
        let long_b = format!("echo {}b", "x ".repeat(1500));
        store.record(session("s"), permission(&long_b));
        assert_eq!(
            described(&store, "s", "d2", &cut),
            None,
            "two requests fit the cut text"
        );
    }

    #[test]
    fn the_chosen_request_survives_eviction_for_its_dialog_only() {
        let store = HookStore::default();
        store.record(session("s"), permission("ls"));
        assert_eq!(matched(&store, "s", "d1", "ls"), Some(Match::Exact));
        for i in 0..MAX_PER_SESSION {
            store.record(session("s"), permission(&format!("echo {i}")));
        }
        assert_eq!(matched(&store, "s", "d1", "ls"), Some(Match::Prefix));
        assert_eq!(
            described(&store, "s", "d1", "ls").as_deref(),
            Some("Run `ls`?")
        );
        assert_eq!(described(&store, "s", "d2", "ls"), None, "another dialog");
    }

    #[test]
    fn a_repeated_request_is_kept_once_and_the_list_is_capped() {
        let store = HookStore::default();
        for i in 0..20 {
            store.record(session("a"), permission(&format!("echo {i}")));
        }
        store.record(session("a"), permission("echo 15"));
        let state = store.lock();
        let requests = &state.sessions[&session("a")].requests;
        assert_eq!(requests.len(), MAX_PER_SESSION);
        assert_eq!(requests[0].prompt, "Run `echo 15`?");
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.prompt == "Run `echo 15`?")
                .count(),
            1
        );
    }

    #[test]
    fn the_least_recent_session_is_evicted() {
        let store = HookStore::default();
        for i in 0..=MAX_SESSIONS {
            store.record(session(&format!("s{i}")), permission("ls"));
        }
        assert_eq!(described(&store, "s0", "d", "ls"), None);
        assert!(described(&store, &format!("s{MAX_SESSIONS}"), "d", "ls").is_some());
    }
}
