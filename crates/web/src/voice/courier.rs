//! Takes a recording from the microphone to a transcript (design §6.3):
//! store it on the device first, then upload it (resuming wherever the
//! bridge got to), then wait for the transcript and store that too. Every
//! step can fail and be run again from the top, including after a reload:
//! the stored clip is the only state.

use core::future::Future;

use protocol::{AgentId, AudioFailure, AudioId, ClipState, Timestamp};

use super::clip::{ClipMeta, ClipStore, ClipStoreError, PendingClip, Stage};
use super::recorder::Recording;
use super::upload::{self, AudioTransport, Progress, TranscriptPoll, UploadError};

/// Transcript polls before giving up for now (about four minutes with
/// [`poll_delay_ms`]): the bridge transcribes one clip at a time.
const MAX_POLLS: u32 = 90;

/// Fresh ids tried within one delivery when the bridge refuses the id.
const MAX_RENAMES: u32 = 2;

/// Times a clip is uploaded again within one delivery when the bridge
/// lost it (swept, or restarted mid-upload).
const MAX_REUPLOADS: u32 = 2;

/// How long to wait before transcript poll `attempt` (0-based): quick at
/// first (a short clip takes about a second), then every three seconds.
#[must_use]
pub fn poll_delay_ms(attempt: u32) -> u32 {
    attempt.saturating_add(1).saturating_mul(500).min(3_000)
}

/// Where a delivery is, for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// Sending audio.
    Uploading(Progress),
    /// Uploaded; waiting for the transcript.
    Transcribing,
}

/// Why a delivery stopped short of a transcript.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeliverError {
    /// The upload or a poll did not get through.
    #[error(transparent)]
    Upload(UploadError),
    /// The device's storage failed.
    #[error(transparent)]
    Store(#[from] ClipStoreError),
    /// The bridge could not transcribe the clip. Delivering it again
    /// queues it again.
    #[error("the bridge couldn't transcribe this recording")]
    Transcription,
    /// The decoded clip is longer than the bridge allows.
    #[error("this recording is longer than the bridge allows")]
    TooLong,
    /// Still not transcribed after a few minutes.
    #[error("the bridge is still transcribing; try again shortly")]
    Slow,
    /// The user discarded the clip while it was on its way.
    #[error("the recording was discarded")]
    Cancelled,
}

impl From<UploadError> for DeliverError {
    fn from(error: UploadError) -> Self {
        match error {
            UploadError::Cancelled => Self::Cancelled,
            error => Self::Upload(error),
        }
    }
}

impl DeliverError {
    /// Whether delivering again, unchanged, may succeed.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::Upload(error) => error.is_retryable(),
            Self::Store(_) | Self::Transcription | Self::Slow => true,
            Self::TooLong | Self::Cancelled => false,
        }
    }
}

/// Moves clips between the device's store and the bridge.
#[derive(Debug, Clone)]
pub struct Courier<S, T> {
    store: S,
    transport: T,
    chunk_bytes: usize,
    new_id: fn() -> Option<AudioId>,
}

impl<S: ClipStore, T: AudioTransport> Courier<S, T> {
    /// A courier over `store` and `transport`, uploading `chunk_bytes` at a
    /// time. `new_id` makes a fresh clip id when the bridge can't take one.
    pub const fn new(
        store: S,
        transport: T,
        chunk_bytes: usize,
        new_id: fn() -> Option<AudioId>,
    ) -> Self {
        Self {
            store,
            transport,
            chunk_bytes,
            new_id,
        }
    }

    /// Stores a finished recording as clip `id` for `agent`. Nothing goes
    /// to the network before this resolves.
    pub async fn keep(
        &self,
        agent: AgentId,
        id: AudioId,
        recording: Recording,
        now: Timestamp,
    ) -> Result<PendingClip, ClipStoreError> {
        let clip = PendingClip {
            meta: ClipMeta {
                audio_id: id,
                agent,
                duration_ms: recording.duration_ms,
                recorded_at: now,
                transcript: None,
                failure: None,
            },
            audio: recording.bytes,
        };
        self.store.put(&clip).await?;
        Ok(clip)
    }

