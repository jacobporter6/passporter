//! One agent's thread as the app holds it, synced by the rules in
//! [`protocol::thread`]: upsert by cursor keeping the higher `rev`, drop
//! everything on an epoch change, resume from the last `resumeCursor`.
//! It also keeps the turn headlines, from pages and `headline_ready`
//! events: final ones, and provisional ones of a turn still in progress
//! (replaced as it goes on, and by the final one when it finishes).
//!
//! Pure, so the merge rules are tested on the host.

use std::collections::BTreeMap;

use protocol::{
    ThreadContent, ThreadCursor, ThreadEpoch, ThreadItem, ThreadPage, TurnHeadline, TurnIndex,
};

/// What the caller should do after applying a page or event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sync {
    /// Merged. Fetch again from [`ThreadModel::resume`] if `more` is set.
    Merged {
        /// The page said it left items out (`hasMore`).
        more: bool,
    },
    /// The numbering changed under us: the cache was dropped; fetch the
    /// whole thread again from `since=0`.
    Refetch,
    /// An event for a thread that was never fetched; ignored (the first
    /// fetch will include it).
    Ignored,
}

/// A turn's headline as held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Headline {
    /// The text. Untrusted: render as text.
    pub text: String,
    /// Made from the turn so far, while it was in progress.
    pub provisional: bool,
}

/// The synced items of one agent's thread.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadModel {
    epoch: Option<ThreadEpoch>,
    items: BTreeMap<ThreadCursor, ThreadItem>,
    resume: ThreadCursor,
    truncated: bool,
    /// The bridge matched the transcript by folder (as of the last page).
    inferred: bool,
    /// Headlines of turns.
    headlines: BTreeMap<TurnIndex, Headline>,
    /// Bumped whenever `headlines` changes (never reset), so a view can
    /// track them without comparing the map.
    headlines_rev: u64,
    /// `headline_ready` events that arrived before the first page, applied
    /// with it (the page may have been read before they were made).
    early: Vec<TurnHeadline>,
}

/// Most `headline_ready` events kept while the first page loads.
const MAX_EARLY_HEADLINES: usize = 16;

impl ThreadModel {
    /// Whether a page has been applied yet.
    #[must_use]
    pub const fn is_loaded(&self) -> bool {
        self.epoch.is_some()
    }

    /// What to pass as `since` on the next fetch: `None` before the first
    /// page, then the latest resume cursor.
    #[must_use]
    pub const fn resume(&self) -> Option<ThreadCursor> {
        if self.epoch.is_some() {
            Some(self.resume)
        } else {
            None
        }
    }

    /// The numbering the held items use, once a page has been applied.
    #[must_use]
    pub const fn epoch(&self) -> Option<&ThreadEpoch> {
        self.epoch.as_ref()
    }

    /// Whether the bridge dropped older history.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    /// Whether the bridge matched the transcript by the agent's folder
    /// rather than by its session ([`ThreadPage::inferred`]).
    #[must_use]
    pub const fn inferred(&self) -> bool {
        self.inferred
    }

    /// The items in cursor order, unknown kinds left out.
    pub fn items(&self) -> impl DoubleEndedIterator<Item = &ThreadItem> + ExactSizeIterator {
        self.items.values()
    }

    /// Headlines of turns, by turn.
    #[must_use]
    pub const fn headlines(&self) -> &BTreeMap<TurnIndex, Headline> {
        &self.headlines
    }

    /// Changes whenever [`Self::headlines`] does.
    #[must_use]
    pub const fn headlines_rev(&self) -> u64 {
        self.headlines_rev
    }

    /// Number of items held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether no items are held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Applies the response to a fetch made with `since` (`None` = from the
    /// start).
    pub fn apply_page(&mut self, page: ThreadPage, since: Option<ThreadCursor>) -> Sync {
        let from_start = since.is_none_or(|c| c == ThreadCursor(0));
        let early = core::mem::take(&mut self.early);
        if self.epoch.as_ref() != Some(&page.epoch) {
            let had_epoch = self.epoch.is_some();
            self.reset(Some(page.epoch));
            if had_epoch && !from_start {
                // An increment in a numbering we hold nothing of.
                return Sync::Refetch;
            }
        }
        if page.truncated_before.is_some() {
            self.truncated = true;
        }
        self.inferred = page.inferred;
        // A page from the start carries the bridge's recent set; a later
        // one the last few.
        if from_start && !self.headlines.is_empty() {
            self.headlines.clear();
            self.headlines_rev = self.headlines_rev.wrapping_add(1);
        }
        let paged: Vec<TurnIndex> = page.headlines.iter().map(|h| h.turn).collect();
        for headline in page.headlines {
            self.set_headline(headline);
        }
        // Events in the order they came. The page may have been read
        // before or after an event, so its copy of a turn wins, except a
        // provisional copy over a final event: a turn's final headline is
        // always its latest.
        for headline in early {
            let stale = paged.contains(&headline.turn)
                && self
                    .headlines
                    .get(&headline.turn)
                    .is_some_and(|held| !held.provisional || headline.provisional);
            if !stale {
                self.set_headline(headline);
            }
        }
        self.merge(page.items);
        self.resume = page.resume_cursor;
        Sync::Merged {
            more: page.has_more,
        }
    }

