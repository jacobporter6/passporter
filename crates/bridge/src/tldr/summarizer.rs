//! The summarizer as a trait object, so the bridge is not generic over it:
//! the binary plugs in [`summarize::ClaudeCli`], tests a
//! `summarize::FakeSummarizer`.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use protocol::Tldr;
use summarize::{Headline, StatusText, Suggestion, SuggestionContext, Summarizer, TurnDigest};

/// A boxed summarizer call.
pub type SummaryFuture<'a, T> = Pin<Box<dyn Future<Output = summarize::Result<T>> + Send + 'a>>;

/// [`Summarizer`] in dyn-compatible form. Implemented for every
/// [`Summarizer`].
pub trait DynSummarizer: Send + Sync + 'static {
    /// [`Summarizer::tldr`].
    fn tldr<'a>(&'a self, turn: &'a TurnDigest) -> SummaryFuture<'a, Tldr>;
    /// [`Summarizer::headline`].
    fn headline<'a>(&'a self, turn: &'a TurnDigest) -> SummaryFuture<'a, Headline>;
    /// [`Summarizer::status`].
    fn status<'a>(&'a self, turn: &'a TurnDigest) -> SummaryFuture<'a, StatusText>;
    /// [`Summarizer::suggestions`].
    fn suggestions<'a>(
        &'a self,
        context: &'a SuggestionContext,
    ) -> SummaryFuture<'a, Vec<Suggestion>>;
}

impl<S: Summarizer + 'static> DynSummarizer for S {
    fn tldr<'a>(&'a self, turn: &'a TurnDigest) -> SummaryFuture<'a, Tldr> {
        Box::pin(Summarizer::tldr(self, turn))
    }

    fn headline<'a>(&'a self, turn: &'a TurnDigest) -> SummaryFuture<'a, Headline> {
        Box::pin(Summarizer::headline(self, turn))
    }

    fn status<'a>(&'a self, turn: &'a TurnDigest) -> SummaryFuture<'a, StatusText> {
        Box::pin(Summarizer::status(self, turn))
    }

    fn suggestions<'a>(
        &'a self,
        context: &'a SuggestionContext,
    ) -> SummaryFuture<'a, Vec<Suggestion>> {
        Box::pin(Summarizer::suggestions(self, context))
    }
}

/// A shared, type-erased [`Summarizer`].
#[derive(Clone)]
pub struct SharedSummarizer(Arc<dyn DynSummarizer>);

impl SharedSummarizer {
    /// Wraps `summarizer`. Pass an `Arc` you keep a clone of to inspect it
    /// later (e.g. a fake's call counts).
    #[must_use]
    pub fn new(summarizer: Arc<dyn DynSummarizer>) -> Self {
        Self(summarizer)
    }
}

impl fmt::Debug for SharedSummarizer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SharedSummarizer")
    }
}

impl Summarizer for SharedSummarizer {
    async fn tldr(&self, turn: &TurnDigest) -> summarize::Result<Tldr> {
        self.0.tldr(turn).await
    }

    async fn headline(&self, turn: &TurnDigest) -> summarize::Result<Headline> {
        self.0.headline(turn).await
    }

    async fn status(&self, turn: &TurnDigest) -> summarize::Result<StatusText> {
        self.0.status(turn).await
    }

    async fn suggestions(&self, context: &SuggestionContext) -> summarize::Result<Vec<Suggestion>> {
        self.0.suggestions(context).await
    }
}