    /// The stored clips for `agent`, oldest first, without their audio,
    /// from one read of the store.
    ///
    /// With `sweep_before`, every clip recorded before it, for any agent,
    /// is deleted first: by then the bridge has swept its copy, and a clip
    /// whose agent is gone (so no screen will ever show it) doesn't stay on
    /// the phone forever.
    pub async fn waiting(
        &self,
        agent: &AgentId,
        sweep_before: Option<Timestamp>,
    ) -> Result<Vec<ClipMeta>, ClipStoreError> {
        let mut clips = Vec::new();
        for meta in self.store.list().await? {
            if sweep_before.is_some_and(|cutoff| meta.recorded_at < cutoff) {
                self.store.remove(&meta.audio_id).await?;
            } else if &meta.agent == agent {
                clips.push(meta);
            }
        }
        clips.sort_by_key(|meta| meta.recorded_at);
        Ok(clips)
    }

    /// One stored clip, with its audio.
    pub async fn load(&self, id: &AudioId) -> Result<Option<PendingClip>, ClipStoreError> {
        self.store.get(id).await
    }

    /// Takes `clip` as far as a stored transcript and returns the
    /// transcribed clip. A clip that already has one comes straight back.
    /// `sleep(ms)` waits between transcript polls. Once `cancelled()` (the
    /// user discarded the clip) it stops, and never writes the clip back.
    ///
    /// If the bridge can't take the clip's id, the clip moves to a fresh
    /// one, in storage and in `clip`, which the caller keeps: a retry then
    /// sends the clip under its new id, whatever this attempt came to.
    pub async fn deliver<F: Future<Output = ()>>(
        &self,
        clip: &mut PendingClip,
        mut report: impl FnMut(Delivery),
        sleep: impl Fn(u32) -> F,
        cancelled: impl Fn() -> bool,
    ) -> Result<PendingClip, DeliverError> {
        if matches!(clip.stage(), Stage::Transcribed(_)) {
            return Ok(clip.clone());
        }
        // Renames don't count as re-uploads: that budget is for a bridge
        // that lost the clip.
        let (mut reuploads, mut renames) = (0, 0);
        'upload: loop {
            let uploaded = upload::upload(
                &self.transport,
                &*clip,
                self.chunk_bytes,
                |progress| report(Delivery::Uploading(progress)),
                &cancelled,
            )
            .await;
            match uploaded {
                Ok(_) => {}
                Err(UploadError::IdTaken) if renames < MAX_RENAMES => {
                    renames += 1;
                    self.rename(clip, &cancelled).await?;
                    continue 'upload;
                }
                Err(error) => return Err(error.into()),
            }
            report(Delivery::Transcribing);
            for attempt in 0..MAX_POLLS {
                if cancelled() {
                    return Err(DeliverError::Cancelled);
                }
                match upload::poll_transcript(&self.transport, &clip.meta.audio_id).await {
                    Ok(TranscriptPoll::Ready(transcript)) => {
                        // The bridge keeps the audio for retries; the
                        // device only needs the text now.
                        let done = PendingClip {
                            meta: ClipMeta {
                                transcript: Some(transcript.text),
                                failure: None,
                                ..clip.meta.clone()
                            },
                            audio: Vec::new(),
                        };
                        self.keep_transcript(&done, &cancelled).await?;
                        return Ok(done);
                    }
                    Ok(TranscriptPoll::Pending(status)) if status.state == ClipState::Uploading => {
                        // The bridge lost bytes (a restart mid-write).
                        if reuploads >= MAX_REUPLOADS {
                            return Err(UploadError::Gone.into());
                        }
                        reuploads += 1;
                        continue 'upload;
                    }
                    Ok(TranscriptPoll::Pending(_)) => sleep(poll_delay_ms(attempt)).await,
                    Ok(TranscriptPoll::Failed(status)) => {
                        return Err(match status.failure {
                            Some(AudioFailure::TooLong) => DeliverError::TooLong,
                            _ => DeliverError::Transcription,
                        });
                    }
                    Err(UploadError::Gone) if reuploads < MAX_REUPLOADS => {
                        reuploads += 1;
                        continue 'upload;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            return Err(DeliverError::Slow);
        }
    }

    /// Moves `clip` to a fresh id (the bridge can't take its id), in one
    /// store transaction, unless it was discarded meanwhile.
    async fn rename(
        &self,
        clip: &mut PendingClip,
        cancelled: &impl Fn() -> bool,
    ) -> Result<(), DeliverError> {
        let id = (self.new_id)().ok_or(UploadError::IdTaken)?;
        if cancelled() {
            return Err(DeliverError::Cancelled);
        }
        let old = core::mem::replace(&mut clip.meta.audio_id, id);
        if let Err(error) = self.store.rename(&old, clip).await {
            clip.meta.audio_id = old;
            return Err(error.into());
        }
        if cancelled() {
            self.store.remove(&clip.meta.audio_id).await?;
            return Err(DeliverError::Cancelled);
        }
        Ok(())
    }

    /// Notes on the stored clip that its delivery failed for good, so a
    /// later visit shows the failure instead of sending it again.
    pub async fn record_failure(
        &self,
        clip: &PendingClip,
        message: String,
    ) -> Result<(), ClipStoreError> {
        let meta = ClipMeta {
            failure: Some(message),
            ..clip.meta.clone()
        };
        self.store.put_meta(&meta).await
    }

    /// Forgets a failure recorded on `clip`, in storage and in `clip`: the
    /// user asked to try again, so a reload while the retry waits resumes
    /// delivering it by itself instead of showing the old failure.
    pub async fn clear_failure(&self, clip: &mut PendingClip) -> Result<(), ClipStoreError> {
        if clip.meta.failure.take().is_some() {
            self.store.put_meta(&clip.meta).await?;
        }
        Ok(())
    }

    /// Stores the transcribed `clip`, unless it was discarded meanwhile. A
    /// discard can land while the write is on its way (storage is
    /// asynchronous), so it is checked again after, and undone.
    async fn keep_transcript(
        &self,
        clip: &PendingClip,
        cancelled: &impl Fn() -> bool,
    ) -> Result<(), DeliverError> {
        if cancelled() {
            return Err(DeliverError::Cancelled);
        }
        self.store.put(clip).await?;
        if cancelled() {
            self.store.remove(&clip.meta.audio_id).await?;
            return Err(DeliverError::Cancelled);
        }
        Ok(())
    }

    /// Deletes the stored clip (its transcript was used or discarded).
    pub async fn forget(&self, id: &AudioId) -> Result<(), ClipStoreError> {
        self.store.remove(id).await
    }
}

#[cfg(test)]
mod tests {
    use core::cell::RefCell;

    use super::super::block_on;
    use super::super::clip::tests::MemoryClips;
    use super::super::recorder::tests::FakeMic;
    use super::super::recorder::{CaptureEvent, Dictaphone, RecorderConfig, Update};
    use super::super::upload::tests::{ChunkFailure, FakeBridge, ID, clip};
    use super::*;

    fn agent() -> AgentId {
        "w1:p1".parse().unwrap()
    }

    fn courier<'a>(
        store: &MemoryClips,
        bridge: &'a FakeBridge,
    ) -> Courier<MemoryClips, &'a FakeBridge> {
        Courier::new(store.clone(), bridge, 4, second_id)
    }

