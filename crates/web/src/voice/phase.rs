//! Where one agent's dictation is: the take being recorded ([`Take`]) and
//! the notes on their way to a transcript ([`Note`], oldest first). What
//! the strip shows ([`rows`], each a [`Strip`] drawn by
//! [`super::panel`]) and what the app's mic button needs to know of it
//! ([`Mic`]) follow from those. Pure, so tested on the host.
//!
//! Notes are sticky: one stays on the strip until its transcript is sent
//! or used, or the user discards it. They are delivered one at a time,
//! oldest first ([`next_to_deliver`]), so prompts reach the agent in the
//! order they were said; a note that failed holds back the ones behind it
//! until it is retried or discarded.

use protocol::AudioId;

use super::upload::Progress;

/// The take: the microphone's side of the strip.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Take {
    /// Not recording.
    #[default]
    Idle,
    /// Waiting for the microphone (the permission prompt, the first time).
    Opening,
    /// Recording; the meter is live.
    Recording,
    /// Stopped; saving the clip on the device.
    Saving,
    /// Recording could not start or finish, and nothing was stored.
    Failed(String),
}

impl Take {
    /// Whether the microphone is (being) used.
    #[must_use]
    pub const fn is_live(&self) -> bool {
        matches!(self, Self::Opening | Self::Recording)
    }

    /// Whether a new take may start: not while one is recording or
    /// saving. Notes on their way don't stop it.
    #[must_use]
    pub const fn can_start(&self) -> bool {
        matches!(self, Self::Idle | Self::Failed(_))
    }

    /// What the mic button can do.
    #[must_use]
    pub const fn mic(&self) -> Mic {
        match self {
            Self::Opening | Self::Recording => Mic::Live,
            Self::Saving => Mic::Busy,
            Self::Idle | Self::Failed(_) => Mic::Ready,
        }
    }

    /// The take's row; `send` when stopping sends the transcript.
    #[must_use]
    pub fn strip(&self, send: bool) -> Strip {
        match self {
            Self::Idle => Strip::Empty,
            Self::Opening => Strip::Live {
                opening: true,
                send,
            },
            Self::Recording => Strip::Live {
                opening: false,
                send,
            },
            Self::Saving => Strip::Status {
                text: "Saving…".to_owned(),
                busy: false,
                cancel: None,
            },
            Self::Failed(message) => Strip::Card {
                alert: true,
                title: None,
                message: message.clone(),
                note: None,
                actions: vec![Action::Dismiss],
            },
        }
    }
}

/// Where one stored note is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteState {
    /// Stored; waiting for the notes before it.
    Queued,
    /// Sending the clip.
    Uploading(Progress),
    /// Waiting for the transcript.
    Transcribing,
    /// The bridge is unreachable; retrying by itself.
    Waiting {
        /// Why, for people.
        message: String,
    },
    /// Stopped short; the clip is stored and can be retried.
    Failed {
        /// What happened, for people.
        message: String,
        /// The delivery stopped retrying by itself (the bridge stayed
        /// unreachable): it is tried again when the network comes back.
        parked: bool,
    },
    /// The transcript, for the user to review (the "Review dictation
    /// before sending" setting, or a note left from an earlier visit).
    Ready {
        /// What the bridge heard (may be empty).
        text: String,
        /// Why the take stopped, when the user may not know (see
        /// [`SCREEN_OFF`]).
        note: Option<&'static str>,
    },
}

/// A stored note on the strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// The strip's own key for it: unlike `id`, it never changes (the
    /// bridge can make a clip move to a fresh id).
    pub key: u32,
    /// The stored clip.
    pub id: AudioId,
    /// How long it is, for its label.
    pub duration_ms: u64,
    /// Whether its transcript is sent without review.
    pub send: bool,
    /// Why its take stopped, for its transcript card, when the user may
    /// not know (see [`SCREEN_OFF`]).
    pub stopped: Option<&'static str>,
    /// Where it is.
    pub state: NoteState,
}

impl Note {
    /// Whether it is being uploaded or transcribed now.
    #[must_use]
    pub const fn is_delivering(&self) -> bool {
        matches!(
            self.state,
            NoteState::Uploading(_) | NoteState::Transcribing
        )
    }

