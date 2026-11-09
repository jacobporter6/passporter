//! The composer's photo, pure so it is tested on the host: where its
//! upload is, whether Send may go, and what the prompt carries.

use protocol::{Action, ActionKind, AttachmentId, Attachments, PromptText, ValidationError};

/// Longest edge of the image the phone uploads, in pixels: plenty for an
/// agent to read a screenshot or see a board, small over mobile data.
pub const MAX_EDGE: u32 = 2048;
/// JPEG quality of the upload (0 to 1).
pub const JPEG_QUALITY: f64 = 0.85;
/// Longest edge of the thumbnail drawn in the composer, in CSS pixels.
pub const THUMB_EDGE: u32 = 56;
/// What a photo sent without words says, so the agent knows to look.
pub const PHOTO_ONLY_TEXT: &str = "Please look at the attached image.";

/// Where a photo's upload is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Being shrunk and sent to the bridge.
    Uploading,
    /// On the bridge: a prompt may carry it.
    Ready,
    /// It could not be sent; the text says why.
    Failed(String),
}

impl Phase {
    /// The preview's classes.
    #[must_use]
    pub const fn class(&self) -> &'static str {
        match self {
            Self::Uploading => "attach-preview attach-uploading",
            Self::Ready => "attach-preview",
            Self::Failed(_) => "attach-preview attach-failed",
        }
    }

    /// The preview's status line.
    #[must_use]
    pub fn status(&self) -> &str {
        match self {
            Self::Uploading => "Uploading photo…",
            Self::Ready => "Photo attached",
            Self::Failed(why) => why,
        }
    }
}

/// The photo attached to the composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Photo {
    /// Its id, picked here: also the upload's idempotency key.
    pub id: AttachmentId,
    /// Its upload.
    pub phase: Phase,
}

impl Photo {
    /// A photo just picked, uploading.
    #[must_use]
    pub const fn new(id: AttachmentId) -> Self {
        Self {
            id,
            phase: Phase::Uploading,
        }
    }

    /// The upload of photo `id` ended with `outcome` (a failure's text).
    /// Ignored unless this is still that photo: a removed or replaced
    /// photo's late answer changes nothing.
    pub fn settle(&mut self, id: &AttachmentId, outcome: Result<(), String>) {
        if &self.id == id {
            self.phase = outcome.map_or_else(Phase::Failed, |()| Phase::Ready);
        }
    }

    /// The failure's text, if the upload failed.
    #[must_use]
    pub fn failure(&self) -> Option<&str> {
        match &self.phase {
            Phase::Failed(why) => Some(why),
            Phase::Uploading | Phase::Ready => None,
        }
    }
}

/// What the Send button may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Nothing to send.
    Empty,
    /// The photo is still uploading.
    Waiting,
    /// The photo failed: remove it (or attach it again) first.
    Blocked,
    /// Send.
    Ready,
}

/// Whether the composer's `text` and `photo` may be sent now.
#[must_use]
pub fn gate(text: &str, photo: Option<&Photo>) -> Gate {
    match photo.map(|photo| &photo.phase) {
        None if text.trim().is_empty() => Gate::Empty,
        None | Some(Phase::Ready) => Gate::Ready,
        Some(Phase::Uploading) => Gate::Waiting,
        Some(Phase::Failed(_)) => Gate::Blocked,
    }
}

/// The prompt text for the composer's `text`: as typed, or
/// [`PHOTO_ONLY_TEXT`] when only a photo goes.
///
/// # Errors
///
/// The text is not a valid prompt (too long, a control character), or
/// empty without a photo.
pub fn prompt_text(text: &str, with_photo: bool) -> Result<PromptText, ValidationError> {
    let text = text.trim_end();
    if with_photo && text.trim().is_empty() {
        PromptText::new(PHOTO_ONLY_TEXT)
    } else {
        PromptText::new(text)
    }
}

/// `action` carrying `photo`, if it is a prompt and the photo is on the
/// bridge; otherwise `action` as it was.
#[must_use]
pub fn attach(mut action: Action, photo: Option<&Photo>) -> Action {
    if let (ActionKind::Prompt(prompt), Some(photo)) = (&mut action.kind, photo)
        && photo.phase == Phase::Ready
        && let Ok(attachments) = Attachments::new(vec![photo.id.clone()])
    {
        prompt.attachments = attachments;
    }
    action
}

/// `width` x `height` shrunk to fit within `max` on its longest edge, the
/// aspect ratio kept (each edge at least 1); never enlarged.
#[must_use]
pub fn fit(width: u32, height: u32, max: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= max || longest == 0 {
        return (width.max(1), height.max(1));
    }
    let scale = |edge: u32| {
        let scaled = u64::from(edge) * u64::from(max) / u64::from(longest);
        u32::try_from(scaled).unwrap_or(max).max(1)
    };
    (scale(width), scale(height))
}