    /// Applies a live `thread_items` event.
    pub fn apply_event(
        &mut self,
        epoch: &ThreadEpoch,
        items: Vec<ThreadItem>,
        resume: ThreadCursor,
    ) -> Sync {
        match &self.epoch {
            None => Sync::Ignored,
            Some(current) if current != epoch => {
                self.reset(None);
                Sync::Refetch
            }
            Some(_) => {
                self.merge(items);
                self.resume = resume;
                Sync::Merged { more: false }
            }
        }
    }

    /// Applies a live `headline_ready` event. Before the first page it is
    /// kept and applied with that page.
    pub fn apply_headline(&mut self, headline: TurnHeadline) {
        if self.is_loaded() {
            self.set_headline(headline);
        } else if self.early.len() < MAX_EARLY_HEADLINES {
            self.early.push(headline);
        }
    }

    /// Holds `turn`'s headline, replacing what it had (a provisional one
    /// replaces even a final one: the turn went on after it).
    fn set_headline(
        &mut self,
        TurnHeadline {
            turn,
            text,
            provisional,
        }: TurnHeadline,
    ) {
        let headline = Headline { text, provisional };
        if !headline.text.trim().is_empty() && self.headlines.get(&turn) != Some(&headline) {
            self.headlines.insert(turn, headline);
            self.headlines_rev = self.headlines_rev.wrapping_add(1);
        }
    }

    /// Drops everything (keeping the headline revision moving), starting
    /// over in `epoch` (`None`: unloaded, to be fetched again).
    fn reset(&mut self, epoch: Option<ThreadEpoch>) {
        *self = Self {
            epoch,
            headlines_rev: self.headlines_rev.wrapping_add(1),
            ..Self::default()
        };
    }