    /// Whether it has a transcript waiting for review.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self.state, NoteState::Ready { .. })
    }

    /// Its row on the strip.
    #[must_use]
    pub fn strip(&self) -> Strip {
        let length = clock(self.duration_ms);
        let status = |text: String, busy, cancel| Strip::Status {
            text,
            busy,
            cancel: Some(cancel),
        };
        match &self.state {
            NoteState::Queued => status(format!("{length} · Queued"), false, Action::Discard),
            NoteState::Uploading(progress) => {
                let percent = progress.sent.saturating_mul(100) / progress.total.max(1);
                status(
                    format!("{length} · Uploading… {percent}%"),
                    false,
                    Action::Cancel,
                )
            }
            NoteState::Transcribing => {
                status(format!("{length} · Transcribing…"), true, Action::Cancel)
            }
            NoteState::Waiting { message } => Strip::Card {
                alert: false,
                title: Some(format!("Dictation ({length})")),
                message: message.clone(),
                note: Some("Retrying by itself."),
                actions: vec![Action::Retry { now: true }, Action::Discard],
            },
            NoteState::Failed { message, .. } => Strip::Card {
                alert: true,
                title: Some(format!("Dictation ({length}) not transcribed")),
                message: message.clone(),
                note: None,
                actions: vec![Action::Retry { now: false }, Action::Discard],
            },
            NoteState::Ready { text, note } => Strip::Transcript {
                title: format!("Transcript ({length})"),
                text: text.clone(),
                note: *note,
            },
        }
    }
}

/// The next note to deliver, by key: the oldest one without a
/// transcript, if it is queued. A note that failed (or is retrying)
/// holds back the ones behind it; one waiting for review does not.
#[must_use]
pub fn next_to_deliver(notes: &[Note]) -> Option<u32> {
    notes
        .iter()
        .find(|note| !note.is_ready())
        .filter(|note| note.state == NoteState::Queued)
        .map(|note| note.key)
}

/// Whether the screen is kept on: from the tap until the take is saved,
/// and while a note is uploading or transcribing or is next to be (so the
/// lock isn't dropped between saving a take and sending it). A card (a
/// transcript to review, a failure, a note waiting for the bridge, and
/// any note queued behind one) waits for the user with the clip safe on
/// the phone, so the screen may sleep.
#[must_use]
pub fn keeps_awake(take: &Take, notes: &[Note]) -> bool {
    matches!(take, Take::Opening | Take::Recording | Take::Saving)
        || notes.iter().any(Note::is_delivering)
        || next_to_deliver(notes).is_some()
}

/// Which row of the strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKey {
    /// The take.
    Take,
    /// The note with this [`Note::key`].
    Note(u32),
}

/// The strip's rows, top to bottom: the notes, oldest first, then the
/// take (nearest the composer) unless it is idle.
#[must_use]
pub fn rows(take: &Take, notes: &[Note], send: bool) -> Vec<(RowKey, Strip)> {
    let mut rows: Vec<_> = notes
        .iter()
        .map(|note| (RowKey::Note(note.key), note.strip()))
        .collect();
    let take = take.strip(send);
    if take != Strip::Empty {
        rows.push((RowKey::Take, take));
    }
    rows
}

/// The mic button's state: all the app needs to know of a [`Take`]. It
/// crosses from the dictation WASM to the page as [`Mic::code`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mic {
    /// Tap or hold to record.
    #[default]
    Ready,
    /// Recording (or opening): tap to stop.
    Live,
    /// Saving the take just stopped: wait a moment.
    Busy,
}

impl Mic {
    /// The state as a number.
    #[must_use]
    pub const fn code(self) -> u32 {
        match self {
            Self::Ready => 0,
            Self::Live => 1,
            Self::Busy => 2,
        }
    }

    /// The state [`Self::code`] gave; an unknown code is [`Mic::Busy`].
    #[must_use]
    pub const fn from_code(code: u32) -> Self {
        match code {
            0 => Self::Ready,
            1 => Self::Live,
            _ => Self::Busy,
        }
    }
}

