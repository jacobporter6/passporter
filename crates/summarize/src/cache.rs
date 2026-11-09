//! TL;DR caching per (session, turn) and eager generation (design §6.4:
//! "Cached per (agent session, turn). Generated eagerly when a long turn
//! finishes, so it's there by the time you look.").

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use protocol::{SessionId, ThreadCursor, ThreadItem, Tldr, TurnIndex};

use crate::digest::{TurnDigest, TurnStats};
use crate::error::Result;
use crate::summarizer::Summarizer;

/// Identifies the TL;DR of one turn of one agent session, as of a given
/// state of that turn. Built from the turn's items with [`TurnKey::new`],
/// so a TL;DR made mid-turn (or before the turn changed) is never served
/// for the completed turn: the completeness flag and the snapshot differ.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TurnKey {
    session: SessionId,
    turn: TurnIndex,
    complete: bool,
    snapshot: TurnSnapshot,
}

/// Which state of a turn's items a key refers to. Any new or changed item
/// changes it: item revisions strictly increase with every change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TurnSnapshot {
    items: usize,
    last_cursor: Option<ThreadCursor>,
    max_rev: u64,
}

impl TurnKey {
    /// The key for `turn` of `session` with these items (e.g.
    /// `transcripts::Thread::turn`). `complete` says whether the turn has
    /// finished.
    #[must_use]
    pub fn new(session: SessionId, turn: TurnIndex, items: &[ThreadItem], complete: bool) -> Self {
        Self {
            session,
            turn,
            complete,
            snapshot: TurnSnapshot {
                items: items.len(),
                last_cursor: items.iter().map(|item| item.cursor).max(),
                max_rev: items.iter().map(|item| item.rev).max().unwrap_or(0),
            },
        }
    }

    /// The agent session.
    #[must_use]
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// The turn within the session.
    #[must_use]
    pub fn turn(&self) -> TurnIndex {
        self.turn
    }

    /// Whether the key is for the finished turn.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.complete
    }
}

/// Storage for generated TL;DRs. [`MemoryCache`] is the in-memory
/// implementation; a persistent one can follow behind the same trait.
pub trait TldrCache: Send + Sync {
    /// The cached TL;DR for `key`, if any.
    fn get(&self, key: &TurnKey) -> Option<Tldr>;
    /// Stores `tldr` for `key`, replacing any earlier one.
    fn put(&self, key: TurnKey, tldr: Tldr);
}

/// Default number of TL;DRs a [`MemoryCache`] holds.
pub const DEFAULT_CACHE_CAPACITY: usize = 512;

/// A bounded in-memory [`TldrCache`]. When full, the oldest entry is
/// evicted.
#[derive(Debug)]
pub struct MemoryCache {
    capacity: usize,
    inner: Mutex<MemoryInner>,
}

#[derive(Debug, Default)]
struct MemoryInner {
    map: HashMap<TurnKey, Tldr>,
    order: VecDeque<TurnKey>,
}

impl MemoryCache {
    /// A cache holding at most `capacity` entries (at least one).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            inner: Mutex::default(),
        }
    }

    /// Number of cached entries.
    #[must_use]
    pub fn len(&self) -> usize {
        lock(&self.inner).map.len()
    }

    /// Whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for MemoryCache {
    fn default() -> Self {
        Self::new(DEFAULT_CACHE_CAPACITY)
    }
}

impl TldrCache for MemoryCache {
    fn get(&self, key: &TurnKey) -> Option<Tldr> {
        lock(&self.inner).map.get(key).cloned()
    }

    fn put(&self, key: TurnKey, tldr: Tldr) {
        let mut inner = lock(&self.inner);
        if inner.map.insert(key.clone(), tldr).is_none() {
            inner.order.push_back(key);
        }
        while inner.map.len() > self.capacity {
            let Some(oldest) = inner.order.pop_front() else {
                break;
            };
            inner.map.remove(&oldest);
        }
    }
}

/// One generation that concurrent callers share: its outcome, success or
/// failure, goes to every caller waiting on it.
type Flight = Arc<tokio::sync::OnceCell<Result<Tldr>>>;

