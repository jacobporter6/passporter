//! Reply suggestions for a question at the end of a turn
//! (`GET /v1/agents/:id/suggestions`; design §6.2).
//!
//! 2–3 short replies to a question an idle or done agent ended its last
//! finished turn on, offered as chips: a tap sends one as a prompt, and
//! none is ever sent without a tap.
//!
//! A blocked agent gets none: herdr refuses every prompt to a blocked
//! agent (`agent_blocked`), so a typed reply to its question could never
//! be delivered; its dialog is answered with its options or the keypad.
//!
//! The summarizer is the TL;DR one ([`TldrSettings::summarizer`]); without
//! it there are no suggestions.
//!
//! - **Made when the turn ends.** The TL;DR watcher starts a job as soon as
//!   it sees a turn finish, so the chips are usually ready by the time the
//!   phone asks; a request starts one too if there is none yet.
//! - **Cached by content.** Answers are kept per (agent, question), so the
//!   same question is never sent to the model twice, whatever happens to
//!   the agent's status meanwhile (re-checked once a job has its turn).
//! - **A finished turn costs no read on a hit.** Its question cannot
//!   change without herdr's `state_change_seq` moving (a new prompt starts
//!   a new turn), so (agent, seq) points at its question and a poll reads
//!   no transcript. A new seq reads the turn once; the same question then
//!   reuses its answer at once. The two are bounded separately, so seq
//!   churn never pushes answers out.
//! - **Settling.** A turn that just finished may not be fully written yet:
//!   unless its first read shows a question already answered, the job waits
//!   [`TldrSettings::settle`] and re-reads the turn until two reads that
//!   far apart agree, like the TL;DR job. A thread with no finished turn
//!   (a fresh session writes its transcript lazily, and a turn's prompt is
//!   written when it starts) has nothing to settle: nothing was asked.
//! - **One at a time, never stale.** Generations take turns with TL;DRs
//!   (one shared queue, as they share the summarizer and the account);
//!   when a job gets its turn it checks the agent is still at the seq it
//!   was asked at, and drops itself otherwise.
//! - **Failures back off.** A failed or timed-out generation, or a failed
//!   herdr check, is answered "later" until [`TldrSettings::retry_after`]
//!   has passed (doubling per failure, up to
//!   [`TldrSettings::max_retry_after`]); then the next request retries. A
//!   usage limit pauses new generations, TL;DRs included, for
//!   [`TldrSettings::max_retry_after`] ("later" too); answers already made
//!   are still served.
//!
//! Everything returned is a validated [`Suggestion`] (see
//! [`protocol::suggestion`]).
//!
//! [`TldrSettings::summarizer`]: super::TldrSettings::summarizer
//! [`TldrSettings::settle`]: super::TldrSettings::settle
//! [`TldrSettings::retry_after`]: super::TldrSettings::retry_after
//! [`TldrSettings::max_retry_after`]: super::TldrSettings::max_retry_after

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use protocol::{
    AgentId, PendingId, PendingInput, PendingKind, PendingOptions, PendingSource, ThreadContent,
    ThreadItem,
};
use summarize::{Suggestion, SuggestionContext, Summarizer};
use tokio::sync::Semaphore;
use tokio::time::Instant;

use super::SharedSummarizer;
use super::pace::{Pacing, lock, settled};
use crate::control::{AgentControl, Resolved, resolve};
use crate::shutdown::Shutdown;
use crate::threads::{ThreadSource, Threads, TurnQuery, TurnSlice};

/// Most turns remembered, and most answers kept (each bounded on its own);
/// the oldest are forgotten first.
const MAX_ENTRIES: usize = 256;

/// Longest end-of-turn question kept as the prompt, in characters (the
/// whole message still goes in as context, capped by the summarizer).
const MAX_QUESTION_CHARS: usize = 500;

/// The latest finished turn, read as if the agent is ready.
const LATEST: TurnQuery = TurnQuery {
    ready: true,
    known_finished: None,
    wanted: None,
};