/// What one row of the strip shows: a few shapes the states share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Strip {
    /// Nothing.
    Empty,
    /// The meter and a stop button; `opening` while the microphone opens
    /// (no meter yet, and the button cancels). With `send`, stopping sends
    /// the transcript, and the button says so.
    Live {
        /// The microphone is still opening.
        opening: bool,
        /// Stopping sends (no review).
        send: bool,
    },
    /// One line of progress.
    Status {
        /// What is happening.
        text: String,
        /// Show the spinner.
        busy: bool,
        /// The button that discards the note, if any.
        cancel: Option<Action>,
    },
    /// A message with actions.
    Card {
        /// Announce it as an alert (a failure), not a status.
        alert: bool,
        /// A heading.
        title: Option<String>,
        /// What happened.
        message: String,
        /// A second line.
        note: Option<&'static str>,
        /// The buttons, in order.
        actions: Vec<Action>,
    },
    /// The transcript card.
    Transcript {
        /// Its heading.
        title: String,
        /// What the bridge heard (may be empty).
        text: String,
        /// Why the take stopped, if the user may not know.
        note: Option<&'static str>,
    },
}

/// A button on a [`Strip::Card`] or [`Strip::Status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Deliver the stored note again (now, if it is waiting to).
    Retry {
        /// It is already retrying by itself: cut the wait short.
        now: bool,
    },
    /// Stop delivering the note and forget it.
    Cancel,
    /// Forget the stored note.
    Discard,
    /// Clear a failure that left nothing stored.
    Dismiss,
}

impl Action {
    /// The button's text.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Retry { now: true } => "Retry now",
            Self::Retry { now: false } => "Retry",
            Self::Cancel => "Cancel",
            Self::Discard => "Discard",
            Self::Dismiss => "Dismiss",
        }
    }

    /// Whether it is the card's primary action.
    #[must_use]
    pub const fn is_primary(self) -> bool {
        matches!(self, Self::Retry { now: false })
    }
}

/// What becomes of a transcript that just arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arrival {
    /// Show it for review ([`NoteState::Ready`]).
    Review(NoteState),
    /// Send it to the agent as a prompt (trimmed), then forget the note.
    Send(String),
    /// Nothing was heard: forget the note and say so.
    Empty,
}

/// The note for a dictation that heard nothing, when there is no card to
/// say so.
pub const NOTHING_HEARD: &str = "Didn't catch that";

/// The transcript card's note for a take cut short by the page being
/// hidden: the screen turning off (the power button, or a timeout the
/// wake lock couldn't prevent) or another app.
pub const SCREEN_OFF: &str = "Recording stopped when the screen turned off or you switched apps.";

impl Arrival {
    /// What to do with `text`: `send` when it goes without review (the
    /// take sends, and no older note waits for review ahead of it); a
    /// review card shows `note`.
    #[must_use]
    pub fn of(text: String, send: bool, note: Option<&'static str>) -> Self {
        let trimmed = text.trim();
        if !send {
            Self::Review(NoteState::Ready { text, note })
        } else if trimmed.is_empty() {
            Self::Empty
        } else {
            Self::Send(trimmed.to_owned())
        }
    }
}

