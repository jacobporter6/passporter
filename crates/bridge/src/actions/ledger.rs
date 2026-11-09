//! The idempotency ledger: `idempotencyKey` → (body, result), in memory.
//! Shared by `POST /v1/actions` and `POST /v1/agents` ([`crate::spawn`]),
//! each with a ledger of its own over its own body type ([`Idempotent`]).
//!
//! MVP: bounded (oldest results forgotten first) and lost on restart. Task
//! A SQLite ledger is planned. Until then a retry after a bridge restart can
//! execute an action twice; the stale guard catches most such retries,
//! because a delivered prompt or key usually moves `state_change_seq`.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use protocol::{Action, ActionResult, IdempotencyKey};

/// A request body the ledger can dedupe: it carries its key, and its
/// result type says how a replay and a transient result look.
pub(crate) trait Idempotent: Clone + PartialEq + std::fmt::Debug {
    /// What the request produced.
    type Outcome: Clone + std::fmt::Debug;

    /// The client's idempotency key.
    fn idempotency_key(&self) -> &IdempotencyKey;

    /// A stored outcome as returned on replay (e.g. `sent` says nothing
    /// was sent this time).
    fn replayed(outcome: Self::Outcome) -> Self::Outcome;

    /// Whether the outcome leaves the key free for a retry (a transient
    /// refusal: rate limited, step-up required).
    fn is_transient(outcome: &Self::Outcome) -> bool;
}

impl Idempotent for Action {
    type Outcome = ActionResult;

    fn idempotency_key(&self) -> &IdempotencyKey {
        &self.idempotency_key
    }

    fn replayed(outcome: ActionResult) -> ActionResult {
        match outcome {
            ActionResult::Sent { .. } => ActionResult::Sent { replayed: true },
            other => other,
        }
    }

    fn is_transient(outcome: &ActionResult) -> bool {
        matches!(outcome, ActionResult::Refused { reason, .. } if reason.is_transient())
    }
}

/// What the ledger says about an incoming request.
#[derive(Debug)]
pub(crate) enum Claim<R: Idempotent> {
    /// First time: execute it, then [`Guard::finish`].
    Fresh(Guard<R>),
    /// Same key, same body, already decided: return this.
    Replay(R::Outcome),
    /// Same key, different body.
    Conflict,
    /// Same key and body, still executing.
    InFlight,
}

#[derive(Debug)]
enum Record<R: Idempotent> {
    InFlight(R),
    Done(R, R::Outcome),
}

impl<R: Idempotent> Record<R> {
    fn request(&self) -> &R {
        match self {
            Self::InFlight(request) | Self::Done(request, _) => request,
        }
    }
}

#[derive(Debug)]
struct Inner<R: Idempotent> {
    records: HashMap<IdempotencyKey, Record<R>>,
    /// Finished keys, oldest first, for eviction.
    order: VecDeque<IdempotencyKey>,
}

/// Results by idempotency key.
#[derive(Debug)]
pub(crate) struct Ledger<R: Idempotent> {
    inner: Mutex<Inner<R>>,
    capacity: usize,
}

impl<R: Idempotent> Ledger<R> {
    /// A ledger remembering at most `capacity` finished results.
    pub(crate) fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                records: HashMap::new(),
                order: VecDeque::new(),
            }),
            capacity: capacity.max(1),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner<R>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Looks `request` up, reserving its key if it is new.
    pub(crate) fn claim(self: &Arc<Self>, request: &R) -> Claim<R> {
        let mut inner = self.lock();
        let key = request.idempotency_key();
        match inner.records.get(key) {
            Some(record) if record.request() != request => Claim::Conflict,
            Some(Record::InFlight(_)) => Claim::InFlight,
            Some(Record::Done(_, result)) => Claim::Replay(R::replayed(result.clone())),
            None => {
                inner
                    .records
                    .insert(key.clone(), Record::InFlight(request.clone()));
                Claim::Fresh(Guard {
                    ledger: Arc::clone(self),
                    key: Some(key.clone()),
                })
            }
        }
    }

    /// Whether `key` is in use here (running, or remembered as done), so
    /// another endpoint's ledger can refuse to reuse it.
    pub(crate) fn holds(&self, key: &IdempotencyKey) -> bool {
        self.lock().records.contains_key(key)
    }

    fn finish(&self, key: IdempotencyKey, result: Option<R::Outcome>) {
        let mut inner = self.lock();
        let Some(result) = result else {
            inner.records.remove(&key);
            return;
        };
        if let Some(record) = inner.records.remove(&key) {
            let request = match record {
                Record::InFlight(request) | Record::Done(request, _) => request,
            };
            inner
                .records
                .insert(key.clone(), Record::Done(request, result));
            inner.order.push_back(key);
        }
        while inner.order.len() > self.capacity {
            if let Some(old) = inner.order.pop_front() {
                inner.records.remove(&old);
            }
        }
    }
}