/// The suggestions for what an agent asks, as far as the bridge knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuggestionsState {
    /// Generated (possibly empty: nothing asked, or the model had nothing
    /// safe to offer).
    Ready(Vec<Suggestion>),
    /// Queued or being generated; ask again shortly.
    Generating,
    /// Not yet: the last try failed or the account is at its usage limit,
    /// and a request after the pause tries again. Ask again later.
    Later,
    /// Nothing to suggest: the agent is not at its prompt, or suggestions
    /// are disabled.
    None,
}

/// The question an agent's turn ended on: the input the model sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Asked {
    /// The question itself (the message's last paragraph).
    question: String,
    /// The whole message, for context.
    message: String,
}

impl Asked {
    /// The question at the end of a finished turn: its last item, if that
    /// is an assistant message ending in a question mark (design §6.2,
    /// "last assistant message ends in a question"). A message followed by
    /// tool calls or file changes asked mid-turn (the turn went on, or was
    /// cut short), so it is not offered.
    #[must_use]
    pub(crate) fn from_turn(items: &[ThreadItem]) -> Option<Self> {
        let last = items
            .iter()
            .rev()
            .find(|item| !matches!(item.content, ThreadContent::Unknown))?;
        let ThreadContent::Assistant { text } = &last.content else {
            return None;
        };
        Some(Self {
            question: last_question(text)?,
            message: text.clone(),
        })
    }

    /// What the model sees. [`SuggestionContext`] is built from a pending
    /// input, so the question is framed as one (`None` only if the fixed
    /// id were invalid).
    fn context(&self) -> Option<SuggestionContext> {
        let pending = PendingInput {
            id: PendingId::new("turn-question").ok()?,
            kind: PendingKind::Question,
            prompt: self.question.clone(),
            detail: None,
            options: PendingOptions::default(),
            suggestions: Vec::new(),
            source: PendingSource::Unknown,
        };
        Some(SuggestionContext::from_pending(
            &pending,
            Some(&self.message),
        ))
    }

    /// A digest of everything the model would see, for the cache key.
    fn digest(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.question.hash(&mut hasher);
        self.message.hash(&mut hasher);
        hasher.finish()
    }
}

/// The last paragraph of `text`, if it ends in a question: trailing
/// Markdown emphasis, quotes and brackets are ignored. Capped at
/// [`MAX_QUESTION_CHARS`] (keeping the end, where the question is).
fn last_question(text: &str) -> Option<String> {
    let paragraph = text.rsplit("\n\n").map(str::trim).find(|p| !p.is_empty())?;
    let bare = paragraph.trim_end_matches(|c: char| {
        c.is_whitespace() || matches!(c, '*' | '_' | '`' | '"' | '\'' | ')' | ']' | '”' | '’')
    });
    if !bare.ends_with('?') {
        return None;
    }
    let collapsed = bare.split_whitespace().collect::<Vec<_>>().join(" ");
    let skip = collapsed.chars().count().saturating_sub(MAX_QUESTION_CHARS);
    Some(collapsed.chars().skip(skip).collect())
}

/// The latest finished turn of `agent`, at herdr seq `seq`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TurnKey {
    agent: AgentId,
    seq: u64,
}

/// The question `agent`'s turn ended on, by content ([`Asked::digest`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ContentKey {
    agent: AgentId,
    digest: u64,
}

/// What is known about a turn.
#[derive(Debug, Clone)]
enum Entry {
    /// A job is on it; the token tells its placeholder from a later one.
    Generating(u64),
    /// Read: the digest of the question it ended on, answered in
    /// [`Book::answers`], or `None` if it asked nothing.
    Asked(Option<u64>),
    /// Its last try failed (or was held by the usage limit) and may be
    /// retried at `retry_at`.
    Failed { attempts: u32, retry_at: Instant },
}