    fn no_wait(_: u32) -> core::future::Ready<()> {
        core::future::ready(())
    }

    fn never() -> bool {
        false
    }

    const SECOND: &str = "1ca09978-ef89-46fd-9a85-118b84bee7b4";
    const THIRD: &str = "2ca09978-ef89-46fd-9a85-118b84bee7b4";

    #[allow(clippy::unnecessary_wraps)] // the shape `Courier::new` takes
    fn second_id() -> Option<AudioId> {
        AudioId::new(SECOND).ok()
    }

    /// Records `chunks` from the fake microphone, as the UI does.
    fn record(chunks: &[&[u8]]) -> Recording {
        let mic = FakeMic::default();
        let phone = Dictaphone::new(&mic, RecorderConfig::default());
        let done = std::rc::Rc::new(RefCell::new(None));
        let sink = done.clone();
        block_on(phone.start(move |update| {
            if let Update::Finished(recording) = update {
                *sink.borrow_mut() = Some(recording);
            }
        }))
        .unwrap();
        for chunk in chunks {
            mic.frame(250.0, 0.2);
            mic.emit(CaptureEvent::Data(chunk.to_vec()));
        }
        phone.stop();
        done.borrow_mut().take().expect("recorded")
    }

    fn id() -> AudioId {
        AudioId::new(ID).unwrap()
    }