/// Serves TL;DRs from a cache, generating missing ones with a
/// [`Summarizer`]. Concurrent requests for the same key share one
/// generation and its outcome, including a failure, so a usage limit is not
/// hit once per waiter. Errors are not cached: once nobody is waiting, the
/// next request tries again.
#[derive(Debug)]
pub struct TldrService<S, C> {
    summarizer: S,
    cache: C,
    in_flight: Mutex<HashMap<TurnKey, Flight>>,
}

impl<S: Summarizer, C: TldrCache> TldrService<S, C> {
    /// A service over `summarizer` and `cache`.
    pub fn new(summarizer: S, cache: C) -> Self {
        Self {
            summarizer,
            cache,
            in_flight: Mutex::default(),
        }
    }

    /// The underlying summarizer (e.g. for suggestions).
    pub fn summarizer(&self) -> &S {
        &self.summarizer
    }

    /// The cached TL;DR for `key`, without generating one.
    pub fn cached(&self, key: &TurnKey) -> Option<Tldr> {
        self.cache.get(key)
    }

    /// The TL;DR for `key`: from the cache, or generated from `turn` and
    /// cached. If another call is already generating it, waits for that
    /// one and returns its outcome. Cancel-safe: if the generating caller is
    /// dropped, a waiting caller takes over.
    pub async fn get_or_generate(&self, key: &TurnKey, turn: &TurnDigest) -> Result<Tldr> {
        if let Some(tldr) = self.cache.get(key) {
            return Ok(tldr);
        }
        let flight = InFlight::join(&self.in_flight, key);
        flight
            .cell
            .get_or_init(|| async {
                if let Some(tldr) = self.cache.get(key) {
                    return Ok(tldr);
                }
                let generated = self.summarizer.tldr(turn).await;
                if let Ok(tldr) = &generated {
                    self.cache.put(key.clone(), tldr.clone());
                }
                generated
            })
            .await
            .clone()
    }

    /// Call when a turn completes. If the turn is long (design §6.4, see
    /// [`TurnStats::is_long`]), generates and caches its TL;DR under the
    /// completed turn's key and returns it; otherwise returns `None` without
    /// building a digest or calling the model. `items` is the turn's slice
    /// (e.g. `transcripts::Thread::turn`) and `diff_stat` optional
    /// `git diff --stat` output.
    pub async fn on_turn_complete(
        &self,
        session: SessionId,
        turn: TurnIndex,
        items: &[ThreadItem],
        diff_stat: Option<&str>,
    ) -> Result<Option<Tldr>> {
        if !TurnStats::measure(items, diff_stat).is_long() {
            return Ok(None);
        }
        let key = TurnKey::new(session, turn, items, true);
        if let Some(tldr) = self.cache.get(&key) {
            return Ok(Some(tldr));
        }
        let digest = TurnDigest::build(items, diff_stat);
        self.get_or_generate(&key, &digest).await.map(Some)
    }
}

/// A caller's share of an in-flight generation. Dropping it, on return or
/// on cancellation, removes the map entry once no other caller holds it.
struct InFlight<'a> {
    map: &'a Mutex<HashMap<TurnKey, Flight>>,
    key: TurnKey,
    cell: Flight,
}

impl<'a> InFlight<'a> {
    fn join(map: &'a Mutex<HashMap<TurnKey, Flight>>, key: &TurnKey) -> Self {
        let cell = Arc::clone(lock(map).entry(key.clone()).or_default());
        Self {
            map,
            key: key.clone(),
            cell,
        }
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        let mut map = lock(self.map);
        // Joins and drops both happen under the map lock, so a count of two
        // (the map and this share) means nobody else is waiting.
        let ours = map
            .get(&self.key)
            .is_some_and(|cell| Arc::ptr_eq(cell, &self.cell));
        if ours && Arc::strong_count(&self.cell) == 2 {
            map.remove(&self.key);
        }
    }
}

