//! Room-level Top-N audio selector.
//!
//! A single async task per room that:
//!
//! 1. Subscribes to every audio [`TrackReceiver`] published in the room.
//! 2. Reads speech-intensity scores from the track's shared [`StreamState`]
//!    — no duplicate audio monitoring needed; the upstream sender's
//!    `StreamMonitor` already maintains these atomics.
//! 3. Keeps the top-N speakers mapped to N output slots
//!    (N = [`SELECTOR_SLOTS`] = [`crate::controller::MAX_SEND_AUDIO_SLOTS`]).
//! 4. Forwards each incoming audio packet to the output slot currently
//!    assigned to that track — zero copies after the packet enters the ring.
//!
//! Each participant receives an [`AudioSelectorSubscription`] containing one
//! `spmc::Receiver<RtpPacket>` per slot.  The participant's `AudioAllocator`
//! polls these receivers and forwards packets to its negotiated audio MIDs,
//! rebasing the RTP timeline whenever the SSRC changes (i.e. whenever the
//! selector moves a different speaker into that slot).
//!
//! ## Input polling
//!
//! Input tracks are polled through a
//! [`futures_concurrency::stream::StreamGroup`], which maintains per-stream
//! wakers.  Only streams that have been notified by their underlying `spmc`
//! ring are visited on each drain cycle — O(woken) instead of O(N).  There is
//! no hard capacity limit (unlike `SlotGroup`'s 64-stream ceiling).
//!
//! ## Re-rank timer
//!
//! The Top-N re-rank fires on a dedicated `tokio::time::Sleep` future stored
//! inside the selector.  The hot packet path never calls `Instant::now()`.
//!
//! ## Pinning
//!
//! Participants that want to always show a specific speaker in a given slot
//! can swap out one of the subscription receivers for a direct
//! `spmc::Receiver` obtained from that participant's `TrackReceiver`.  The
//! `AudioAllocator::pin_slot` API supports this without any changes to the
//! selector.