    #[test]
    fn a_recording_survives_a_reload_before_upload() {
        let store = MemoryClips::default();
        let bridge = FakeBridge::default();
        let recording = record(&[b"hello ", b"bridge"]);
        let kept = {
            let first_load = courier(&store, &bridge);
            let kept =
                block_on(first_load.keep(agent(), id(), recording, Timestamp::from_unix_millis(5)))
                    .unwrap();
            assert!(bridge.log.borrow().is_empty(), "stored before any request");
            // Offline: the upload fails, the clip stays.
            bridge.offline.set(true);
            let error = block_on(first_load.deliver(&mut kept.clone(), |_| {}, no_wait, never))
                .unwrap_err();
            assert!(error.is_retryable(), "{error:?}");
            kept
        };
        // The page reloads: a new courier over the same storage.
        bridge.offline.set(false);
        let second_load = courier(&store, &bridge);
        let waiting = block_on(second_load.waiting(&agent(), None)).unwrap();
        assert_eq!(waiting, core::slice::from_ref(&kept.meta));
        let reloaded = block_on(second_load.load(&waiting[0].audio_id))
            .unwrap()
            .expect("still stored");
        assert_eq!(reloaded, kept);
        assert_eq!(reloaded.audio, b"hello bridge");
        assert_eq!(
            block_on(second_load.waiting(&"w9:p9".parse().unwrap(), None)).unwrap(),
            []
        );

        let mut seen = Vec::new();
        let done =
            block_on(second_load.deliver(&mut reloaded.clone(), |d| seen.push(d), no_wait, never))
                .unwrap();
        assert_eq!(done.stage(), Stage::Transcribed("12 bytes heard"));
        assert_eq!(done.audio, b"", "the audio is dropped once transcribed");
        assert_eq!(bridge.received(ID).as_deref(), Some(&b"hello bridge"[..]));
        assert_eq!(seen.last(), Some(&Delivery::Transcribing));
        assert!(seen.contains(&Delivery::Uploading(Progress {
            sent: 12,
            total: 12
        })));

        // The transcript itself survives a reload too, without the network.
        bridge.offline.set(true);
        let third_load = courier(&store, &bridge);
        let waiting = block_on(third_load.waiting(&agent(), None)).unwrap();
        assert_eq!(waiting[0].transcript.as_deref(), Some("12 bytes heard"));
        let stored = block_on(third_load.load(&id())).unwrap().unwrap();
        let again =
            block_on(third_load.deliver(&mut stored.clone(), |_| {}, no_wait, never)).unwrap();
        assert_eq!(again, stored);

        block_on(third_load.forget(&id())).unwrap();
        assert_eq!(block_on(third_load.waiting(&agent(), None)).unwrap(), []);
    }

    #[test]
    fn delivery_resumes_after_a_failed_chunk() {
        let store = MemoryClips::default();
        let bridge = FakeBridge::default();
        let courier = courier(&store, &bridge);
        let kept = block_on(courier.keep(
            agent(),
            id(),
            record(&[b"0123456789"]),
            Timestamp::from_unix_millis(5),
        ))
        .unwrap();
        bridge.chunk_failure.set(Some(ChunkFailure {
            after: 1,
            lands: false,
        }));
        let error =
            block_on(courier.deliver(&mut kept.clone(), |_| {}, no_wait, never)).unwrap_err();
        assert!(error.is_retryable());
        assert_eq!(bridge.received(ID).map(|r| r.len()), Some(4));
        bridge.log.borrow_mut().clear();
        let done = block_on(courier.deliver(&mut kept.clone(), |_| {}, no_wait, never)).unwrap();
        assert_eq!(done.stage(), Stage::Transcribed("10 bytes heard"));
        let offsets: Vec<Option<u64>> = bridge
            .log
            .borrow()
            .iter()
            .filter_map(|logged| logged.offset.map(Some))
            .collect();
        assert_eq!(offsets, [Some(4), Some(8)], "only the missing bytes");
    }

