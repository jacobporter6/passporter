//! Where one agent's dictation is ([`Phase`]), what its strip shows
//! ([`Strip`], drawn by [`super::panel`]) and what the app's mic button
//! needs to know of it ([`Mic`]). Pure, so tested on the host.

use protocol::AudioId;

use super::upload::Progress;

/// What the dictation strip shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Phase {
    /// Nothing going on.
    #[default]
    Idle,
    /// Waiting for the microphone (the permission prompt, the first time).
    Opening,
    /// Recording; the meter is live.
    Recording,
    /// Stopped; saving the clip on the device.
    Saving,
    /// Sending the clip.
    Uploading(Progress),
    /// Waiting for the transcript.
    Transcribing,
    /// The bridge is unreachable; retrying by itself.
    Waiting {
        /// The stored clip.
        id: AudioId,
        /// Why, for people.
        message: String,
    },
    /// The transcript, for the user to review (the "Review dictation
    /// before sending" setting, or a clip left from an earlier visit).
    Ready {
        /// The stored clip.
        id: AudioId,
        /// What the bridge heard (may be empty).
        text: String,
        /// Why the take stopped, when the user may not know (see
        /// [`SCREEN_OFF`]).
        note: Option<&'static str>,
    },
    /// Stopped short. With `id`, the clip is stored and can be retried.
    Failed {
        /// The stored clip, if there is one.
        id: Option<AudioId>,
        /// What happened, for people.
        message: String,
    },
}

impl Phase {
    /// Whether the mic button may start (or stop) a recording now.
    #[must_use]
    pub const fn mic_enabled(&self) -> bool {
        matches!(
            self,
            Self::Idle | Self::Opening | Self::Recording | Self::Failed { id: None, .. }
        )
    }

    /// Whether Cancel may discard the clip now: while it uploads or
    /// transcribes (while saving there is nothing stored yet).
    #[must_use]
    pub const fn can_cancel(&self) -> bool {
        matches!(self, Self::Uploading(_) | Self::Transcribing)
    }

    /// Whether the microphone is (being) used.
    #[must_use]
    pub const fn is_live(&self) -> bool {
        matches!(self, Self::Opening | Self::Recording)
    }

    /// Whether a take is under way, so the screen is kept on: from the
    /// tap until the transcript is in (or the take is cancelled or fails).
    /// A card (a transcript to review, a failure, an upload waiting for
    /// the bridge) waits for the user with the clip safe on the phone, so
    /// the screen may sleep.
    #[must_use]
    pub const fn keeps_awake(&self) -> bool {
        matches!(
            self,
            Self::Opening
                | Self::Recording
                | Self::Saving
                | Self::Uploading(_)
                | Self::Transcribing
        )
    }

    /// What the mic button can do in this phase.
    #[must_use]
    pub const fn mic(&self) -> Mic {
        if self.is_live() {
            Mic::Live
        } else if self.mic_enabled() {
            Mic::Ready
        } else {
            Mic::Busy
        }
    }
}

/// The mic button's state: all the app needs to know of a [`Phase`]. It
/// crosses from the dictation WASM to the page as [`Mic::code`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mic {
    /// Tap or hold to record.
    #[default]
    Ready,
    /// Recording (or opening): tap to stop.
    Live,
    /// A clip is on its way: wait for its card.
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

/// What the strip shows for a phase: a few shapes the phases share.
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
        /// Offer Cancel, which discards the clip.
        cancel: bool,
    },
    /// A message with actions on the stored clip `id`, if any.
    Card {
        /// The stored clip.
        id: Option<AudioId>,
        /// Announce it as an alert (a failure), not a status.
        alert: bool,
        /// A heading.
        title: Option<&'static str>,
        /// What happened.
        message: String,
        /// A second line.
        note: Option<&'static str>,
        /// The buttons, in order.
        actions: Vec<Action>,
    },
    /// The transcript card.
    Transcript {
        /// The stored clip.
        id: AudioId,
        /// What the bridge heard (may be empty).
        text: String,
        /// Why the take stopped, if the user may not know.
        note: Option<&'static str>,
    },
}

/// A button on a [`Strip::Card`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Deliver the stored clip again (now, if it is waiting to).
    Retry {
        /// It is already retrying by itself: cut the wait short.
        now: bool,
    },
    /// Forget the stored clip.
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