/// `m:ss` for a recording length.
#[must_use]
pub fn clock(ms: u64) -> String {
    let seconds = ms / 1_000;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// The CSS step (`0..=10`) for a meter bar of `percent`.
#[must_use]
pub fn bar_step(percent: u8) -> u8 {
    percent.min(100).div_ceil(10)
}

/// Appends a transcript to the composer text, with a space
/// between it and what was already typed.
#[must_use]
pub fn append_transcript(composer: &str, transcript: &str) -> String {
    let transcript = transcript.trim();
    if composer.trim().is_empty() {
        return transcript.to_owned();
    }
    if composer.ends_with(char::is_whitespace) {
        format!("{composer}{transcript}")
    } else {
        format!("{composer} {transcript}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip() -> AudioId {
        AudioId::new("9ca09978-ef89-46fd-9a85-118b84bee7b4").unwrap()
    }

    fn note(key: u32, state: NoteState) -> Note {
        Note {
            key,
            id: clip(),
            duration_ms: 42_000,
            send: true,
            stopped: None,
            state,
        }
    }

    fn failed(parked: bool) -> NoteState {
        NoteState::Failed {
            message: "No.".into(),
            parked,
        }
    }

    fn ready(text: &str) -> NoteState {
        NoteState::Ready {
            text: text.into(),
            note: None,
        }
    }

    #[test]
    fn the_mic_state_crosses_as_a_code() {
        for mic in [Mic::Ready, Mic::Live, Mic::Busy] {
            assert_eq!(Mic::from_code(mic.code()), mic);
        }
        assert_eq!(Mic::from_code(7), Mic::Busy);
    }

    /// The mic records again as soon as a take is saved: notes on their
    /// way don't hold it.
    #[test]
    fn the_mic_waits_only_for_the_take_to_be_saved() {
        assert_eq!(Take::Idle.mic(), Mic::Ready);
        assert_eq!(Take::Opening.mic(), Mic::Live);
        assert_eq!(Take::Recording.mic(), Mic::Live, "a second tap stops");
        assert_eq!(Take::Saving.mic(), Mic::Busy);
        assert_eq!(Take::Failed("No.".into()).mic(), Mic::Ready);
        assert!(Take::Idle.can_start());
        assert!(Take::Failed("No.".into()).can_start());
        for take in [Take::Opening, Take::Recording, Take::Saving] {
            assert!(!take.can_start(), "{take:?}");
        }
    }

    #[test]
    fn the_take_has_a_row_of_its_own() {
        assert_eq!(Take::Idle.strip(true), Strip::Empty);
        assert_eq!(
            Take::Opening.strip(true),
            Strip::Live {
                opening: true,
                send: true
            }
        );
        assert_eq!(
            Take::Recording.strip(false),
            Strip::Live {
                opening: false,
                send: false
            }
        );
        assert_eq!(
            Take::Saving.strip(true),
            Strip::Status {
                text: "Saving…".into(),
                busy: false,
                cancel: None,
            },
            "nothing is stored yet, so nothing to cancel"
        );
        let Strip::Card { actions, alert, .. } = Take::Failed("Blocked.".into()).strip(true) else {
            panic!("a card")
        };
        assert!(alert);
        assert_eq!(actions, [Action::Dismiss]);
    }

    /// Each note's row is labelled with its length, so a stack of them
    /// tells apart.
    #[test]
    fn notes_show_their_length_and_progress() {
        assert_eq!(
            note(1, NoteState::Queued).strip(),
            Strip::Status {
                text: "0:42 · Queued".into(),
                busy: false,
                cancel: Some(Action::Discard),
            }
        );
        assert_eq!(
            note(1, NoteState::Uploading(Progress { sent: 1, total: 4 })).strip(),
            Strip::Status {
                text: "0:42 · Uploading… 25%".into(),
                busy: false,
                cancel: Some(Action::Cancel),
            }
        );
        assert_eq!(
            note(1, NoteState::Transcribing).strip(),
            Strip::Status {
                text: "0:42 · Transcribing…".into(),
                busy: true,
                cancel: Some(Action::Cancel),
            }
        );
        let Strip::Card {
            actions,
            note: line,
            alert,
            ..
        } = note(
            1,
            NoteState::Waiting {
                message: "Offline.".into(),
            },
        )
        .strip()
        else {
            panic!("a card")
        };
        assert_eq!(actions, [Action::Retry { now: true }, Action::Discard]);
        assert!(line.is_some() && !alert);
        for parked in [false, true] {
            let Strip::Card {
                actions,
                alert,
                title,
                ..
            } = note(1, failed(parked)).strip()
            else {
                panic!("a card")
            };
            assert!(alert);
            assert_eq!(title.as_deref(), Some("Dictation (0:42) not transcribed"));
            assert!(actions[0].is_primary());
            assert_eq!(actions[0].label(), "Retry");
            assert_eq!(actions[1], Action::Discard);
        }
        assert_eq!(
            note(1, ready("hi")).strip(),
            Strip::Transcript {
                title: "Transcript (0:42)".into(),
                text: "hi".into(),
                note: None,
            }
        );
    }

    /// Notes stack oldest first, with the take nearest the composer.
    #[test]
    fn rows_stack_notes_above_the_take() {
        let notes = [note(4, failed(true)), note(7, NoteState::Queued)];
        let keys: Vec<_> = rows(&Take::Recording, &notes, true)
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(keys, [RowKey::Note(4), RowKey::Note(7), RowKey::Take]);
        let keys: Vec<_> = rows(&Take::Idle, &notes, true)
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(
            keys,
            [RowKey::Note(4), RowKey::Note(7)],
            "idle: no take row"
        );
        assert_eq!(rows(&Take::Idle, &[], true), []);
    }

    /// One at a time, oldest first, so prompts arrive in the order they
    /// were said. A failure holds the queue; a review card doesn't.
    #[test]
    fn notes_are_delivered_in_order() {
        assert_eq!(next_to_deliver(&[]), None);
        assert_eq!(
            next_to_deliver(&[note(1, NoteState::Queued), note(2, NoteState::Queued)]),
            Some(1)
        );
        assert_eq!(
            next_to_deliver(&[note(1, ready("hi")), note(2, NoteState::Queued)]),
            Some(2),
            "a card waiting for review doesn't hold the queue"
        );
        for ahead in [
            NoteState::Transcribing,
            NoteState::Uploading(Progress { sent: 0, total: 1 }),
            NoteState::Waiting {
                message: "Offline.".into(),
            },
            failed(true),
            failed(false),
        ] {
            assert_eq!(
                next_to_deliver(&[note(1, ahead.clone()), note(2, NoteState::Queued)]),
                None,
                "{ahead:?}"
            );
        }
    }

    /// The screen stays on from the tap until the take is saved, and while
    /// a note uploads or transcribes; not for cards waiting on the user.
    #[test]
    fn the_screen_stays_on_while_something_is_under_way() {
        for take in [Take::Opening, Take::Recording, Take::Saving] {
            assert!(keeps_awake(&take, &[]), "{take:?}");
        }
        for state in [
            NoteState::Queued,
            NoteState::Uploading(Progress { sent: 0, total: 1 }),
            NoteState::Transcribing,
        ] {
            assert!(
                keeps_awake(&Take::Idle, &[note(1, state.clone())]),
                "{state:?}"
            );
        }
        assert!(!keeps_awake(&Take::Idle, &[]));
        assert!(!keeps_awake(&Take::Failed("No.".into()), &[]));
        for state in [
            ready("hi"),
            failed(false),
            failed(true),
            NoteState::Waiting {
                message: "Offline.".into(),
            },
        ] {
            assert!(
                !keeps_awake(&Take::Idle, &[note(1, state.clone())]),
                "{state:?}"
            );
        }
        assert!(
            !keeps_awake(
                &Take::Idle,
                &[note(1, failed(true)), note(2, NoteState::Queued)]
            ),
            "queued behind a failure"
        );
    }

    #[test]
    fn a_transcript_that_sends_is_trimmed() {
        assert_eq!(
            Arrival::of(" run the tests\n".into(), true, None),
            Arrival::Send("run the tests".into())
        );
    }

    #[test]
    fn nothing_heard_is_not_sent() {
        for heard in ["", "  \n "] {
            assert_eq!(Arrival::of(heard.into(), true, None), Arrival::Empty);
        }
        assert_eq!(NOTHING_HEARD, "Didn't catch that");
    }

    /// "Review dictation before sending" (or an older note waiting for
    /// review): the transcript, even an empty one, waits on its card,
    /// with a note saying why the take stopped if the user may not know.
    #[test]
    fn a_reviewed_transcript_waits_on_its_card() {
        for heard in ["run the tests", ""] {
            assert_eq!(
                Arrival::of(heard.into(), false, None),
                Arrival::Review(ready(heard))
            );
        }
        assert_eq!(
            Arrival::of("so the".into(), false, Some(SCREEN_OFF)),
            Arrival::Review(NoteState::Ready {
                text: "so the".into(),
                note: Some(SCREEN_OFF),
            })
        );
    }

    #[test]
    fn clock_and_bars() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(7_900), "0:07");
        assert_eq!(clock(125_000), "2:05");
        assert_eq!(bar_step(0), 0);
        assert_eq!(bar_step(1), 1);
        assert_eq!(bar_step(55), 6);
        assert_eq!(bar_step(100), 10);
        assert_eq!(bar_step(255), 10);
    }

    #[test]
    fn transcripts_append_to_what_was_typed() {
        assert_eq!(append_transcript("", " run the tests "), "run the tests");
        assert_eq!(append_transcript("   ", "go"), "go");
        assert_eq!(append_transcript("Then", "run them"), "Then run them");
        assert_eq!(append_transcript("Then\n", "run them"), "Then\nrun them");
    }
}