    #[test]
    fn polls_wait_and_a_lost_clip_is_uploaded_again() {
        let store = MemoryClips::default();
        let bridge = FakeBridge::default();
        let courier = courier(&store, &bridge);
        let kept = block_on(courier.keep(
            agent(),
            id(),
            record(&[b"abc"]),
            Timestamp::from_unix_millis(5),
        ))
        .unwrap();
        bridge.busy_polls.set(1);
        let waits = RefCell::new(Vec::new());
        let done = block_on(courier.deliver(
            &mut kept.clone(),
            |_| {},
            |ms| {
                waits.borrow_mut().push(ms);
                // The bridge sweeps the clip while we wait.
                bridge.forget(ID);
                core::future::ready(())
            },
            never,
        ))
        .unwrap();
        assert_eq!(done.stage(), Stage::Transcribed("3 bytes heard"));
        assert_eq!(*waits.borrow(), [500]);
    }

    #[test]
    fn failed_transcriptions_are_retried_by_delivering_again() {
        let store = MemoryClips::default();
        let bridge = FakeBridge::default();
        let courier = courier(&store, &bridge);
        let kept = block_on(courier.keep(
            agent(),
            id(),
            record(&[b"abc"]),
            Timestamp::from_unix_millis(5),
        ))
        .unwrap();
        bridge.fail_transcription.set(true);
        let error =
            block_on(courier.deliver(&mut kept.clone(), |_| {}, no_wait, never)).unwrap_err();
        assert_eq!(error, DeliverError::Transcription);
        assert!(error.is_retryable());
        // Still stored with its audio, for the retry.
        assert_eq!(
            block_on(courier.load(&id())).unwrap().unwrap().audio,
            b"abc"
        );
        bridge.fail_transcription.set(false);
        assert!(block_on(courier.deliver(&mut kept.clone(), |_| {}, no_wait, never)).is_ok());
    }

    #[test]
    fn nothing_is_uploaded_when_the_device_cannot_store_it() {
        let store = MemoryClips::default();
        store.broken.set(true);
        let bridge = FakeBridge::default();
        let courier = courier(&store, &bridge);
        assert!(
            block_on(courier.keep(
                agent(),
                id(),
                record(&[b"x"]),
                Timestamp::from_unix_millis(5)
            ))
            .is_err()
        );
        assert!(bridge.log.borrow().is_empty());
    }

    #[test]
    fn clips_wait_oldest_first() {
        let store = MemoryClips::default();
        let bridge = FakeBridge::default();
        let courier = courier(&store, &bridge);
        let mut newer = clip(b"new".to_vec());
        newer.meta.recorded_at = Timestamp::from_unix_millis(9);
        let mut older = clip(b"old".to_vec());
        older.meta.audio_id = AudioId::new("0ca09978-ef89-46fd-9a85-118b84bee7b4").unwrap();
        older.meta.recorded_at = Timestamp::from_unix_millis(2);
        block_on(store.put(&newer)).unwrap();
        block_on(store.put(&older)).unwrap();
        let waiting = block_on(courier.waiting(&agent(), None)).unwrap();
        assert_eq!(waiting, [older.meta, newer.meta]);
    }

