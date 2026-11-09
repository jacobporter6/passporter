//! A horizontal swipe gesture, as pure state: fed pointer positions, it
//! decides whether the gesture is a sideways drag (and how far the content
//! should follow the finger) or a vertical scroll it must leave alone, and
//! whether the release travelled far enough to count.
//!
//! The DOM wiring lives with the screen that swipes (the agent detail); this
//! is pure so the thresholds are tested on the host.

/// Horizontal travel, in CSS pixels, that counts as a swipe on release.
pub const COMMIT_PX: f64 = 72.0;
/// Travel, in CSS pixels, before the gesture picks an axis.
const SLOP_PX: f64 = 10.0;
/// How much of the finger's travel the content follows when there is
/// nothing to swipe to in that direction (a rubber band).
const EDGE_RESISTANCE: f64 = 0.25;

/// Which way a swipe goes, in list terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Finger moved right: the item before this one.
    Previous,
    /// Finger moved left: the item after this one.
    Next,
}

impl Direction {
    /// Whichever of `previous` and `next` this direction points at.
    #[must_use]
    pub fn pick<T>(self, previous: T, next: T) -> T {
        match self {
            Self::Previous => previous,
            Self::Next => next,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    Undecided,
    Horizontal,
    Vertical,
}

/// What a pointer move means for the content.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Move {
    /// Not enough travel to tell yet: leave the content alone.
    Pending,
    /// A sideways drag: translate the content by this many CSS pixels.
    Drag(f64),
    /// A vertical gesture: the browser scrolls; ignore the rest of it.
    Scroll,
}

/// One swipe gesture, from pointer down to pointer up.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Swipe {
    origin: (f64, f64),
    dx: f64,
    axis: Axis,
    has_previous: bool,
    has_next: bool,
}

impl Swipe {
    /// Starts a gesture at `(x, y)`; `has_previous`/`has_next` say whether
    /// there is anything to swipe to on each side.
    #[must_use]
    pub const fn start(x: f64, y: f64, has_previous: bool, has_next: bool) -> Self {
        Self {
            origin: (x, y),
            dx: 0.0,
            axis: Axis::Undecided,
            has_previous,
            has_next,
        }
    }

    /// The pointer moved to `(x, y)`.
    pub fn moved(&mut self, x: f64, y: f64) -> Move {
        let (dx, dy) = (x - self.origin.0, y - self.origin.1);
        if self.axis == Axis::Undecided && dx.hypot(dy) >= SLOP_PX {
            // The same split the browser makes under `touch-action: pan-y`:
            // it scrolls mostly-vertical gestures and leaves the rest to
            // us, so every gesture either scrolls or swipes, never neither.
            self.axis = if dx.abs() > dy.abs() {
                Axis::Horizontal
            } else {
                Axis::Vertical
            };
        }
        match self.axis {
            Axis::Undecided => Move::Pending,
            Axis::Vertical => Move::Scroll,
            Axis::Horizontal => {
                self.dx = dx;
                Move::Drag(self.offset())
            }
        }
    }

    /// Whether the gesture is a sideways drag.
    #[must_use]
    pub fn is_dragging(&self) -> bool {
        self.axis == Axis::Horizontal
    }

    /// The pointer lifted at `(x, y)`: where the gesture lands. The lift
    /// point counts as a last move, so a quick flick whose final move
    /// event was coalesced away still commits.
    pub fn end(&mut self, x: f64, y: f64) -> Option<Direction> {
        self.moved(x, y);
        self.release()
    }

    /// Where the gesture lands on release: a direction there is something
    /// to go to, travelled past [`COMMIT_PX`]; otherwise `None` (snap back).
    #[must_use]
    pub fn release(&self) -> Option<Direction> {
        if !self.is_dragging() || self.dx.abs() < COMMIT_PX {
            return None;
        }
        let direction = self.direction()?;
        self.can_go(direction).then_some(direction)
    }

    fn direction(&self) -> Option<Direction> {
        if self.dx < 0.0 {
            Some(Direction::Next)
        } else if self.dx > 0.0 {
            Some(Direction::Previous)
        } else {
            None
        }
    }

    fn can_go(&self, direction: Direction) -> bool {
        direction.pick(self.has_previous, self.has_next)
    }

