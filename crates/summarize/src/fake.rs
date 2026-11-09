//! An in-memory [`Summarizer`] for tests (feature `fake`).

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use protocol::Tldr;

use crate::digest::TurnDigest;
use crate::error::{Error, Result};
use crate::headline::Headline;
use crate::status::StatusText;
use crate::suggest::{MAX_SUGGESTIONS, Suggestion, SuggestionContext};
use crate::summarizer::Summarizer;

/// A scripted [`Summarizer`]: returns a fixed TL;DR, headline and status
/// line (by default ones derived from the digest's stats) and fixed suggestions, fails with queued
/// errors, and counts calls.
#[derive(Debug, Default)]
pub struct FakeSummarizer {
    tldr: Mutex<Option<Tldr>>,
    headline: Mutex<Option<String>>,
    suggestions: Mutex<Vec<String>>,
    failures: Mutex<VecDeque<Error>>,
    headline_failures: Mutex<VecDeque<Error>>,
    status: Mutex<Option<String>>,
    status_failures: Mutex<VecDeque<Error>>,
    status_calls: AtomicUsize,
    delay: Option<Duration>,
    tldr_calls: AtomicUsize,
    headline_calls: AtomicUsize,
    suggestion_calls: AtomicUsize,
}

impl FakeSummarizer {
    /// A fake with the default TL;DR and no suggestions.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Always returns `tldr`.
    #[must_use]
    pub fn with_tldr(self, tldr: Tldr) -> Self {
        *lock(&self.tldr) = Some(tldr);
        self
    }

    /// Always returns this headline, validated like real model output (a
    /// headline that fails validation makes every call fail).
    #[must_use]
    pub fn with_headline(self, headline: &str) -> Self {
        *lock(&self.headline) = Some(headline.to_owned());
        self
    }

    /// Changes the headline later calls return.
    pub fn set_headline(&self, headline: &str) {
        *lock(&self.headline) = Some(headline.to_owned());
    }

    /// Always returns this status line, validated like real model output.
    #[must_use]
    pub fn with_status(self, status: &str) -> Self {
        *lock(&self.status) = Some(status.to_owned());
        self
    }

    /// Changes the status line later calls return.
    pub fn set_status(&self, status: &str) {
        *lock(&self.status) = Some(status.to_owned());
    }

    /// Returns these candidates, filtered like real model output (invalid,
    /// elevated and duplicate ones dropped, at most three kept).
    #[must_use]
    pub fn with_suggestions(self, candidates: &[&str]) -> Self {
        *lock(&self.suggestions) = candidates.iter().map(|&c| c.to_owned()).collect();
        self
    }

    /// Sleeps this long in every call, to exercise concurrency.
    #[must_use]
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }

    /// Makes the next TL;DR or suggestions call fail with `error`.
    /// Headline calls have their own queue ([`Self::fail_next_headline`]),
    /// so a headline made first does not take a failure meant for these.
    pub fn fail_next(&self, error: Error) {
        lock(&self.failures).push_back(error);
    }

    /// Makes the next headline call fail with `error`.
    pub fn fail_next_headline(&self, error: Error) {
        lock(&self.headline_failures).push_back(error);
    }

    /// Makes the next status-line call fail with `error`.
    pub fn fail_next_status(&self, error: Error) {
        lock(&self.status_failures).push_back(error);
    }

    /// Number of [`Summarizer::status`] calls so far.
    #[must_use]
    pub fn status_calls(&self) -> usize {
        self.status_calls.load(Ordering::SeqCst)
    }

    /// Number of [`Summarizer::tldr`] calls so far.
    #[must_use]
    pub fn tldr_calls(&self) -> usize {
        self.tldr_calls.load(Ordering::SeqCst)
    }

    /// Number of [`Summarizer::headline`] calls so far.
    #[must_use]
    pub fn headline_calls(&self) -> usize {
        self.headline_calls.load(Ordering::SeqCst)
    }

    /// Number of [`Summarizer::suggestions`] calls so far.
    #[must_use]
    pub fn suggestion_calls(&self) -> usize {
        self.suggestion_calls.load(Ordering::SeqCst)
    }

    async fn begin(&self, counter: &AtomicUsize, failures: &Mutex<VecDeque<Error>>) -> Result<()> {
        counter.fetch_add(1, Ordering::SeqCst);
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        match lock(failures).pop_front() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Summarizer for FakeSummarizer {
    async fn tldr(&self, turn: &TurnDigest) -> Result<Tldr> {
        self.begin(&self.tldr_calls, &self.failures).await?;
        if turn.is_empty() {
            return Err(Error::EmptyInput);
        }
        let fixed = lock(&self.tldr).clone();
        Ok(fixed.unwrap_or_else(|| Tldr {
            outcome: format!(
                "Fake summary: {} files changed.",
                turn.stats().files_changed
            ),
            changed: Vec::new(),
            decide: None,
            risks: Vec::new(),
        }))
    }

    async fn headline(&self, turn: &TurnDigest) -> Result<Headline> {
        self.begin(&self.headline_calls, &self.headline_failures)
            .await?;
        if turn.is_empty() {
            return Err(Error::EmptyInput);
        }
        let text = lock(&self.headline).clone().unwrap_or_else(|| {
            format!(
                "Fake headline: {} words, {} files changed.",
                turn.stats().output_words,
                turn.stats().files_changed
            )
        });
        Headline::new(&text).ok_or_else(|| Error::InvalidOutput("headline: fake".into()))
    }

    async fn status(&self, turn: &TurnDigest) -> Result<StatusText> {
        self.begin(&self.status_calls, &self.status_failures)
            .await?;
        if turn.is_empty() {
            return Err(Error::EmptyInput);
        }
        let text = lock(&self.status).clone().unwrap_or_else(|| {
            format!("Fake status: {} tool calls so far", turn.stats().tool_calls)
        });
        StatusText::new(&text).ok_or_else(|| Error::InvalidOutput("status line: fake".into()))
    }

    async fn suggestions(&self, _context: &SuggestionContext) -> Result<Vec<Suggestion>> {
        self.begin(&self.suggestion_calls, &self.failures).await?;
        let candidates = lock(&self.suggestions).clone();
        let mut out: Vec<Suggestion> = Vec::new();
        for suggestion in candidates.iter().filter_map(|c| Suggestion::new(c).ok()) {
            let duplicate = out
                .iter()
                .any(|s| s.as_str().eq_ignore_ascii_case(suggestion.as_str()));
            if !duplicate && out.len() < MAX_SUGGESTIONS {
                out.push(suggestion);
            }
        }
        Ok(out)
    }
}

/// Locks a mutex, recovering from poisoning (a panicked test thread).
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