/// A reserved key. Dropping it without [`finish`](Self::finish) releases
/// the key (nothing recorded).
#[derive(Debug)]
pub(crate) struct Guard<R: Idempotent> {
    ledger: Arc<Ledger<R>>,
    key: Option<IdempotencyKey>,
}

impl<R: Idempotent> Guard<R> {
    /// Records `result` for the key, unless it is transient (the same key
    /// may be retried) or `record` is false (the bridge could not decide,
    /// e.g. herdr was unreachable before anything was sent).
    pub(crate) fn finish(mut self, result: &R::Outcome, record: bool) {
        if let Some(key) = self.key.take() {
            let stored = (record && !R::is_transient(result)).then(|| result.clone());
            self.ledger.finish(key, stored);
        }
    }
}

impl<R: Idempotent> Drop for Guard<R> {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            self.ledger.finish(key, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use protocol::RefusalReason;

    use super::*;

    fn action(key: &str, text: &str) -> Action {
        serde_json::from_value(serde_json::json!({
            "idempotencyKey": key,
            "agentId": "w1:p1",
            "expectedSeq": 1,
            "createdAt": 0,
            "kind": "prompt",
            "payload": {"text": text},
            "origin": "app",
        }))
        .expect("valid action")
    }

    const K1: &str = "00000000-0000-4000-8000-000000000001";
    const K2: &str = "00000000-0000-4000-8000-000000000002";
    const K3: &str = "00000000-0000-4000-8000-000000000003";

    #[test]
    fn a_held_key_is_visible_to_other_endpoints() {
        let ledger = Ledger::new(4);
        let a = action(K1, "go");
        assert!(!ledger.holds(&a.idempotency_key));
        let Claim::Fresh(guard) = ledger.claim(&a) else {
            panic!("fresh")
        };
        assert!(ledger.holds(&a.idempotency_key), "in flight");
        guard.finish(&ActionResult::Sent { replayed: false }, true);
        assert!(ledger.holds(&a.idempotency_key), "done");
    }

    #[test]
    fn replays_conflicts_and_in_flight() {
        let ledger = Ledger::new(8);
        let a = action(K1, "go");
        let Claim::Fresh(guard) = ledger.claim(&a) else {
            panic!("fresh");
        };
        assert!(matches!(ledger.claim(&a), Claim::InFlight));
        assert!(matches!(
            ledger.claim(&action(K1, "other")),
            Claim::Conflict
        ));
        guard.finish(&ActionResult::Sent { replayed: false }, true);
        assert!(matches!(
            ledger.claim(&a),
            Claim::Replay(ActionResult::Sent { replayed: true })
        ));
        assert!(matches!(
            ledger.claim(&action(K1, "other")),
            Claim::Conflict
        ));
    }

    #[test]
    fn transient_and_unrecorded_results_release_the_key() {
        let ledger = Ledger::new(8);
        let a = action(K1, "go");
        let Claim::Fresh(guard) = ledger.claim(&a) else {
            panic!("fresh");
        };
        let step_up = ActionResult::Refused {
            reason: RefusalReason::StepUpRequired,
            message: String::new(),
        };
        guard.finish(&step_up, true);
        let Claim::Fresh(guard) = ledger.claim(&a) else {
            panic!("released after a transient refusal");
        };
        guard.finish(
            &ActionResult::Failed {
                message: String::new(),
            },
            false,
        );
        let Claim::Fresh(guard) = ledger.claim(&a) else {
            panic!("released when not recorded");
        };
        drop(guard);
        assert!(
            matches!(ledger.claim(&a), Claim::Fresh(_)),
            "released on drop"
        );
    }

    #[test]
    fn oldest_results_are_forgotten_past_capacity() {
        let ledger = Ledger::new(2);
        for key in [K1, K2, K3] {
            let Claim::Fresh(guard) = ledger.claim(&action(key, "go")) else {
                panic!("fresh");
            };
            guard.finish(&ActionResult::Sent { replayed: false }, true);
        }
        assert!(matches!(ledger.claim(&action(K1, "go")), Claim::Fresh(_)));
        assert!(matches!(ledger.claim(&action(K3, "go")), Claim::Replay(_)));
    }
}