    fn offset(&self) -> f64 {
        match self.direction() {
            Some(direction) if !self.can_go(direction) => self.dx * EDGE_RESISTANCE,
            _ => self.dx,
        }
    }
}

/// The CSS `transform` that shifts content `offset` CSS pixels sideways, to
/// a tenth of a pixel.
///
/// Formats integer tenths rather than `{offset:.1}`: precision formatting of
/// an `f64` drags the whole float printer (Grisu, Dragon4, bignums, about
/// 15 KB) into the WASM for this one call.
#[must_use]
pub fn translate_x(offset: f64) -> String {
    // A drag is at most a screen wide, far inside `i32`.
    #[allow(clippy::cast_possible_truncation)]
    let tenths = (offset * 10.0).round() as i32;
    let sign = if tenths < 0 { "-" } else { "" };
    let tenths = tenths.unsigned_abs();
    format!("translate3d({sign}{}.{}px, 0, 0)", tenths / 10, tenths % 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_x_matches_one_decimal_formatting() {
        for offset in [0.0, 0.04, -0.04, 0.06, -0.06, 12.34, -12.35, 72.0, -399.96] {
            let tenths = (offset * 10.0_f64).round() / 10.0;
            let want = format!("translate3d({tenths:.1}px, 0, 0)").replace("-0.0px", "0.0px");
            assert_eq!(translate_x(offset), want, "{offset}");
        }
    }

    #[test]
    fn small_moves_wait_for_an_axis() {
        let mut swipe = Swipe::start(100.0, 100.0, true, true);
        assert_eq!(swipe.moved(104.0, 103.0), Move::Pending);
        assert_eq!(swipe.release(), None);
    }

    #[test]
    fn horizontal_drag_follows_the_finger_and_commits_past_the_threshold() {
        let mut swipe = Swipe::start(200.0, 100.0, true, true);
        assert_eq!(swipe.moved(180.0, 102.0), Move::Drag(-20.0));
        assert_eq!(swipe.release(), None);
        assert_eq!(swipe.moved(110.0, 110.0), Move::Drag(-90.0));
        assert_eq!(swipe.release(), Some(Direction::Next));
        // Dragging back under the threshold snaps back.
        swipe.moved(150.0, 110.0);
        assert_eq!(swipe.release(), None);
        swipe.moved(300.0, 110.0);
        assert_eq!(swipe.release(), Some(Direction::Previous));
    }

    #[test]
    fn vertical_gestures_are_left_to_the_browser() {
        let mut swipe = Swipe::start(100.0, 100.0, true, true);
        assert_eq!(swipe.moved(108.0, 130.0), Move::Scroll);
        // Even if it later turns sideways, it stays a scroll.
        assert_eq!(swipe.moved(0.0, 130.0), Move::Scroll);
        assert_eq!(swipe.release(), None);
    }

    #[test]
    fn diagonals_split_at_45_degrees() {
        let mut steep = Swipe::start(100.0, 100.0, true, true);
        assert_eq!(steep.moved(120.0, 130.0), Move::Scroll);
        let mut shallow = Swipe::start(100.0, 100.0, true, true);
        assert_eq!(shallow.moved(130.0, 120.0), Move::Drag(30.0));
    }

    #[test]
    fn the_lift_point_counts() {
        let mut swipe = Swipe::start(200.0, 100.0, true, true);
        assert_eq!(swipe.moved(170.0, 100.0), Move::Drag(-30.0));
        // The finger lifted well past the threshold after the last move.
        assert_eq!(swipe.end(100.0, 104.0), Some(Direction::Next));
        // A tap never commits.
        let mut tap = Swipe::start(200.0, 100.0, true, true);
        assert_eq!(tap.end(201.0, 100.0), None);
    }

    #[test]
    fn direction_picks_its_side() {
        assert_eq!(Direction::Previous.pick("a", "b"), "a");
        assert_eq!(Direction::Next.pick("a", "b"), "b");
    }

    #[test]
    fn edges_resist_and_never_commit() {
        let mut swipe = Swipe::start(100.0, 100.0, false, true);
        assert_eq!(swipe.moved(300.0, 100.0), Move::Drag(50.0));
        assert_eq!(swipe.release(), None);
        // The other side is open.
        assert_eq!(swipe.moved(0.0, 100.0), Move::Drag(-100.0));
        assert_eq!(swipe.release(), Some(Direction::Next));
    }
}