use std::{
    cmp::Ordering,
    collections::HashMap,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use futures_concurrency::stream::StreamGroup;
use futures_concurrency::stream::stream_group::Key;
use futures_lite::stream::Stream as _;
use pulsebeam_runtime::sync::{Arc, spmc};
use tokio::{sync::mpsc, time::Instant};

use crate::{
    controller::MAX_SEND_AUDIO_SLOTS,
    entity::TrackId,
    rtp::{AudioRtpPacket, RtpPacket, monitor::StreamState},
    track::{TrackMeta, TrackReceiver},
};

/// Number of output slots produced by the selector; matches the controller's
/// `MAX_SEND_AUDIO_SLOTS` so every negotiated audio MID has a source.
pub const SELECTOR_SLOTS: usize = MAX_SEND_AUDIO_SLOTS;

/// Minimum interval between Top-N re-rank passes.
const RERANK_INTERVAL: Duration = Duration::from_millis(200);

// ── Public API ──────────────────────────────────────────────────────────────

/// Commands sent from the room to the [`TopNAudioSelector`] task.
pub enum AudioSelectorCmd {
    /// A new audio track has been published in the room.
    AddTrack(TrackReceiver),
    /// An audio track has been unpublished (participant left or track ended).
    RemoveTrack(TrackId),
}

/// A set of receivers — one per selector slot — handed to a single participant.
///
/// Obtain one via [`AudioSelectorHandle::subscribe`].
#[derive(Clone, Debug)]
pub struct AudioSelectorSubscription {
    /// `SELECTOR_SLOTS` receivers, indexed 1-to-1 with the selector's output
    /// slots.  Slot 0 is the loudest speaker, slot 1 the second-loudest, etc.
    /// (assignments shift after each re-rank, but the slot ordering is stable
    /// within a re-rank window).
    pub receivers: Vec<spmc::UnsyncReceiver<AudioRtpPacket>>,
}

/// Room-side handle: send track commands and create per-participant subscriptions.
pub struct AudioSelectorHandle {
    /// Send [`AudioSelectorCmd`]s to the background task.
    pub cmd_tx: mpsc::Sender<AudioSelectorCmd>,
    /// One prototype receiver per slot; cloned for every subscriber.
    receivers: Vec<spmc::UnsyncReceiver<AudioRtpPacket>>,
}

impl AudioSelectorHandle {
    /// Create a subscription for a newly-joined participant.
    ///
    /// Clones the receiver end of each output slot's ring buffer so the
    /// participant starts at the current head position (ready to receive
    /// future audio for all slots without any replay).
    pub fn subscribe(&self) -> AudioSelectorSubscription {
        AudioSelectorSubscription {
            receivers: self.receivers.clone(),
        }
    }
}

/// Create the selector and return the room-side handle plus the background task.
///
/// The caller should spawn the returned future as a dedicated tokio task.
/// The task shuts down automatically when the [`AudioSelectorHandle`] is dropped
/// (i.e. when the room actor exits), because the command sender is closed.
///
/// # Arguments
/// * `cmd_buf` — capacity of the command channel (e.g. `64`).
pub fn create(
    cmd_buf: usize,
) -> (
    AudioSelectorHandle,
    impl std::future::Future<Output = ()> + 'static,
) {
    let (cmd_tx, cmd_rx) = mpsc::channel(cmd_buf);

    let mut senders = Vec::with_capacity(SELECTOR_SLOTS);
    let mut receivers = Vec::with_capacity(SELECTOR_SLOTS);
    for _ in 0..SELECTOR_SLOTS {
        // 256-packet ring per slot.  At 50 pkt/s that is 5 s of runway;
        // enough to absorb any downstream stall without dropping audio.
        let (tx, rx) = spmc::unsync_channel::<AudioRtpPacket>(256);
        senders.push(tx);
        receivers.push(rx);
    }

    let handle = AudioSelectorHandle { cmd_tx, receivers };
    let task = TopNAudioSelector {
        cmd_rx,
        slots: senders
            .into_iter()
            .map(|sender| OutputSlot {
                sender,
                track_id: None,
            })
            .collect(),
        tracks: HashMap::new(),
        inputs: StreamGroup::new(),
        rerank_sleep: Box::pin(tokio::time::sleep(RERANK_INTERVAL)),
    };

    (handle, task.run())
}

// ── Internal implementation ──────────────────────────────────────────────────

/// Scoring metadata and `StreamGroup` key for one input audio track.
struct InputTrackMeta {
    /// Key returned by [`StreamGroup::insert`]; used to remove the stream on
    /// `RemoveTrack` without scanning the group.
    key: Key,
    /// Shared speech-intensity state maintained by the upstream `StreamMonitor`.
    /// Atomic loads — zero overhead on the hot path.
    state: StreamState,
    meta: Arc<TrackMeta>,
}

/// One `spmc` receiver wrapped as a `Stream<Item=(TrackId, RtpPacket)>` so it
/// can live inside the [`StreamGroup`].
///
/// * On `Lagged`: the receiver has already moved its position to the ring head;
///   retrying in the same `poll_next` call recovers without losing a waker.
/// * On `Closed`: the upstream sender has been dropped.  We stay `Pending`
///   (without registering a waker that will never fire) until an explicit
///   `RemoveTrack` command calls `StreamGroup::remove`.
struct InputStream {
    track_id: TrackId,
    receiver: spmc::UnsyncReceiver<RtpPacket>,
}

impl futures_lite::stream::Stream for InputStream {
    type Item = (TrackId, RtpPacket);

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // InputStream is Unpin (all fields are Unpin), so get_mut is safe.
        let this = self.get_mut();
        loop {
            match this.receiver.poll_recv(cx) {
                Poll::Ready(Ok(pkt)) => {
                    return Poll::Ready(Some((this.track_id, pkt)));
                }
                Poll::Ready(Err(spmc::RecvError::Lagged(n))) => {
                    tracing::warn!(
                        track_id = %this.track_id,
                        skipped = n,
                        "audio selector input lagging; retrying"
                    );
                    // The receiver's position is now at the ring head.  Loop
                    // to attempt another read immediately rather than parking
                    // and requiring another wakeup cycle.
                    continue;
                }
                Poll::Ready(Err(spmc::RecvError::Closed)) => {
                    // Sender dropped.  No waker will ever fire for this slot.
                    // Stay Pending until explicit RemoveTrack + StreamGroup::remove.
                    return Poll::Pending;
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// One of the N output slots produced by the selector.
struct OutputSlot {
    /// Broadcast channel; all participant subscriptions share the same ring.
    sender: spmc::UnsyncSender<AudioRtpPacket>,
    /// Which input track is currently assigned to this slot.
    track_id: Option<TrackId>,
}

struct TopNAudioSelector {
    cmd_rx: mpsc::Receiver<AudioSelectorCmd>,
    /// Exactly `SELECTOR_SLOTS` output slots.
    slots: Vec<OutputSlot>,
    /// Scoring metadata (StreamGroup key + StreamState) keyed by TrackId.
    tracks: HashMap<TrackId, InputTrackMeta>,
    /// Fan-in of all input audio streams.
    ///
    /// `StreamGroup` maintains a per-stream waker bitmask so `poll_next` only
    /// visits streams that have been notified — O(woken) rather than O(N).
    /// No capacity limit (unlike `SlotGroup`'s 64-stream ceiling), so rooms
    /// of any size are supported.
    ///
    /// `StreamGroup<InputStream>` is `Unpin` because `InputStream` is `Unpin`
    /// and `ChunkedVec` (the internal storage) has no pinned fields.
    inputs: StreamGroup<InputStream>,
    /// Dedicated timer for re-ranking.  Stored as `Pin<Box<Sleep>>` so it
    /// can be polled directly inside `poll_fn` without a separate `select!`
    /// branch.  The hot packet-forwarding path never calls `Instant::now()`.
    rerank_sleep: Pin<Box<tokio::time::Sleep>>,
}

impl TopNAudioSelector {
    /// Run the selector until the command channel is closed.
    ///
    /// Uses `poll_fn` to drive three concurrent concerns from a single task
    /// without constructing any per-poll heap allocations:
    ///
    /// 1. Command drain — O(commands)
    /// 2. Re-rank timer — O(1) atomic + optional O(N log N) sort
    /// 3. Packet drain — O(woken streams) via `StreamGroup`
    async fn run(mut self) {
        std::future::poll_fn(|cx| -> Poll<()> {
            // ── 1. Drain the command channel ──────────────────────────────
            loop {
                match self.cmd_rx.poll_recv(cx) {
                    Poll::Ready(Some(cmd)) => self.handle_cmd(cmd),
                    // Room dropped the handle → shut down.
                    Poll::Ready(None) => return Poll::Ready(()),
                    Poll::Pending => break,
                }
            }

            // ── 2. Re-rank timer ─────────────────────────────────────────
            // Polled once per outer `poll_fn` call — zero Instant::now() on
            // the packet path.
            if self.rerank_sleep.as_mut().poll(cx).is_ready() {
                self.rerank();
                self.rerank_sleep
                    .as_mut()
                    .reset(Instant::now() + RERANK_INTERVAL);
            }

            // ── 3. Drain all available input packets ─────────────────────
            //
            // `StreamGroup::poll_next` with per-stream wakers:
            //   • Only visits streams whose waker has fired since last drain.
            //   • Re-arms each visited stream's ready bit after returning an
            //     item, so the outer loop keeps draining the same stream
            //     until it returns Pending — no wasted round-trips.
            //   • When all ready bits are clear, returns Pending and the task
            //     parks until the next packet arrives.
            //
            // `StreamGroup<InputStream>: Unpin` so `Pin::new` is safe.
            loop {
                match Pin::new(&mut self.inputs).poll_next(cx) {
                    Poll::Ready(Some((track_id, packet))) => {
                        self.forward(track_id, packet);
                    }
                    // Pending: all ready bits cleared, wakers registered.
                    // None: would mean all streams ended simultaneously; treat
                    // the same as Pending — commands can add new streams later.
                    Poll::Ready(None) | Poll::Pending => break,
                }
            }

            Poll::Pending
        })
        .await
    }

    // ── Command handling ─────────────────────────────────────────────────────

    fn handle_cmd(&mut self, cmd: AudioSelectorCmd) {
        match cmd {
            AudioSelectorCmd::AddTrack(track) => {
                let id = track.meta.id;
                if self.tracks.contains_key(&id) {
                    return;
                }
                let sim = track.lowest_quality();
                let state = sim.state.clone();
                let meta = track.meta.clone();
                let receiver = sim.channel.clone();
                let key = self.inputs.insert(InputStream {
                    track_id: id,
                    receiver,
                });
                self.tracks.insert(id, InputTrackMeta { key, state, meta });
            }
            AudioSelectorCmd::RemoveTrack(id) => {
                self.remove_track(id);
            }
        }
    }

    fn remove_track(&mut self, id: TrackId) {
        if let Some(meta) = self.tracks.remove(&id) {
            self.inputs.remove(meta.key);
        }
        for slot in &mut self.slots {
            if slot.track_id == Some(id) {
                slot.track_id = None;
            }
        }
    }

    // ── Hot path ─────────────────────────────────────────────────────────────

    /// Route `packet` to the output slot currently assigned to `track_id`.
    ///
    /// Silently drops packets for tracks that have no slot assignment; this
    /// only occurs in the ≤200 ms window between a speaker becoming active and
    /// the next re-rank pass assigning them a slot.
    fn forward(&mut self, track_id: TrackId, packet: RtpPacket) {
        let Some(slot) = self.slots.iter_mut().find(|s| s.track_id == Some(track_id)) else {
            return;
        };

        let Some(track) = self.tracks.get(&track_id) else {
            return;
        };

        slot.sender.send(AudioRtpPacket {
            participant_id: track.meta.origin_participant,
            track_id,
            packet,
        });
    }

    // ── Re-ranking ────────────────────────────────────────────────────────────

    /// Re-score every input track and update slot assignments.
    ///
    /// The algorithm is *stable*: tracks that were already in a slot and remain
    /// in the new top-N keep their slot, avoiding unnecessary timeline resets
    /// at the subscriber side.
    fn rerank(&mut self) {
        if self.tracks.is_empty() {
            return;
        }

        // Score every track using the shared speech-intensity envelope
        // maintained by the upstream sender's StreamMonitor.
        let mut scored: Vec<(TrackId, f32)> = self
            .tracks
            .iter()
            .map(|(&id, meta)| {
                let envelope = meta.state.audio_envelope();
                let silence_penalty = {
                    // Slight penalty for recent silence so active speakers
                    // are preferred over those who just stopped talking.
                    let secs = meta.state.silence_duration().as_secs_f32();
                    // Decays from 0 → −0.2 over the first 2 s of silence.
                    -(secs / 2.0).min(1.0) * 0.2
                };
                (id, envelope + silence_penalty)
            })
            .collect();

        // Descending by score; stable sort preserves insertion order for ties,
        // giving deterministic results within a session.
        scored.sort_by(|(_, a), (_, b)| b.partial_cmp(a).unwrap_or(Ordering::Equal));

        let top_n: Vec<TrackId> = scored
            .iter()
            .take(SELECTOR_SLOTS)
            .map(|(id, _)| *id)
            .collect();

        // Pass 1 — retain existing valid assignments.
        // Tracks that stay in the top-N keep whichever slot they already hold.
        let mut unassigned: Vec<TrackId> = top_n.clone();
        for slot in &mut self.slots {
            if let Some(id) = slot.track_id {
                if let Some(pos) = unassigned.iter().position(|x| *x == id) {
                    // Still in top-N → keep assignment.
                    unassigned.remove(pos);
                } else {
                    // Fell out of top-N → vacate slot.
                    slot.track_id = None;
                }
            }
        }

        // Pass 2 — fill empty slots with any remaining top-N tracks.
        let mut iter = unassigned.into_iter();
        for slot in &mut self.slots {
            if slot.track_id.is_none() {
                slot.track_id = iter.next();
            }
        }

        tracing::trace!(
            tracks = self.tracks.len(),
            assigned = self.slots.iter().filter(|s| s.track_id.is_some()).count(),
            "audio selector re-ranked"
        );
    }
}