    #[test]
    fn a_taken_id_is_replaced_and_the_clip_stored_under_the_new_one() {
        let store = MemoryClips::default();
        let bridge = FakeBridge::default();
        let courier = courier(&store, &bridge);
        let kept = block_on(courier.keep(
            agent(),
            id(),
            record(&[b"abcdef"]),
            Timestamp::from_unix_millis(5),
        ))
        .unwrap();
        // A different clip already holds the id on the bridge.
        let mut other = clip(b"xy".to_vec());
        other.meta.audio_id = id();
        block_on(upload::upload(&&bridge, &other, 4, |_| {}, never)).unwrap();

        // The first chunk under the new id fails: the retry must send the
        // clip as re-keyed, not re-key it again and orphan a copy.
        bridge.chunk_failure.set(Some(ChunkFailure {
            after: 0,
            lands: false,
        }));
        let mut sending = kept.clone();
        let error = block_on(courier.deliver(&mut sending, |_| {}, no_wait, never)).unwrap_err();
        assert!(error.is_retryable(), "{error:?}");
        let done = block_on(courier.deliver(&mut sending, |_| {}, no_wait, never)).unwrap();
        let second = AudioId::new(SECOND).unwrap();
        assert_eq!(sending.meta.audio_id, second, "the caller's clip moved too");
        assert_eq!(done.meta.audio_id, second);
        assert_eq!(done.stage(), Stage::Transcribed("6 bytes heard"));
        assert_eq!(bridge.received(SECOND).as_deref(), Some(&b"abcdef"[..]));
        let waiting = block_on(courier.waiting(&agent(), None)).unwrap();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].audio_id, second, "the old record is gone");
    }

    #[test]
    fn a_failure_is_remembered_and_cleared_by_a_transcript() {
        let store = MemoryClips::default();
        let bridge = FakeBridge::default();
        let courier = courier(&store, &bridge);
        let kept = block_on(courier.keep(
            agent(),
            id(),
            record(&[b"abc"]),
            Timestamp::from_unix_millis(5),
        ))
        .unwrap();
        block_on(courier.record_failure(&kept, "Too long.".into())).unwrap();
        let waiting = block_on(courier.waiting(&agent(), None)).unwrap();
        assert_eq!(waiting[0].failure.as_deref(), Some("Too long."));
        let stored = block_on(courier.load(&id())).unwrap().unwrap();
        assert_eq!(stored.audio, b"abc", "the audio is untouched");
        let done = block_on(courier.deliver(&mut stored.clone(), |_| {}, no_wait, never)).unwrap();
        assert_eq!(done.meta.failure, None);
        assert_eq!(
            block_on(courier.waiting(&agent(), None)).unwrap()[0].failure,
            None
        );
    }

    #[test]
    fn old_clips_of_any_agent_are_swept_in_the_same_read() {
        let store = MemoryClips::default();
        let bridge = FakeBridge::default();
        let courier = courier(&store, &bridge);
        let mut old = clip(b"old".to_vec());
        old.meta.agent = "w9:p9".parse().unwrap();
        old.meta.recorded_at = Timestamp::from_unix_millis(10);
        let mut new = clip(b"new".to_vec());
        new.meta.audio_id = AudioId::new(SECOND).unwrap();
        new.meta.recorded_at = Timestamp::from_unix_millis(100);
        block_on(store.put(&old)).unwrap();
        block_on(store.put(&new)).unwrap();
        let waiting =
            block_on(courier.waiting(&agent(), Some(Timestamp::from_unix_millis(50)))).unwrap();
        assert_eq!(waiting, core::slice::from_ref(&new.meta));
        assert_eq!(store.lists.get(), 1, "one read of the store");
        assert_eq!(block_on(store.list()).unwrap(), [new.meta]);
    }

    #[test]
    fn renames_leave_the_reupload_budget_for_a_bridge_that_lost_the_clip() {
        thread_local! {
            static NEXT: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
        }
        fn next_id() -> Option<AudioId> {
            let ids = [SECOND, THIRD];
            let i = NEXT.with(|n| n.replace(n.get() + 1));
            AudioId::new(ids[i.min(1)]).ok()
        }
        let store = MemoryClips::default();
        let bridge = FakeBridge::default();
        let courier = Courier::new(store.clone(), &bridge, 4, next_id);
        let kept = block_on(courier.keep(
            agent(),
            id(),
            record(&[b"abcdef"]),
            Timestamp::from_unix_millis(5),
        ))
        .unwrap();
        // Both the clip's id and the first fresh one are taken.
        for taken in [ID, SECOND] {
            let mut other = clip(b"xy".to_vec());
            other.meta.audio_id = AudioId::new(taken).unwrap();
            block_on(upload::upload(&&bridge, &other, 4, |_| {}, never)).unwrap();
        }
        // Then the bridge loses the clip while it transcribes.
        bridge.busy_polls.set(1);
        let lost = core::cell::Cell::new(false);
        let done = block_on(courier.deliver(
            &mut kept.clone(),
            |_| {},
            |_| {
                if !lost.replace(true) {
                    bridge.forget(THIRD);
                }
                core::future::ready(())
            },
            never,
        ))
        .unwrap();
        assert_eq!(done.meta.audio_id.as_str(), THIRD);
        assert_eq!(done.stage(), Stage::Transcribed("6 bytes heard"));
    }

    #[test]
    fn a_retry_forgets_the_stored_failure() {
        let store = MemoryClips::default();
        let bridge = FakeBridge::default();
        let courier = courier(&store, &bridge);
        let kept = block_on(courier.keep(
            agent(),
            id(),
            record(&[b"abc"]),
            Timestamp::from_unix_millis(5),
        ))
        .unwrap();
        block_on(courier.record_failure(&kept, "Offline.".into())).unwrap();
        let mut stored = block_on(courier.load(&id())).unwrap().unwrap();
        assert!(stored.meta.failure.is_some());
        block_on(courier.clear_failure(&mut stored)).unwrap();
        assert_eq!(stored.meta.failure, None);
        // A reload now finds a clip to deliver, not a failure to show.
        let waiting = block_on(courier.waiting(&agent(), None)).unwrap();
        assert_eq!(waiting[0].failure, None);
        assert_eq!(
            block_on(courier.load(&id())).unwrap().unwrap().audio,
            b"abc",
            "the audio is untouched"
        );
    }

    #[test]
    fn a_failure_is_not_recorded_on_a_discarded_clip() {
        let store = MemoryClips::default();
        let bridge = FakeBridge::default();
        let courier = courier(&store, &bridge);
        let kept = block_on(courier.keep(
            agent(),
            id(),
            record(&[b"abc"]),
            Timestamp::from_unix_millis(5),
        ))
        .unwrap();
        block_on(courier.forget(&id())).unwrap();
        block_on(courier.record_failure(&kept, "Offline.".into())).unwrap();
        assert_eq!(block_on(courier.waiting(&agent(), None)).unwrap(), []);
    }

    #[test]
    fn a_discarded_clip_is_never_written_back() {
        let store = MemoryClips::default();
        let bridge = FakeBridge::default();
        let courier = courier(&store, &bridge);
        let kept = block_on(courier.keep(
            agent(),
            id(),
            record(&[b"abc"]),
            Timestamp::from_unix_millis(5),
        ))
        .unwrap();
        // Discarded while the transcript was on its way: the user's
        // forget lands, and delivery must not re-create the clip.
        let discarded = core::cell::Cell::new(false);
        bridge.busy_polls.set(1);
        let error = block_on(courier.deliver(
            &mut kept.clone(),
            |_| {},
            |_| {
                block_on(courier.forget(&id())).unwrap();
                discarded.set(true);
                core::future::ready(())
            },
            || discarded.get(),
        ))
        .unwrap_err();
        assert_eq!(error, DeliverError::Cancelled);
        assert!(!error.is_retryable());
        assert_eq!(block_on(courier.waiting(&agent(), None)).unwrap(), []);

        // Discarded during the final write itself: undone after it.
        let writes = core::cell::Cell::new(0);
        let racing = MemoryClips::default();
        let courier = Courier::new(racing.clone(), &bridge, 4, second_id);
        block_on(racing.put(&kept)).unwrap();
        let error = block_on(courier.deliver(
            &mut kept.clone(),
            |_| {},
            no_wait,
            || {
                // Cancelled from the second check on: after the first put.
                writes.set(writes.get() + 1);
                writes.get() > 2
            },
        ))
        .unwrap_err();
        assert_eq!(error, DeliverError::Cancelled);
        assert_eq!(block_on(courier.waiting(&agent(), None)).unwrap(), []);
    }

    #[test]
    fn poll_delays_grow_then_settle() {
        let delays: Vec<u32> = (0..8).map(poll_delay_ms).collect();
        assert_eq!(delays, [500, 1000, 1500, 2000, 2500, 3000, 3000, 3000]);
    }
}
