//! The bounded, cursor-addressed item store behind a [`Thread`](super::Thread).

use std::collections::BTreeSet;

use protocol::{ThreadContent, ThreadCursor, ThreadItem, Timestamp, TurnIndex};

/// Bounds on what a thread keeps in memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// Most items kept. Older items are dropped (see
    /// [`Thread::truncated_before`](super::Thread::truncated_before)).
    pub max_items: usize,
    /// Approximate byte budget for kept items (their strings).
    pub max_bytes: usize,
    /// Most tool calls tracked as running; past this the oldest is settled
    /// as [`ToolStatus::Unknown`](protocol::ToolStatus::Unknown).
    pub max_pending: usize,
    /// A running tool call more than this many items behind the newest one
    /// is settled as unknown: a result that late is not worth pinning
    /// every client's resume cursor for.
    pub max_pending_age: u64,
}

impl Default for Window {
    fn default() -> Self {
        Self {
            max_items: 20_000,
            max_bytes: 32 * 1024 * 1024,
            max_pending: 64,
            max_pending_age: 2_000,
        }
    }
}

/// Fixed per-item overhead counted against [`Window::max_bytes`].
const ITEM_OVERHEAD: usize = 64;

#[derive(Debug, Default)]
pub(super) struct Store {
    /// The cursor just before `items[0]`.
    base: u64,
    items: Vec<ThreadItem>,
    bytes: usize,
    /// Items dropped from the front since the last reset.
    truncated: bool,
    /// Cursors added or updated since the last [`take_changed`](Self::take_changed).
    changed: BTreeSet<u64>,
    /// The last revision stamped on an item; never reset, so revisions
    /// only grow for the thread's lifetime.
    rev: u64,
}

impl Store {
    pub(super) fn items(&self) -> &[ThreadItem] {
        &self.items
    }

    pub(super) fn first_cursor(&self) -> ThreadCursor {
        ThreadCursor(self.base + 1)
    }

    pub(super) fn last_cursor(&self) -> ThreadCursor {
        ThreadCursor(self.base + self.items.len() as u64)
    }

    pub(super) fn truncated(&self) -> bool {
        self.truncated
    }

    fn index(&self, cursor: ThreadCursor) -> Option<usize> {
        let offset = cursor.0.checked_sub(self.base + 1)?;
        usize::try_from(offset)
            .ok()
            .filter(|&i| i < self.items.len())
    }

    pub(super) fn get(&self, cursor: ThreadCursor) -> Option<&ThreadItem> {
        self.items.get(self.index(cursor)?)
    }

    pub(super) fn since(&self, since: ThreadCursor) -> &[ThreadItem] {
        let skip = since.0.saturating_sub(self.base);
        usize::try_from(skip)
            .ok()
            .and_then(|skip| self.items.get(skip..))
            .unwrap_or_default()
    }

    pub(super) fn push(
        &mut self,
        turn: TurnIndex,
        at: Option<Timestamp>,
        content: ThreadContent,
    ) -> ThreadCursor {
        let cursor = ThreadCursor(self.last_cursor().0 + 1);
        self.bytes += size(&content);
        self.rev += 1;
        self.items.push(ThreadItem {
            cursor,
            turn,
            rev: self.rev,
            at,
            content,
        });
        self.changed.insert(cursor.0);
        cursor
    }

    /// Changes the content of an existing item and marks it changed. Returns
    /// false if the cursor is not held (dropped or never existed).
    pub(super) fn update(
        &mut self,
        cursor: ThreadCursor,
        change: impl FnOnce(&mut ThreadContent),
    ) -> bool {
        let Some(item) = self.index(cursor).and_then(|i| self.items.get_mut(i)) else {
            return false;
        };
        let before = size(&item.content);
        change(&mut item.content);
        let after = size(&item.content);
        self.bytes = self.bytes - before + after;
        self.rev += 1;
        item.rev = self.rev;
        self.changed.insert(cursor.0);
        true
    }