    fn merge(&mut self, items: Vec<ThreadItem>) {
        for item in items {
            if item.content == ThreadContent::Unknown {
                continue;
            }
            match self.items.get(&item.cursor) {
                Some(held) if held.rev >= item.rev => {}
                _ => {
                    self.items.insert(item.cursor, item);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use protocol::{ToolStatus, TurnIndex};

    use super::*;

    fn epoch(e: &str) -> ThreadEpoch {
        ThreadEpoch(e.to_owned())
    }

    fn text(cursor: u64, rev: u64, text: &str) -> ThreadItem {
        ThreadItem {
            cursor: ThreadCursor(cursor),
            turn: TurnIndex(1),
            rev,
            at: None,
            content: ThreadContent::Assistant { text: text.into() },
        }
    }

    fn tool(cursor: u64, rev: u64, status: ToolStatus) -> ThreadItem {
        ThreadItem {
            cursor: ThreadCursor(cursor),
            turn: TurnIndex(1),
            rev,
            at: None,
            content: ThreadContent::Tool {
                name: "Bash".into(),
                input: "ls".into(),
                output: None,
                status,
            },
        }
    }

    fn page(e: &str, items: Vec<ThreadItem>, resume: u64) -> ThreadPage {
        ThreadPage {
            epoch: epoch(e),
            items,
            resume_cursor: ThreadCursor(resume),
            truncated_before: None,
            has_more: false,
            headlines: Vec::new(),
            inferred: false,
        }
    }

    fn texts(model: &ThreadModel) -> Vec<u64> {
        model.items().map(|i| i.cursor.0).collect()
    }

    #[test]
    fn first_page_loads_and_sets_resume() {
        let mut model = ThreadModel::default();
        assert_eq!(model.resume(), None);
        let sync = model.apply_page(page("a", vec![text(2, 1, "b"), text(1, 1, "a")], 2), None);
        assert_eq!(sync, Sync::Merged { more: false });
        assert_eq!(texts(&model), [1, 2]);
        assert_eq!(model.resume(), Some(ThreadCursor(2)));
    }

    #[test]
    fn higher_rev_wins_in_either_order() {
        let mut model = ThreadModel::default();
        model.apply_page(page("a", vec![tool(1, 1, ToolStatus::Running)], 0), None);
        model.apply_event(
            &epoch("a"),
            vec![tool(1, 3, ToolStatus::Ok)],
            ThreadCursor(1),
        );
        // A page fetched before the update, delivered after it.
        model.apply_page(
            page("a", vec![tool(1, 2, ToolStatus::Running)], 0),
            Some(ThreadCursor(0)),
        );
        let held = model.items().next().unwrap();
        assert_eq!(held.rev, 3);
        assert!(matches!(
            held.content,
            ThreadContent::Tool {
                status: ToolStatus::Ok,
                ..
            }
        ));
    }

    #[test]
    fn event_with_new_epoch_drops_cache_and_asks_for_refetch() {
        let mut model = ThreadModel::default();
        model.apply_page(page("a", vec![text(1, 1, "x")], 1), None);
        let sync = model.apply_event(&epoch("b"), vec![text(9, 1, "y")], ThreadCursor(9));
        assert_eq!(sync, Sync::Refetch);
        assert!(model.is_empty());
        assert_eq!(model.resume(), None);
    }

    #[test]
    fn late_transcript_new_epoch_without_items_resets() {
        let mut model = ThreadModel::default();
        model.apply_page(page("none", vec![], 0), None);
        assert!(model.is_loaded());
        let sync = model.apply_event(&epoch("t1"), vec![], ThreadCursor(0));
        assert_eq!(sync, Sync::Refetch);
        assert!(!model.is_loaded());
    }

    #[test]
    fn incremental_page_in_a_new_epoch_asks_for_refetch() {
        let mut model = ThreadModel::default();
        model.apply_page(page("a", vec![text(1, 1, "x")], 1), None);
        let sync = model.apply_page(page("b", vec![text(5, 1, "y")], 5), Some(ThreadCursor(1)));
        assert_eq!(sync, Sync::Refetch);
        assert!(model.is_empty());
        // The refetch from the start then loads the new numbering.
        let sync = model.apply_page(page("b", vec![text(1, 1, "z")], 1), Some(ThreadCursor(0)));
        assert_eq!(sync, Sync::Merged { more: false });
        assert_eq!(texts(&model), [1]);
    }

    #[test]
    fn events_before_the_first_fetch_are_ignored() {
        let mut model = ThreadModel::default();
        assert_eq!(
            model.apply_event(&epoch("a"), vec![text(1, 1, "x")], ThreadCursor(1)),
            Sync::Ignored
        );
        assert!(!model.is_loaded());
    }

    #[test]
    fn has_more_and_truncation_are_reported() {
        let mut model = ThreadModel::default();
        let mut first = page("a", vec![text(3, 1, "x")], 3);
        first.has_more = true;
        first.truncated_before = Some(ThreadCursor(3));
        assert_eq!(model.apply_page(first, None), Sync::Merged { more: true });
        assert!(model.truncated());
    }

    #[test]
    fn a_transcript_matched_by_folder_is_reported_per_page() {
        let mut model = ThreadModel::default();
        let mut first = page("a", vec![text(1, 1, "x")], 1);
        first.inferred = true;
        model.apply_page(first, None);
        assert!(model.inferred());
        // herdr named the session since: a new numbering, not inferred.
        model.apply_page(page("b", vec![text(1, 1, "y")], 1), None);
        assert!(!model.inferred());
    }

    #[test]
    fn headlines_come_from_pages_and_events() {
        let rev = |model: &ThreadModel| model.headlines_rev();
        let headline = |turn: u32, text: &str| TurnHeadline {
            turn: TurnIndex(turn),
            text: text.into(),
            provisional: false,
        };
        let mut model = ThreadModel::default();
        let mut first = page("a", vec![text(1, 1, "x")], 1);
        first.headlines = vec![headline(1, "Fixed the parser."), headline(2, "  ")];
        model.apply_page(first, None);
        assert_eq!(headline_of(&model, TurnIndex(1)), Some("Fixed the parser."));
        assert_eq!(
            headline_of(&model, TurnIndex(2)),
            None,
            "blank ones are dropped"
        );

        // A later page merges its last few.
        let mut next = page("a", vec![], 1);
        next.headlines = vec![headline(2, "Pushed.")];
        model.apply_page(next, Some(ThreadCursor(1)));
        assert_eq!(headline_of(&model, TurnIndex(1)), Some("Fixed the parser."));
        assert_eq!(headline_of(&model, TurnIndex(2)), Some("Pushed."));

        // An event adds one, even the same text as another turn's.
        model.apply_headline(headline(3, "Pushed."));
        assert_eq!(headline_of(&model, TurnIndex(3)), Some("Pushed."));

        // A page from the start replaces the set.
        let mut again = page("a", vec![], 1);
        again.headlines = vec![headline(3, "Pushed.")];
        model.apply_page(again, None);
        assert_eq!(headline_of(&model, TurnIndex(1)), None);
        assert_eq!(headline_of(&model, TurnIndex(3)), Some("Pushed."));

        let before = rev(&model);
        model.apply_headline(headline(3, "Pushed."));
        assert_eq!(rev(&model), before, "the same text changes nothing");

        model.apply_event(&epoch("b"), vec![], ThreadCursor(0));
        assert_eq!(headline_of(&model, TurnIndex(3)), None);
        assert_ne!(rev(&model), before);
        // Before the first page an event is kept, and applied with it
        // unless the page has that turn's headline.
        model.apply_headline(headline(4, "Later."));
        model.apply_headline(headline(1, "Stale."));
        assert_eq!(headline_of(&model, TurnIndex(4)), None);
        let mut first = page("b", vec![], 1);
        first.headlines = vec![headline(1, "From the page.")];
        model.apply_page(first, None);
        assert_eq!(headline_of(&model, TurnIndex(4)), Some("Later."));
        assert_eq!(headline_of(&model, TurnIndex(1)), Some("From the page."));
    }

    #[test]
    fn a_final_event_before_the_first_page_beats_its_provisional_copy() {
        let headline = |turn: u32, text: &str, provisional| TurnHeadline {
            turn: TurnIndex(turn),
            text: text.into(),
            provisional,
        };
        let mut model = ThreadModel::default();
        // While the page loads: turn 2 so far, then its final headline,
        // and turn 3 so far twice.
        model.apply_headline(headline(2, "Merging.", true));
        model.apply_headline(headline(2, "Merged.", false));
        model.apply_headline(headline(3, "Starting.", true));
        model.apply_headline(headline(3, "Started.", true));
        let mut first = page("a", vec![text(1, 2, "x")], 1);
        // The page was read before the final headline was made.
        first.headlines = vec![headline(2, "Merging.", true), headline(3, "Paged.", false)];
        model.apply_page(first, None);
        let held = &model.headlines()[&TurnIndex(2)];
        assert_eq!((held.text.as_str(), held.provisional), ("Merged.", false));
        // A final copy on the page beats provisional events.
        assert_eq!(headline_of(&model, TurnIndex(3)), Some("Paged."));
        // Events for turns the page lacks apply in order: the last wins.
        let mut model = ThreadModel::default();
        model.apply_headline(headline(4, "Starting.", true));
        model.apply_headline(headline(4, "Started.", true));
        model.apply_page(page("a", vec![], 1), None);
        assert_eq!(headline_of(&model, TurnIndex(4)), Some("Started."));
    }

    #[test]
    fn a_provisional_headline_is_replaced_as_the_turn_goes_on() {
        let headline = |text: &str, provisional| TurnHeadline {
            turn: TurnIndex(2),
            text: text.into(),
            provisional,
        };
        let mut model = ThreadModel::default();
        model.apply_page(page("a", vec![text(1, 2, "x")], 1), None);
        model.apply_headline(headline("Merging PRs.", true));
        let held = model.headlines().get(&TurnIndex(2)).expect("held");
        assert!(held.provisional);
        // The final one replaces it, and a later provisional one (the
        // turn went on) the final one.
        let before = model.headlines_rev();
        model.apply_headline(headline("Merging PRs.", false));
        assert_ne!(model.headlines_rev(), before, "the same text, now final");
        assert!(!model.headlines()[&TurnIndex(2)].provisional);
        model.apply_headline(headline("Merging more.", true));
        assert_eq!(headline_of(&model, TurnIndex(2)), Some("Merging more."));
        assert!(model.headlines()[&TurnIndex(2)].provisional);
    }

    fn headline_of(model: &ThreadModel, turn: TurnIndex) -> Option<&str> {
        model.headlines().get(&turn).map(|h| h.text.as_str())
    }

    #[test]
    fn unknown_items_are_skipped() {
        let mut model = ThreadModel::default();
        let mut unknown = text(1, 1, "");
        unknown.content = ThreadContent::Unknown;
        model.apply_page(page("a", vec![unknown, text(2, 1, "x")], 2), None);
        assert_eq!(texts(&model), [2]);
        assert_eq!(model.len(), 1);
    }
}
