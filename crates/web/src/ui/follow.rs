//! "Stick to the bottom" scrolling for the thread and screen panels: new
//! content scrolls the page to the end, unless the user scrolled up to read.
//! Content that arrives meanwhile can be noted in a [`Missed`] so a "jump to
//! latest" can be offered.

use leptos::prelude::*;

/// How close to the end, in CSS pixels, still counts as "at the bottom".
const SLACK_PX: f64 = 96.0;

/// Whether new content arrived below where the user is reading: set by
/// [`FollowBottom::on_arrival`], cleared when the user is back at the end
/// or jumps there.
#[derive(Debug, Clone, Copy)]
pub struct Missed(RwSignal<bool>);

impl Missed {
    /// Nothing missed yet.
    #[must_use]
    pub fn new() -> Self {
        Self(RwSignal::new(false))
    }

    /// Whether something was missed (tracked).
    #[must_use]
    pub fn get(self) -> bool {
        self.0.get()
    }

    /// Notes that content arrived out of view.
    pub fn note(self) {
        if self.0.try_get_untracked() == Some(false) {
            self.0.set(true);
        }
    }

    /// The user has seen the end.
    pub fn clear(self) {
        if self.0.try_get_untracked() == Some(true) {
            self.0.set(false);
        }
    }
}

impl Default for Missed {
    fn default() -> Self {
        Self::new()
    }
}

/// Follows the end of the page while the user is there.
#[derive(Debug, Clone, Copy)]
pub struct FollowBottom {
    stick: StoredValue<bool>,
    missed: Option<Missed>,
}

/// What one content change does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Follow it to the end.
    Scroll,
    /// The user is reading above: note that it arrived.
    Note,
    /// Nothing.
    Stay,
}

/// The step for a change: `changed` (anything differs), `arrived` (new
/// content at the end), with the user `at_end` or not.
const fn step(changed: bool, arrived: bool, at_end: bool) -> Step {
    if !changed {
        Step::Stay
    } else if at_end {
        Step::Scroll
    } else if arrived {
        Step::Note
    } else {
        Step::Stay
    }
}

impl FollowBottom {
    /// Starts tracking the page's scroll position for this component's
    /// lifetime.
    #[must_use]
    pub fn new() -> Self {
        Self::track(None)
    }

    /// Like [`Self::new`], noting arrivals out of view in `missed` (and
    /// clearing it when the user reaches the end).
    #[must_use]
    pub fn noting(missed: Missed) -> Self {
        Self::track(Some(missed))
    }

    fn track(missed: Option<Missed>) -> Self {
        let stick = StoredValue::new(true);
        let handle = window_event_listener(leptos::ev::scroll, move |_| {
            let bottom = at_bottom();
            stick.set_value(bottom);
            if let (true, Some(missed)) = (bottom, missed) {
                missed.clear();
            }
        });
        on_cleanup(move || handle.remove());
        Self { stick, missed }
    }

    /// Whether the user was at the end at the last scroll.
    #[must_use]
    pub fn at_end(self) -> bool {
        self.stick.get_value()
    }

    /// Scrolls to the end whenever `fingerprint` changes, if the user was
    /// at the end.
    pub fn on_change<T: PartialEq + Send + Sync + 'static>(
        self,
        fingerprint: impl Fn() -> T + Send + Sync + 'static,
    ) {
        self.on_arrival(fingerprint, |_, _| false);
    }

    /// Like [`Self::on_change`]; a change `arrived(previous, current)` says
    /// is new content is also noted in [`Missed`] while the user reads
    /// above. (Showing earlier items or a reload is a change, not news.)
    pub fn on_arrival<T: PartialEq + Send + Sync + 'static>(
        self,
        fingerprint: impl Fn() -> T + Send + Sync + 'static,
        arrived: impl Fn(&T, &T) -> bool + Send + Sync + 'static,
    ) {
        let Self { stick, missed } = self;
        Effect::new(move |previous: Option<T>| {
            let current = fingerprint();
            let changed = previous.as_ref() != Some(&current);
            let news = previous.as_ref().is_some_and(|p| arrived(p, &current));
            match step(changed, news, stick.get_value()) {
                Step::Scroll => request_animation_frame(scroll_to_bottom),
                Step::Note => {
                    if let Some(missed) = missed {
                        missed.note();
                    }
                }
                Step::Stay => {}
            }
            current
        });
    }
}

impl Default for FollowBottom {
    fn default() -> Self {
        Self::new()
    }
}

fn metrics() -> Option<(f64, f64, f64)> {
    let window = web_sys::window()?;
    let root = window.document()?.document_element()?;
    let scroll_y = window.scroll_y().ok()?;
    let height = window.inner_height().ok()?.as_f64()?;
    Some((scroll_y, height, f64::from(root.scroll_height())))
}

fn at_bottom() -> bool {
    metrics().is_none_or(|(y, h, total)| y + h >= total - SLACK_PX)
}

/// Scrolls the page to its end.
pub fn scroll_to_bottom() {
    if let (Some(window), Some((_, _, total))) = (web_sys::window(), metrics()) {
        window.scroll_to_with_x_and_y(0.0, total);
    }
}

#[cfg(test)]
mod tests {
    use super::{Missed, Step, step};

    #[test]
    fn changes_scroll_at_the_end_and_note_news_above() {
        assert_eq!(step(true, true, true), Step::Scroll);
        assert_eq!(step(true, false, true), Step::Scroll);
        assert_eq!(step(true, true, false), Step::Note);
        // "Show earlier", a reload: not news.
        assert_eq!(step(true, false, false), Step::Stay);
        assert_eq!(step(false, false, true), Step::Stay);
        assert_eq!(step(false, true, false), Step::Stay);
    }

    #[test]
    fn missed_is_noted_and_cleared() {
        let missed = Missed::new();
        assert!(!missed.get());
        missed.note();
        missed.note();
        assert!(missed.get());
        missed.clear();
        assert!(!missed.get());
        // A jump clears it; the next arrival notes it again.
        missed.note();
        assert!(missed.get());
    }
}