    /// Drops the oldest items while over either bound, down to three
    /// quarters of it (so trimming is amortized), always keeping the newest.
    pub(super) fn enforce(&mut self, window: &Window) {
        if self.items.len() <= window.max_items && self.bytes <= window.max_bytes {
            return;
        }
        let item_target = window.max_items / 4 * 3;
        let byte_target = window.max_bytes / 4 * 3;
        let mut drop = 0;
        let mut bytes = self.bytes;
        let mut kept = self.items.len();
        for item in &self.items {
            if kept <= 1 || (kept <= item_target && bytes <= byte_target) {
                break;
            }
            bytes -= size(&item.content);
            kept -= 1;
            drop += 1;
        }
        if drop == 0 {
            return;
        }
        self.items.drain(..drop);
        self.bytes = bytes;
        self.base += drop as u64;
        self.truncated = true;
        let first = self.base + 1;
        self.changed = self.changed.split_off(&first);
    }

    pub(super) fn take_changed(&mut self) -> Vec<ThreadCursor> {
        std::mem::take(&mut self.changed)
            .into_iter()
            .map(ThreadCursor)
            .collect()
    }

    /// Forgets every item; cursors continue after the last one.
    pub(super) fn reset(&mut self) {
        self.base = self.last_cursor().0;
        self.items.clear();
        self.bytes = 0;
        self.truncated = false;
        self.changed.clear();
    }
}

/// Approximate heap size of an item's content.
fn size(content: &ThreadContent) -> usize {
    ITEM_OVERHEAD
        + match content {
            ThreadContent::User { text } | ThreadContent::Assistant { text } => text.len(),
            ThreadContent::Tool {
                name,
                input,
                output,
                ..
            } => name.len() + input.len() + output.as_ref().map_or(0, String::len),
            ThreadContent::Diff { path, patch, .. } => {
                path.len() + patch.as_ref().map_or(0, String::len)
            }
            ThreadContent::Aside { label, text } => label.len() + text.len(),
            ThreadContent::Unknown => 0,
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(n: usize) -> ThreadContent {
        ThreadContent::Assistant {
            text: "x".repeat(n),
        }
    }

    #[test]
    fn drops_oldest_items_past_the_item_bound() {
        let window = Window {
            max_items: 8,
            ..Window::default()
        };
        let mut store = Store::default();
        for _ in 0..9 {
            store.push(TurnIndex(0), None, text(1));
            store.enforce(&window);
        }
        assert_eq!(store.items().len(), 6);
        assert_eq!(store.first_cursor(), ThreadCursor(4));
        assert_eq!(store.last_cursor(), ThreadCursor(9));
        assert!(store.truncated());
        assert_eq!(store.get(ThreadCursor(3)), None);
        assert_eq!(store.since(ThreadCursor(0)).len(), 6);
        assert_eq!(
            store.take_changed(),
            (4..=9).map(ThreadCursor).collect::<Vec<_>>()
        );
    }

    #[test]
    fn drops_oldest_items_past_the_byte_bound_but_keeps_the_newest() {
        let window = Window {
            max_bytes: 1_000,
            ..Window::default()
        };
        let mut store = Store::default();
        store.push(TurnIndex(0), None, text(400));
        store.push(TurnIndex(0), None, text(400));
        store.enforce(&window);
        assert_eq!(store.items().len(), 2, "within budget");
        store.push(TurnIndex(0), None, text(5_000));
        store.enforce(&window);
        assert_eq!(store.items().len(), 1, "the newest stays even when over");
        assert_eq!(store.first_cursor(), ThreadCursor(3));
    }

    #[test]
    fn updates_track_size_and_changes() {
        let mut store = Store::default();
        let c = store.push(TurnIndex(0), None, text(10));
        store.take_changed();
        let first_rev = store.get(c).expect("held").rev;
        assert!(store.update(c, |content| *content = text(100)));
        assert_eq!(store.bytes, ITEM_OVERHEAD + 100);
        assert!(
            store.get(c).expect("held").rev > first_rev,
            "updates bump rev"
        );
        assert_eq!(store.take_changed(), [c]);
        assert!(!store.update(ThreadCursor(99), |_| {}));
        store.reset();
        assert_eq!(store.first_cursor(), ThreadCursor(2));
        assert!(!store.truncated());
    }
}