/// What to tell the user when the bridge answered an upload with
/// `status` (`None`: no answer at all).
#[must_use]
pub const fn failure_text(status: Option<u16>) -> &'static str {
    match status {
        None => "Couldn't reach the bridge. Remove the photo and try again.",
        Some(401 | 403) => "This phone isn't signed in to the bridge.",
        Some(413) => "That photo is too large.",
        Some(415 | 422) => "That file isn't a photo the bridge can read.",
        Some(429) => "Too many photos at once. Try again in a minute.",
        Some(507) => "The bridge has no room for more photos right now.",
        Some(_) => "The bridge couldn't take the photo.",
    }
}

#[cfg(test)]
mod tests {
    use protocol::{AgentId, IdempotencyKey, Origin, PromptPayload, Seq, Timestamp};

    use super::*;

    const A: &str = "00000000-0000-4000-8000-0000000000a1";
    const B: &str = "00000000-0000-4000-8000-0000000000b2";

    fn id(s: &str) -> AttachmentId {
        AttachmentId::new(s).unwrap()
    }

    fn ready(s: &str) -> Photo {
        Photo {
            id: id(s),
            phase: Phase::Ready,
        }
    }

    fn prompt(text: &str) -> Action {
        Action {
            idempotency_key: IdempotencyKey::new("9ca09978-ef89-46fd-9a85-118b84bee7b4").unwrap(),
            agent_id: "w1:p1".parse::<AgentId>().unwrap(),
            expected_seq: Seq(3),
            created_at: Timestamp::from_unix_millis(1),
            kind: ActionKind::Prompt(PromptPayload::text(PromptText::new(text).unwrap())),
            origin: Origin::App,
        }
    }

    #[test]
    fn send_waits_for_the_upload() {
        assert_eq!(gate("", None), Gate::Empty);
        assert_eq!(gate("  \n", None), Gate::Empty);
        assert_eq!(gate("hi", None), Gate::Ready);
        let mut photo = Photo::new(id(A));
        assert_eq!(gate("", Some(&photo)), Gate::Waiting);
        assert_eq!(gate("hi", Some(&photo)), Gate::Waiting);
        photo.settle(&id(A), Ok(()));
        assert_eq!(gate("", Some(&photo)), Gate::Ready);
        photo.settle(&id(A), Err("no".into()));
        assert_eq!(gate("hi", Some(&photo)), Gate::Blocked);
        assert_eq!(photo.failure(), Some("no"));
    }

    #[test]
    fn the_preview_says_where_the_upload_is() {
        assert_eq!(Phase::Uploading.status(), "Uploading photo…");
        assert!(Phase::Uploading.class().contains("attach-uploading"));
        assert_eq!(Phase::Ready.class(), "attach-preview");
        let failed = Phase::Failed("Too big.".into());
        assert_eq!(failed.status(), "Too big.");
        assert!(failed.class().contains("attach-failed"));
    }

    #[test]
    fn a_late_answer_for_another_photo_changes_nothing() {
        let mut photo = Photo::new(id(B));
        photo.settle(&id(A), Ok(()));
        assert_eq!(photo.phase, Phase::Uploading);
        assert_eq!(photo.failure(), None);
    }

    #[test]
    fn a_photo_alone_says_to_look_at_it() {
        assert_eq!(prompt_text("", true).unwrap().as_str(), PHOTO_ONLY_TEXT);
        assert_eq!(prompt_text("  ", true).unwrap().as_str(), PHOTO_ONLY_TEXT);
        assert_eq!(prompt_text("why?\n", true).unwrap().as_str(), "why?");
        assert_eq!(prompt_text("why?  ", false).unwrap().as_str(), "why?");
        assert!(prompt_text("", false).is_err());
        assert!(prompt_text("\u{1b}[A", true).is_err());
    }

    #[test]
    fn only_a_ready_photo_rides_on_a_prompt() {
        let carried = attach(prompt("see"), Some(&ready(A)));
        let ActionKind::Prompt(payload) = &carried.kind else {
            panic!("a prompt");
        };
        assert_eq!(payload.attachments.as_slice(), [id(A)]);
        let uploading = attach(prompt("see"), Some(&Photo::new(id(A))));
        assert_eq!(uploading, prompt("see"));
        assert_eq!(attach(prompt("see"), None), prompt("see"));
        let mut interrupt = prompt("x");
        interrupt.kind = ActionKind::Interrupt(protocol::InterruptPayload {});
        assert_eq!(attach(interrupt.clone(), Some(&ready(A))), interrupt);
    }

    #[test]
    fn images_shrink_to_fit_and_never_grow() {
        assert_eq!(fit(4032, 3024, MAX_EDGE), (2048, 1536));
        assert_eq!(fit(3024, 4032, MAX_EDGE), (1536, 2048));
        assert_eq!(fit(800, 600, MAX_EDGE), (800, 600));
        assert_eq!(fit(10_000, 3, 100), (100, 1));
        assert_eq!(fit(0, 0, 100), (1, 1));
    }

    #[test]
    fn failures_read_as_sentences() {
        for status in [
            None,
            Some(401),
            Some(413),
            Some(415),
            Some(429),
            Some(500),
            Some(507),
        ] {
            let text = failure_text(status);
            assert!(text.ends_with('.'), "{text}");
        }
        assert_ne!(failure_text(Some(413)), failure_text(Some(415)));
    }
}
