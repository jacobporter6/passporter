//! A dictation clip kept on the device (design §6.3, "record first,
//! transcribe later"): written to durable storage the moment recording
//! stops, before anything touches the network, and removed once the user
//! has used or discarded its transcript, or after seven days (the
//! bridge's own retention; it also clears clips of agents that are gone).

use core::future::Future;

use protocol::{AgentId, AudioId, Timestamp};
use serde::{Deserialize, Serialize};

/// Everything about a clip but its audio. Stored as JSON next to the
/// audio bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipMeta {
    /// The clip's id, made on the phone at record time, so a lost reply
    /// or a reload resumes the same upload.
    pub audio_id: AudioId,
    /// The agent whose composer the transcript fills.
    pub agent: AgentId,
    /// Length in ms.
    pub duration_ms: u64,
    /// When recording stopped (device clock).
    pub recorded_at: Timestamp,
    /// The bridge's transcript, once it arrived. The audio is dropped
    /// then: the bridge keeps its own copy for retries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
    /// Why the last delivery failed for good (people-facing). A clip with
    /// a failure waits for the user's Retry or Discard instead of being
    /// sent again on every visit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

/// A clip as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingClip {
    /// What it is.
    pub meta: ClipMeta,
    /// The encoded audio; empty once transcribed.
    pub audio: Vec<u8>,
}

impl PendingClip {
    /// Where the clip is in its life.
    #[must_use]
    pub fn stage(&self) -> Stage<'_> {
        match &self.meta.transcript {
            Some(text) => Stage::Transcribed(text),
            None => Stage::Recorded,
        }
    }
}

/// Where a stored clip is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage<'a> {
    /// Audio kept; upload and transcription still to do (or in progress).
    Recorded,
    /// The transcript is here, to send (or for the user to review).
    Transcribed(&'a str),
}

/// Why the clip store failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("saving the recording on this device failed: {0}")]
pub struct ClipStoreError(pub String);

/// Durable storage for clips: `IndexedDB` in the browser, memory in tests.
pub trait ClipStore {
    /// Saves `clip` (replacing one with the same id) and resolves once it
    /// is durable.
    fn put(&self, clip: &PendingClip) -> impl Future<Output = Result<(), ClipStoreError>>;

    /// What every stored clip is, without its audio, in no particular
    /// order. Records that no longer read back are skipped.
    fn list(&self) -> impl Future<Output = Result<Vec<ClipMeta>, ClipStoreError>>;

    /// One clip with its audio, if stored (and readable).
    fn get(
        &self,
        id: &AudioId,
    ) -> impl Future<Output = Result<Option<PendingClip>, ClipStoreError>>;

    /// Replaces the metadata of the clip `meta` names, if it is still
    /// stored (the audio stays as stored). A clip removed meanwhile (the
    /// user discarded it) stays removed: no metadata without audio.
    fn put_meta(&self, meta: &ClipMeta) -> impl Future<Output = Result<(), ClipStoreError>>;

    /// Stores `clip` and deletes the clip `old`, in one transaction: a
    /// clip that moves to a new id is never stored twice, nor lost.
    fn rename(
        &self,
        old: &AudioId,
        clip: &PendingClip,
    ) -> impl Future<Output = Result<(), ClipStoreError>>;

    /// Deletes the clip, if stored.
    fn remove(&self, id: &AudioId) -> impl Future<Output = Result<(), ClipStoreError>>;
}

#[cfg(test)]
pub(crate) mod tests {
    use core::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;

    use super::*;

    /// An in-memory store. Clones share the clips, like two page loads
    /// share `IndexedDB`.
    #[derive(Clone, Default)]
    pub(crate) struct MemoryClips {
        pub(crate) clips: Rc<RefCell<BTreeMap<String, PendingClip>>>,
        pub(crate) broken: Rc<core::cell::Cell<bool>>,
        /// Calls to `list`.
        pub(crate) lists: Rc<core::cell::Cell<u32>>,
    }

    impl ClipStore for MemoryClips {
        fn put(&self, clip: &PendingClip) -> impl Future<Output = Result<(), ClipStoreError>> {
            let result = if self.broken.get() {
                Err(ClipStoreError("quota exceeded".into()))
            } else {
                self.clips
                    .borrow_mut()
                    .insert(clip.meta.audio_id.to_string(), clip.clone());
                Ok(())
            };
            core::future::ready(result)
        }

        fn list(&self) -> impl Future<Output = Result<Vec<ClipMeta>, ClipStoreError>> {
            self.lists.set(self.lists.get() + 1);
            let metas = self
                .clips
                .borrow()
                .values()
                .map(|c| c.meta.clone())
                .collect();
            core::future::ready(Ok(metas))
        }

        fn get(
            &self,
            id: &AudioId,
        ) -> impl Future<Output = Result<Option<PendingClip>, ClipStoreError>> {
            core::future::ready(Ok(self.clips.borrow().get(id.as_str()).cloned()))
        }

        fn put_meta(&self, meta: &ClipMeta) -> impl Future<Output = Result<(), ClipStoreError>> {
            if let Some(clip) = self.clips.borrow_mut().get_mut(meta.audio_id.as_str()) {
                clip.meta = meta.clone();
            }
            core::future::ready(Ok(()))
        }

        fn rename(
            &self,
            old: &AudioId,
            clip: &PendingClip,
        ) -> impl Future<Output = Result<(), ClipStoreError>> {
            let mut clips = self.clips.borrow_mut();
            clips.remove(old.as_str());
            clips.insert(clip.meta.audio_id.to_string(), clip.clone());
            core::future::ready(Ok(()))
        }

        fn remove(&self, id: &AudioId) -> impl Future<Output = Result<(), ClipStoreError>> {
            self.clips.borrow_mut().remove(id.as_str());
            core::future::ready(Ok(()))
        }
    }

    #[test]
    fn meta_round_trips_as_json() {
        let meta = ClipMeta {
            audio_id: AudioId::new("9ca09978-ef89-46fd-9a85-118b84bee7b4").unwrap(),
            agent: "w1:p1".parse().unwrap(),
            duration_ms: 4_100,
            recorded_at: Timestamp::from_unix_millis(7),
            transcript: None,
            failure: None,
        };
        let json = serde_json::to_string(&meta).unwrap();
        assert!(!json.contains("transcript"), "{json}");
        assert_eq!(serde_json::from_str::<ClipMeta>(&json).unwrap(), meta);
        let clip = PendingClip {
            meta: ClipMeta {
                transcript: Some("hi".into()),
                ..meta
            },
            audio: Vec::new(),
        };
        assert_eq!(clip.stage(), Stage::Transcribed("hi"));
    }
}