/// Locks a mutex, recovering from poisoning: the guarded data stays
/// consistent because no critical section here can panic midway.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use protocol::ThreadContent;

    use super::*;
    use crate::error::Error;
    use crate::fake::FakeSummarizer;

    fn session() -> SessionId {
        SessionId::new("s1").unwrap()
    }

    fn key(turn: u32) -> TurnKey {
        TurnKey::new(session(), TurnIndex(turn), &[assistant(1, 10)], true)
    }

    fn tldr(outcome: &str) -> Tldr {
        Tldr {
            outcome: outcome.into(),
            changed: Vec::new(),
            decide: None,
            risks: Vec::new(),
        }
    }

    fn assistant(cursor: u64, words: usize) -> ThreadItem {
        ThreadItem {
            cursor: ThreadCursor(cursor),
            turn: TurnIndex(1),
            rev: cursor,
            at: None,
            content: ThreadContent::Assistant {
                text: "word ".repeat(words),
            },
        }
    }

    fn digest() -> TurnDigest {
        TurnDigest::build(&[assistant(1, 10)], None)
    }

    fn in_flight_is_empty<S, C>(service: &TldrService<S, C>) -> bool {
        lock(&service.in_flight).is_empty()
    }

    fn spawn_requests(
        service: &Arc<TldrService<FakeSummarizer, MemoryCache>>,
        n: usize,
    ) -> Vec<tokio::task::JoinHandle<Result<Tldr>>> {
        (0..n)
            .map(|_| {
                let service = Arc::clone(service);
                tokio::spawn(async move { service.get_or_generate(&key(1), &digest()).await })
            })
            .collect()
    }

    #[test]
    fn memory_cache_evicts_oldest() {
        let cache = MemoryCache::new(2);
        cache.put(key(1), tldr("one"));
        cache.put(key(2), tldr("two"));
        cache.put(key(1), tldr("one again"));
        assert_eq!(cache.len(), 2);
        cache.put(key(3), tldr("three"));
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(&key(1)), None);
        assert_eq!(cache.get(&key(2)).unwrap().outcome, "two");
        assert_eq!(cache.get(&key(3)).unwrap().outcome, "three");
    }

    #[test]
    fn keys_track_completeness_and_turn_state() {
        let partial = [assistant(1, 10)];
        let full = [assistant(1, 10), assistant(2, 10)];
        let mid = TurnKey::new(session(), TurnIndex(1), &partial, false);
        assert_ne!(mid, TurnKey::new(session(), TurnIndex(1), &partial, true));
        assert_ne!(
            TurnKey::new(session(), TurnIndex(1), &partial, true),
            TurnKey::new(session(), TurnIndex(1), &full, true)
        );
        // A changed item (higher rev, same cursor) is a new state.
        let mut changed = full.clone();
        changed[1].rev = 9;
        assert_ne!(
            TurnKey::new(session(), TurnIndex(1), &full, true),
            TurnKey::new(session(), TurnIndex(1), &changed, true)
        );
        assert_eq!(mid, TurnKey::new(session(), TurnIndex(1), &partial, false));
        assert!(!mid.is_complete());
        assert_eq!(mid.turn(), TurnIndex(1));
        assert_eq!(mid.session(), &session());
    }

    #[tokio::test]
    async fn generates_once_then_serves_from_cache() {
        let service = TldrService::new(FakeSummarizer::new(), MemoryCache::default());
        let first = service.get_or_generate(&key(1), &digest()).await.unwrap();
        let second = service.get_or_generate(&key(1), &digest()).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(service.summarizer().tldr_calls(), 1);
        assert_eq!(service.cached(&key(1)), Some(first));
        service.get_or_generate(&key(2), &digest()).await.unwrap();
        assert_eq!(service.summarizer().tldr_calls(), 2);
        assert!(in_flight_is_empty(&service));
    }

    #[tokio::test]
    async fn a_mid_turn_tldr_is_never_served_for_the_completed_turn() {
        let service = TldrService::new(FakeSummarizer::new(), MemoryCache::default());
        let partial = vec![assistant(1, 2000)];
        let mid_key = TurnKey::new(session(), TurnIndex(1), &partial, false);
        let mid = service
            .get_or_generate(&mid_key, &TurnDigest::build(&partial, None))
            .await
            .unwrap();

        let mut full = partial.clone();
        full.push(assistant(2, 10));
        let done = service
            .on_turn_complete(session(), TurnIndex(1), &full, Some(" 7 files changed"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(service.summarizer().tldr_calls(), 2);
        assert_ne!(done, mid);
        let done_key = TurnKey::new(session(), TurnIndex(1), &full, true);
        assert_eq!(service.cached(&done_key), Some(done));
        assert_eq!(service.cached(&mid_key), Some(mid));
    }

    #[tokio::test]
    async fn errors_are_not_cached() {
        let service = TldrService::new(FakeSummarizer::new(), MemoryCache::default());
        service.summarizer().fail_next(Error::UsageLimit {
            message: "limit".into(),
        });
        let err = service
            .get_or_generate(&key(1), &digest())
            .await
            .unwrap_err();
        assert!(err.is_usage_limit());
        assert_eq!(service.cached(&key(1)), None);
        assert!(service.get_or_generate(&key(1), &digest()).await.is_ok());
        assert_eq!(service.summarizer().tldr_calls(), 2);
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_generation() {
        let service = Arc::new(TldrService::new(
            FakeSummarizer::new().with_delay(Duration::from_millis(50)),
            MemoryCache::default(),
        ));
        for call in spawn_requests(&service, 5) {
            call.await.unwrap().unwrap();
        }
        assert_eq!(service.summarizer().tldr_calls(), 1);
        assert!(in_flight_is_empty(&service));
    }

    #[tokio::test]
    async fn a_failure_is_shared_with_waiters_not_retried_by_each() {
        let service = Arc::new(TldrService::new(
            FakeSummarizer::new().with_delay(Duration::from_millis(50)),
            MemoryCache::default(),
        ));
        service.summarizer().fail_next(Error::UsageLimit {
            message: "limit".into(),
        });
        for call in spawn_requests(&service, 5) {
            assert!(call.await.unwrap().unwrap_err().is_usage_limit());
        }
        assert_eq!(service.summarizer().tldr_calls(), 1);
        assert!(in_flight_is_empty(&service));
        // Once nobody waits, a new request retries.
        assert!(service.get_or_generate(&key(1), &digest()).await.is_ok());
        assert_eq!(service.summarizer().tldr_calls(), 2);
    }

    #[tokio::test]
    async fn cancellation_releases_the_in_flight_entry() {
        let service = Arc::new(TldrService::new(
            FakeSummarizer::new().with_delay(Duration::from_millis(200)),
            MemoryCache::default(),
        ));
        let leader = spawn_requests(&service, 1).remove(0);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!in_flight_is_empty(&service));
        leader.abort();
        assert!(leader.await.unwrap_err().is_cancelled());
        assert!(in_flight_is_empty(&service));
        assert!(service.get_or_generate(&key(1), &digest()).await.is_ok());
    }

    #[tokio::test]
    async fn eager_generation_only_for_long_turns() {
        let service = TldrService::new(FakeSummarizer::new(), MemoryCache::default());
        let short = service
            .on_turn_complete(session(), TurnIndex(1), &[assistant(1, 100)], None)
            .await
            .unwrap();
        assert_eq!(short, None);
        assert_eq!(service.summarizer().tldr_calls(), 0);

        let items = [assistant(1, 2000)];
        let long = service
            .on_turn_complete(session(), TurnIndex(2), &items, None)
            .await
            .unwrap();
        assert!(long.is_some());
        let done_key = TurnKey::new(session(), TurnIndex(2), &items, true);
        assert_eq!(service.cached(&done_key), long);
        // A repeated completion is served from the cache.
        service
            .on_turn_complete(session(), TurnIndex(2), &items, None)
            .await
            .unwrap();
        assert_eq!(service.summarizer().tldr_calls(), 1);

        let by_files = service
            .on_turn_complete(
                session(),
                TurnIndex(3),
                &[],
                Some(" 9 files changed, 9 insertions(+)"),
            )
            .await
            .unwrap();
        assert!(by_files.is_some());
    }
}