impl Strip {
    /// What the strip shows in `phase`; `send` when stopping the take
    /// sends its transcript (no review).
    #[must_use]
    pub fn of(phase: &Phase, send: bool) -> Self {
        let status = |text: &str, busy| Self::Status {
            text: text.to_owned(),
            busy,
            cancel: phase.can_cancel(),
        };
        match phase.clone() {
            Phase::Idle => Self::Empty,
            Phase::Opening => Self::Live {
                opening: true,
                send,
            },
            Phase::Recording => Self::Live {
                opening: false,
                send,
            },
            Phase::Saving => status("Saving…", false),
            Phase::Uploading(progress) => {
                let percent = progress.sent.saturating_mul(100) / progress.total.max(1);
                status(&format!("Uploading… {percent}%"), false)
            }
            Phase::Transcribing => status("Transcribing…", true),
            Phase::Waiting { id, message } => Self::Card {
                id: Some(id),
                alert: false,
                title: None,
                message,
                note: Some("Retrying by itself."),
                actions: vec![Action::Retry { now: true }, Action::Discard],
            },
            Phase::Ready { id, text, note } => Self::Transcript { id, text, note },
            Phase::Failed {
                id: Some(id),
                message,
            } => Self::Card {
                id: Some(id),
                alert: true,
                title: Some("Dictation not transcribed"),
                message,
                note: None,
                actions: vec![Action::Retry { now: false }, Action::Discard],
            },
            Phase::Failed { id: None, message } => Self::Card {
                id: None,
                alert: true,
                title: None,
                message,
                note: None,
                actions: vec![Action::Dismiss],
            },
        }
    }
}

/// What becomes of a transcript that just arrived for the stored clip
/// `id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arrival {
    /// Show it for review ([`Phase::Ready`]).
    Review(Phase),
    /// Send it to the agent as a prompt, then forget the clip.
    Send {
        /// The stored clip.
        id: AudioId,
        /// What to send, trimmed.
        text: String,
    },
    /// Nothing was heard: forget the clip and say so.
    Empty {
        /// The stored clip.
        id: AudioId,
    },
}

/// The note for a dictation that heard nothing, when there is no card to
/// say so.
pub const NOTHING_HEARD: &str = "Didn't catch that";

/// The transcript card's note for a take cut short by the page being
/// hidden: the screen turning off (the power button, or a timeout the
/// wake lock couldn't prevent) or another app.
pub const SCREEN_OFF: &str = "Recording stopped when the screen turned off or you switched apps.";

