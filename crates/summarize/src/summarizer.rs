//! The summarizer interface (design §6.4: "The summarizer sits behind an
//! interface so it can switch to the API later").

use protocol::Tldr;

use crate::digest::TurnDigest;
use crate::error::Result;
use crate::headline::Headline;
use crate::status::StatusText;
use crate::suggest::{Suggestion, SuggestionContext};

/// Produces TL;DRs, headlines, status lines and reply suggestions from untrusted agent output.
///
/// Implementations must treat their input as data, return only validated,
/// display-safe output, and fail with [`Error::UsageLimit`] when the
/// account's limit is hit so callers can back off.
///
/// [`Error::UsageLimit`]: crate::Error::UsageLimit
pub trait Summarizer: Send + Sync {
    /// A four-part summary of one turn.
    fn tldr(&self, turn: &TurnDigest) -> impl Future<Output = Result<Tldr>> + Send;

    /// A one-sentence headline of one turn (any length of turn). Build the
    /// digest with [`DigestLimits::headline`](crate::DigestLimits::headline)
    /// to keep the call cheap.
    fn headline(&self, turn: &TurnDigest) -> impl Future<Output = Result<Headline>> + Send;

    /// A one-line status of a turn still in progress: what the agent is
    /// doing now, and what it is waiting on if anything. Build the digest
    /// with [`DigestLimits::status`](crate::DigestLimits::status).
    fn status(&self, turn: &TurnDigest) -> impl Future<Output = Result<StatusText>> + Send;

    /// Up to [`MAX_SUGGESTIONS`](crate::MAX_SUGGESTIONS) short replies to a
    /// question or free-text request. May be empty. Offered only; never send
    /// one without the user choosing it.
    fn suggestions(
        &self,
        context: &SuggestionContext,
    ) -> impl Future<Output = Result<Vec<Suggestion>>> + Send;
}