/// A map that forgets its oldest entries beyond [`MAX_ENTRIES`].
#[derive(Debug)]
struct Bounded<K, V> {
    entries: HashMap<K, V>,
    order: VecDeque<K>,
}

impl<K, V> Default for Bounded<K, V> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }
}

impl<K: Clone + Eq + Hash, V> Bounded<K, V> {
    fn get(&self, key: &K) -> Option<&V> {
        self.entries.get(key)
    }

    fn insert(&mut self, key: K, value: V) {
        if self.entries.insert(key.clone(), value).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > MAX_ENTRIES {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }

    fn remove(&mut self, key: &K) {
        if self.entries.remove(key).is_some() {
            self.order.retain(|k| k != key);
        }
    }
}

#[derive(Debug, Default)]
struct Book {
    turns: Bounded<TurnKey, Entry>,
    answers: Bounded<ContentKey, Vec<Suggestion>>,
    /// The next placeholder token.
    next_token: u64,
}

impl Book {
    /// Whether `key` holds the placeholder with `token`.
    fn holds(&self, key: &TurnKey, token: u64) -> bool {
        matches!(self.turns.get(key), Some(Entry::Generating(t)) if *t == token)
    }

    /// The answer to the question `key` asked, if it is still kept.
    fn answer_for(&self, key: &TurnKey, digest: Option<u64>) -> Option<Vec<Suggestion>> {
        match digest {
            None => Some(Vec::new()),
            Some(digest) => self
                .answers
                .get(&ContentKey {
                    agent: key.agent.clone(),
                    digest,
                })
                .cloned(),
        }
    }
}

/// Removes a generation's placeholder if its job ends without an answer
/// (stale, shutdown, panic), so a later request starts it again. Only its
/// own placeholder: if it was evicted and the key reused by a later job
/// (or answered), that entry is left alone.
struct Placeholder<'a> {
    book: &'a Mutex<Book>,
    key: Option<TurnKey>,
    token: u64,
}

impl Placeholder<'_> {
    /// Replaces the placeholder with `entry`, if it is still there (or
    /// was evicted and nothing took its key).
    fn settle(mut self, entry: Entry) {
        if let Some(key) = self.key.take() {
            let mut book = lock(self.book);
            if book.holds(&key, self.token) || book.turns.get(&key).is_none() {
                book.turns.insert(key, entry);
            }
        }
    }
}

impl Drop for Placeholder<'_> {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            let mut book = lock(self.book);
            if book.holds(&key, self.token) {
                book.turns.remove(&key);
            }
        }
    }
}

/// Where suggestions come from, and how they are paced.
#[derive(Clone)]
pub(crate) struct SuggestDeps {
    /// The model; `None` disables suggestions.
    pub summarizer: Option<SharedSummarizer>,
    /// Transcript readers.
    pub threads: Arc<Threads>,
    /// herdr, to check a job is still current when it gets its turn.
    pub control: Arc<dyn AgentControl>,
    /// Backstop on one generation, once it has its turn.
    pub timeout: Duration,
    /// Pause before (and between) reads of a just-finished turn.
    pub settle: Duration,
    /// The backoff after a failure, and the account's usage-limit pause,
    /// shared with TL;DRs.
    pub(crate) pacing: Pacing,
    /// The generation queue (one at a time), shared with TL;DRs.
    pub(crate) queue: Arc<Semaphore>,
}

impl std::fmt::Debug for SuggestDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SuggestDeps")
            .field("enabled", &self.summarizer.is_some())
            .field("timeout", &self.timeout)
            .field("settle", &self.settle)
            .finish_non_exhaustive()
    }
}

/// The suggestion cache and its generator.
pub struct Suggestions {
    deps: SuggestDeps,
    book: Mutex<Book>,
    shutdown: Shutdown,
}

impl std::fmt::Debug for Suggestions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Suggestions")
            .field("enabled", &self.enabled())
            .field("timeout", &self.deps.timeout)
            .finish_non_exhaustive()
    }
}