impl Arrival {
    /// What to do with `text`, transcribed from the clip `id`: `send` when
    /// the take sends without review; a review card shows `note`.
    #[must_use]
    pub fn of(id: AudioId, text: String, send: bool, note: Option<&'static str>) -> Self {
        let trimmed = text.trim();
        if !send {
            Self::Review(Phase::Ready { id, text, note })
        } else if trimmed.is_empty() {
            Self::Empty { id }
        } else {
            Self::Send {
                text: trimmed.to_owned(),
                id,
            }
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

    #[test]
    fn the_mic_state_crosses_as_a_code() {
        for mic in [Mic::Ready, Mic::Live, Mic::Busy] {
            assert_eq!(Mic::from_code(mic.code()), mic);
        }
        assert_eq!(Mic::from_code(7), Mic::Busy);
        assert_eq!(Phase::Opening.mic(), Mic::Live);
        assert_eq!(Phase::Idle.mic(), Mic::Ready);
        assert_eq!(Phase::Saving.mic(), Mic::Busy);
    }

    #[test]
    fn phases_share_a_few_strips() {
        let id = AudioId::new("9ca09978-ef89-46fd-9a85-118b84bee7b4").unwrap();
        assert_eq!(Strip::of(&Phase::Idle, true), Strip::Empty);
        assert_eq!(
            Strip::of(&Phase::Opening, true),
            Strip::Live {
                opening: true,
                send: true
            }
        );
        assert_eq!(
            Strip::of(&Phase::Uploading(Progress { sent: 1, total: 4 }), true),
            Strip::Status {
                text: "Uploading… 25%".into(),
                busy: false,
                cancel: true,
            }
        );
        let Strip::Card { actions, note, .. } = Strip::of(
            &Phase::Waiting {
                id: id.clone(),
                message: "Offline.".into(),
            },
            true,
        ) else {
            panic!("a card")
        };
        assert_eq!(actions, [Action::Retry { now: true }, Action::Discard]);
        assert!(note.is_some());
        let Strip::Card { actions, alert, .. } = Strip::of(
            &Phase::Failed {
                id: Some(id),
                message: "No.".into(),
            },
            true,
        ) else {
            panic!("a card")
        };
        assert!(alert);
        assert!(actions[0].is_primary());
        assert_eq!(actions[0].label(), "Retry");
        let Strip::Card { actions, id, .. } = Strip::of(
            &Phase::Failed {
                id: None,
                message: "Blocked.".into(),
            },
            true,
        ) else {
            panic!("a card")
        };
        assert_eq!((actions.as_slice(), id), (&[Action::Dismiss][..], None));
    }

    fn clip() -> AudioId {
        AudioId::new("9ca09978-ef89-46fd-9a85-118b84bee7b4").unwrap()
    }

    /// The strip and mic through one take that sends: tap, record, tap
    /// (the strip's button says Send), transcribe (Cancel offered), sent.
    #[test]
    fn a_take_records_then_sends_its_transcript() {
        let send = true;
        assert_eq!(Phase::Idle.mic(), Mic::Ready);
        // Tap: the microphone opens, then records.
        assert_eq!(Phase::Opening.mic(), Mic::Live);
        assert_eq!(
            Strip::of(&Phase::Recording, send),
            Strip::Live {
                opening: false,
                send: true
            }
        );
        assert_eq!(Phase::Recording.mic(), Mic::Live, "a second tap stops");
        // Tap again: saved, uploaded, transcribed, the mic waiting.
        for phase in [
            Phase::Saving,
            Phase::Uploading(Progress { sent: 0, total: 1 }),
            Phase::Transcribing,
        ] {
            assert_eq!(phase.mic(), Mic::Busy);
        }
        assert_eq!(
            Strip::of(&Phase::Transcribing, send),
            Strip::Status {
                text: "Transcribing…".into(),
                busy: true,
                cancel: true,
            }
        );
        // No card: the transcript is sent, trimmed, and the clip forgotten.
        assert_eq!(
            Arrival::of(clip(), " run the tests\n".into(), send, None),
            Arrival::Send {
                id: clip(),
                text: "run the tests".into()
            }
        );
    }

    #[test]
    fn nothing_heard_is_not_sent() {
        for heard in ["", "  \n "] {
            assert_eq!(
                Arrival::of(clip(), heard.into(), true, None),
                Arrival::Empty { id: clip() }
            );
        }
        assert_eq!(NOTHING_HEARD, "Didn't catch that");
    }

    #[test]
    fn saving_cant_be_cancelled_but_the_upload_can() {
        let Strip::Status { cancel, .. } = Strip::of(&Phase::Saving, true) else {
            panic!("a status")
        };
        assert!(!cancel, "nothing is stored yet");
        let Strip::Status { cancel, .. } = Strip::of(&Phase::Transcribing, false) else {
            panic!("a status")
        };
        assert!(cancel, "also when reviewing");
        assert!(Phase::Uploading(Progress { sent: 0, total: 1 }).can_cancel());
        assert!(
            !Phase::Recording.can_cancel(),
            "the strip's own button stops it"
        );
        assert!(
            !Phase::Ready {
                id: clip(),
                text: "hi".into(),
                note: None,
            }
            .can_cancel(),
            "the card has Discard"
        );
    }

    /// "Review dictation before sending": the button stops, and the
    /// transcript (even an empty one) waits on its card.
    #[test]
    fn with_review_on_the_transcript_waits_for_the_user() {
        assert_eq!(
            Strip::of(&Phase::Recording, false),
            Strip::Live {
                opening: false,
                send: false
            }
        );
        for heard in ["run the tests", ""] {
            let ready = Phase::Ready {
                id: clip(),
                text: heard.into(),
                note: None,
            };
            assert_eq!(
                Arrival::of(clip(), heard.into(), false, None),
                Arrival::Review(ready.clone())
            );
            assert_eq!(
                Strip::of(&ready, false),
                Strip::Transcript {
                    id: clip(),
                    text: heard.into(),
                    note: None,
                }
            );
        }
    }

    /// A take cut short by the screen turning off waits on its card, with
    /// a note saying why it stopped.
    #[test]
    fn a_take_cut_short_says_why_on_its_card() {
        assert_eq!(
            Strip::of(
                &Phase::Ready {
                    id: clip(),
                    text: "so the".into(),
                    note: Some(SCREEN_OFF),
                },
                true
            ),
            Strip::Transcript {
                id: clip(),
                text: "so the".into(),
                note: Some(SCREEN_OFF),
            }
        );
        assert_eq!(
            Arrival::of(clip(), "so the".into(), false, Some(SCREEN_OFF)),
            Arrival::Review(Phase::Ready {
                id: clip(),
                text: "so the".into(),
                note: Some(SCREEN_OFF),
            })
        );
    }

    /// The screen stays on from the tap until the transcript is sent, and
    /// may sleep once the take is sent, cancelled (both idle), waits on a
    /// card for review, or failed.
    #[test]
    fn the_screen_stays_on_while_a_take_is_under_way() {
        for phase in [
            Phase::Opening,
            Phase::Recording,
            Phase::Saving,
            Phase::Uploading(Progress { sent: 0, total: 1 }),
            Phase::Transcribing,
        ] {
            assert!(phase.keeps_awake(), "{phase:?}");
        }
        for phase in [
            Phase::Idle,
            Phase::Ready {
                id: clip(),
                text: "hi".into(),
                note: None,
            },
            Phase::Failed {
                id: None,
                message: "No.".into(),
            },
            Phase::Failed {
                id: Some(clip()),
                message: "No.".into(),
            },
            Phase::Waiting {
                id: clip(),
                message: "Offline.".into(),
            },
        ] {
            assert!(!phase.keeps_awake(), "{phase:?}");
        }
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

    #[test]
    fn the_mic_waits_for_the_card() {
        assert!(Phase::Idle.mic_enabled());
        assert!(Phase::Recording.mic_enabled(), "to stop");
        let id = AudioId::new("9ca09978-ef89-46fd-9a85-118b84bee7b4").unwrap();
        assert!(
            !Phase::Ready {
                id: id.clone(),
                text: "hi".into(),
                note: None,
            }
            .mic_enabled()
        );
        assert!(!Phase::Transcribing.mic_enabled());
        assert!(
            !Phase::Failed {
                id: Some(id),
                message: String::new()
            }
            .mic_enabled()
        );
        assert!(
            Phase::Failed {
                id: None,
                message: String::new()
            }
            .mic_enabled()
        );
    }
}