impl Suggestions {
    /// The service. Jobs stop at `shutdown`.
    #[must_use]
    pub(crate) fn new(deps: SuggestDeps, shutdown: Shutdown) -> Self {
        Self {
            deps,
            book: Mutex::default(),
            shutdown,
        }
    }

    /// Whether suggestions are made at all.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.deps.summarizer.is_some()
    }

    /// Suggestions for the question at the end of `agent`'s latest
    /// finished turn, if it is ready (idle or done); starts a job for it
    /// if there is none. Reads nothing on a cache hit.
    pub fn for_turn(self: &Arc<Self>, id: &AgentId, agent: &herdr::Agent) -> SuggestionsState {
        if !self.enabled() || !agent.agent_status.is_ready() {
            return SuggestionsState::None;
        }
        let Some(source) = ThreadSource::for_agent(id.clone(), agent) else {
            return SuggestionsState::None;
        };
        let key = TurnKey {
            agent: id.clone(),
            seq: agent.state_change_seq,
        };
        self.lookup(key, source)
    }

    /// The cached answer for `key`, or a job reading `source` queued for it
    /// unless a backoff or the usage limit holds it.
    fn lookup(self: &Arc<Self>, key: TurnKey, source: ThreadSource) -> SuggestionsState {
        let now = Instant::now();
        let (attempts, token) = {
            let mut book = lock(&self.book);
            let attempts = match book.turns.get(&key) {
                Some(Entry::Asked(digest)) => match book.answer_for(&key, *digest) {
                    Some(found) => return SuggestionsState::Ready(found),
                    // The answer was forgotten: read and answer again.
                    None => 0,
                },
                Some(Entry::Generating(_)) => return SuggestionsState::Generating,
                Some(Entry::Failed { retry_at, .. }) if *retry_at > now => {
                    return SuggestionsState::Later;
                }
                // Retried below; the count keeps the backoff doubling.
                Some(Entry::Failed { attempts, .. }) => *attempts,
                None => 0,
            };
            if self.deps.pacing.limit.active(now).is_some() {
                return SuggestionsState::Later;
            }
            let token = book.next_token;
            book.next_token = token.wrapping_add(1);
            book.turns.insert(key.clone(), Entry::Generating(token));
            (attempts, token)
        };
        let this = Arc::clone(self);
        let mut shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            let placeholder = Placeholder {
                book: &this.book,
                key: Some(key.clone()),
                token,
            };
            tokio::select! {
                entry = this.run(&key, source, attempts) => {
                    if let Some(entry) = entry {
                        placeholder.settle(entry);
                    }
                }
                () = shutdown.wait() => {}
            }
        });
        SuggestionsState::Generating
    }

    /// Runs one job: reads the turn (settling it unless its question was
    /// answered already), reuses an answer to the same question, else
    /// waits for its turn, checks it is still current and generates.
    /// `None` if it went stale (the placeholder is then dropped; nothing
    /// asks for an old seq again).
    async fn run(&self, key: &TurnKey, source: ThreadSource, attempts: u32) -> Option<Entry> {
        let summarizer = self.deps.summarizer.as_ref()?;
        let read = || self.deps.threads.finished_turn(source.clone(), LATEST);
        // No finished turn: a fresh session writes its transcript lazily,
        // and a turn's prompt is in it from the turn's start. Nothing asked.
        let Some(first) = read().await else {
            return Some(Entry::Asked(None));
        };
        let answered = |asked: &Asked| {
            let digest = asked.digest();
            lock(&self.book)
                .answer_for(key, Some(digest))
                .map(|_| Entry::Asked(Some(digest)))
        };
        // The same question as before (e.g. done → idle moved the seq):
        // nothing is waited for.
        if let Some(entry) = Asked::from_turn(&first.items).and_then(|a| answered(&a)) {
            return Some(entry);
        }
        // herdr may report the turn finished before its last lines are
        // written, whatever the first read ended on.
        tokio::time::sleep(self.deps.settle).await;
        let slice: Option<TurnSlice> = settled(self.deps.settle, read).await;
        let Some(asked) = slice.and_then(|slice| Asked::from_turn(&slice.items)) else {
            return Some(Entry::Asked(None));
        };
        if let Some(entry) = answered(&asked) {
            return Some(entry);
        }
        let Ok(_permit) = self.deps.queue.acquire().await else {
            return None;
        };
        // Another job may have answered the same question meanwhile.
        if let Some(entry) = answered(&asked) {
            return Some(entry);
        }
        if let Some(until) = self.deps.pacing.limit.active(Instant::now()) {
            return Some(Entry::Failed {
                attempts,
                retry_at: until,
            });
        }
        match self.still_at(&key.agent, key.seq).await {
            Some(true) => {}
            Some(false) => return None,
            None => {
                return Some(self.failed(&key.agent, attempts, "herdr agent lookup failed", false));
            }
        }
        let Some(context) = asked.context() else {
            return Some(Entry::Asked(None));
        };
        let generated =
            tokio::time::timeout(self.deps.timeout, summarizer.suggestions(&context)).await;
        let found = match generated {
            Ok(Ok(found)) => found,
            Ok(Err(error)) => {
                let usage_limit = error.is_usage_limit();
                return Some(self.failed(&key.agent, attempts, &error.to_string(), usage_limit));
            }
            Err(_) => {
                let error = format!("timed out after {:?}", self.deps.timeout);
                return Some(self.failed(&key.agent, attempts, &error, false));
            }
        };
        let digest = asked.digest();
        let by_content = ContentKey {
            agent: key.agent.clone(),
            digest,
        };
        lock(&self.book).answers.insert(by_content, found);
        Some(Entry::Asked(Some(digest)))
    }

    /// The entry for a failure: backs off, and a usage limit pauses every
    /// generation.
    fn failed(&self, agent: &AgentId, attempts: u32, error: &str, usage_limit: bool) -> Entry {
        let attempts = attempts.saturating_add(1);
        let wait = self.deps.pacing.after_failure(attempts, usage_limit);
        tracing::warn!(%error, %agent, retry_in = ?wait, "suggestions failed");
        Entry::Failed {
            attempts,
            retry_at: Instant::now() + wait,
        }
    }

    /// Whether `agent` is still at herdr seq `seq`; `None` if herdr could
    /// not tell.
    async fn still_at(&self, agent: &AgentId, seq: u64) -> Option<bool> {
        match resolve(self.deps.control.as_ref(), agent).await {
            Ok(Resolved::Current(now)) => Some(now.state_change_seq == seq),
            Ok(Resolved::Moved(_) | Resolved::Gone) => Some(false),
            Err(error) => {
                tracing::debug!(%error, %agent, "herdr agent lookup failed");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use protocol::{ThreadCursor, TurnIndex};

    use super::*;

    fn item(content: ThreadContent) -> ThreadItem {
        ThreadItem {
            cursor: ThreadCursor(1),
            turn: TurnIndex(1),
            rev: 1,
            at: None,
            content,
        }
    }

    fn assistant(text: &str) -> ThreadItem {
        item(ThreadContent::Assistant { text: text.into() })
    }

    #[test]
    fn a_question_followed_by_more_work_is_not_the_end_of_the_turn() {
        let tool = item(ThreadContent::Tool {
            name: "Bash".into(),
            input: "pnpm test".into(),
            status: protocol::ToolStatus::Error,
            output: None,
        });
        // Asked mid-turn, then the turn went on (or was cut short).
        assert!(Asked::from_turn(&[assistant("Shall I run the tests?"), tool.clone()]).is_none());
        // Unknown entries are skipped; the question is still last.
        let unknown = item(ThreadContent::Unknown);
        assert!(Asked::from_turn(&[tool, assistant("Shall I commit?"), unknown]).is_some());
    }

    #[test]
    fn the_end_of_turn_question_is_the_last_paragraph_of_the_last_message() {
        assert_eq!(
            last_question("Done.\n\nShall I **open a PR**?").as_deref(),
            Some("Shall I **open a PR**?")
        );
        assert_eq!(
            last_question("Want me to run them (it takes a while)?**\n").as_deref(),
            Some("Want me to run them (it takes a while)?")
        );
        assert_eq!(last_question("Is it?\n\nDone."), None);
        assert_eq!(last_question("   "), None);
        let long = format!("{}end?", "word ".repeat(400));
        let capped = last_question(&long).expect("a question");
        assert_eq!(capped.chars().count(), MAX_QUESTION_CHARS);
        assert!(capped.ends_with("end?"));
    }

    #[test]
    fn only_a_final_question_is_asked() {
        let asked = Asked::from_turn(&[
            item(ThreadContent::User {
                text: "fix it?".into(),
            }),
            assistant("Fixed.\n\nShould I also update the docs?"),
        ])
        .expect("asked");
        assert_eq!(asked.question, "Should I also update the docs?");
        assert!(Asked::from_turn(&[assistant("All done.")]).is_none());
        assert!(
            Asked::from_turn(&[item(ThreadContent::User {
                text: "really?".into()
            })])
            .is_none()
        );
    }

    #[test]
    fn digests_follow_the_content() {
        let a = Asked::from_turn(&[assistant("Proceed?")]).expect("asked");
        let b = Asked::from_turn(&[assistant("Proceed?")]).expect("asked");
        let c = Asked::from_turn(&[assistant("Merge?")]).expect("asked");
        assert_eq!(a.digest(), b.digest());
        assert_ne!(a.digest(), c.digest());
    }

    #[test]
    fn a_placeholder_only_touches_its_own_entry() {
        let agent: AgentId = "w1:p1".parse().expect("id");
        let key = TurnKey { agent, seq: 5 };
        let book = Mutex::new(Book::default());
        let answered =
            |book: &Mutex<Book>| matches!(lock(book).turns.get(&key), Some(Entry::Asked(None)));
        // Evicted and reused by a later job that answered: left alone.
        lock(&book).turns.insert(key.clone(), Entry::Asked(None));
        drop(Placeholder {
            book: &book,
            key: Some(key.clone()),
            token: 1,
        });
        assert!(answered(&book));
        let stale = Placeholder {
            book: &book,
            key: Some(key.clone()),
            token: 1,
        };
        stale.settle(Entry::Failed {
            attempts: 1,
            retry_at: Instant::now(),
        });
        assert!(answered(&book));
        // Its own placeholder is removed.
        lock(&book).turns.insert(key.clone(), Entry::Generating(2));
        drop(Placeholder {
            book: &book,
            key: Some(key.clone()),
            token: 2,
        });
        assert!(lock(&book).turns.get(&key).is_none());
    }

    #[test]
    fn turns_and_answers_are_bounded_apart() {
        let agent: AgentId = "w1:p1".parse().expect("id");
        let mut book = Book::default();
        let content = ContentKey {
            agent: agent.clone(),
            digest: 7,
        };
        book.answers.insert(content.clone(), Vec::new());
        let turn = |seq| TurnKey {
            agent: agent.clone(),
            seq,
        };
        for seq in 0..(MAX_ENTRIES as u64 + 10) {
            book.turns.insert(turn(seq), Entry::Asked(Some(7)));
        }
        assert_eq!(book.turns.entries.len(), MAX_ENTRIES);
        assert!(book.turns.get(&turn(0)).is_none());
        // However many seqs went by, the answer is still kept.
        assert_eq!(
            book.answer_for(&turn(MAX_ENTRIES as u64), Some(7)),
            Some(Vec::new())
        );
        assert_eq!(book.answer_for(&turn(1), Some(8)), None);
        book.turns.remove(&turn(MAX_ENTRIES as u64 + 9));
        assert_eq!(book.turns.order.len(), MAX_ENTRIES - 1);
    }
}
