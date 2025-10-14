use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::stream::{SelectAll, Stream, StreamExt};
use pulsebeam_runtime::sync::spmc;
use str0m::rtp::RtpPacket;
use tokio::sync::watch;

use crate::entity::TrackId;
use crate::track::{TrackMeta, TrackReceiver};

// A convenient type alias for the streams we'll be managing.
type TrackDownstream = Pin<Box<dyn Stream<Item = (Arc<TrackMeta>, Arc<RtpPacket>)> + Send>>;

// The configuration for a single downstream track.
// Clone is cheap as it only contains a boolean.
#[derive(Debug, Clone)]
struct DownstreamConfig {
    paused: bool,
}

impl Default for DownstreamConfig {
    fn default() -> Self {
        Self { paused: true }
    }
}

/// Manages a dynamic number of downstream tracks, multiplexing them into a single stream.
/// Provides controls to pause, resume, or remove individual tracks.
pub struct DownstreamManager {
    // The core multiplexer. It polls all managed streams and returns the next ready item.
    streams: SelectAll<TrackDownstream>,

    // A map to store the control handles (watch::Sender) for each track.
    tracks: HashMap<Arc<TrackId>, watch::Sender<DownstreamConfig>>,
}

impl DownstreamManager {
    /// Creates a new, empty DownstreamManager.
    pub fn new() -> Self {
        Self {
            streams: SelectAll::new(),
            tracks: HashMap::new(),
        }
    }

    /// Adds a new track to the manager.
    /// The track will be actively polled for packets until it ends or is removed.
    pub fn add_track(&mut self, mut track: TrackReceiver) {
        if self.tracks.contains_key(&track.meta.id) {
            tracing::warn!(?track.meta.id, "Attempted to add a track that already exists.");
            return;
        }

        tracing::debug!(?track.meta.id, "Adding downstream track");
        let (tx, mut rx) = watch::channel(DownstreamConfig::default());

        // We store the sender so we can control this stream later.
        self.tracks.insert(track.meta.id.clone(), tx);

        let stream = async_stream::stream! {
            let meta = track.meta.clone();
            // Assuming `by_default` gives us the primary simulcast receiver.
            let Some(receiver) = track.by_default() else {
                tracing::warn!(?meta.id, "Track has no default receiver to subscribe to.");
                return;
            };

            loop {
                // Get the current config state.
                let config = rx.borrow().clone();

                if config.paused {
                    // --- PAUSED STATE ---
                    // If paused, we must *only* wait for the config to change.
                    // If rx.changed() returns an error, the sender was dropped (e.g., by remove_track),
                    // so we should terminate this stream.
                    if rx.changed().await.is_err() {
                        break;
                    }
                    // Loop again to re-check the new config.
                    continue;
                }

                // --- ACTIVE STATE ---
                // If not paused, we wait for EITHER a packet to arrive OR the config to change.
                tokio::select! {
                    // Biased ensures we check for config changes first if both are ready.
                    biased;

                    // Branch 1: The configuration for this track has changed.
                    result = rx.changed() => {
                        if result.is_err() {
                            // Sender was dropped, so this track was removed from the manager.
                            // Break the loop to terminate the stream.
                            break;
                        }
                        // The config has changed. The loop will restart and re-borrow the new config.
                        continue;
                    }

                    // Branch 2: A packet has arrived from the underlying spmc channel.
                    result = receiver.channel.recv() => {
                        match result {
                            Ok(pkt) => {
                                // We got a packet, yield it from the stream.
                                yield (meta.clone(), pkt);
                            }
                            Err(spmc::RecvError::Closed) => {
                                // The upstream source of this track has closed. Terminate.
                                break;
                            }
                            Err(spmc::RecvError::Lagged(n)) => {
                                tracing::warn!(?receiver, "Downstream track lagged by {n} packets");
                            }
                        }
                    }
                }
            }
            tracing::debug!(?meta.id, "downstream track stream has ended.");
        }
        .boxed(); // Pin the stream to the heap.

        self.streams.push(stream);
    }

    /// Pauses a specific track, preventing it from forwarding packets.
    pub fn pause_track(&self, track_id: &Arc<TrackId>) {
        if let Some(tx) = self.tracks.get(track_id) {
            // send() will update the watch channel's value.
            tx.send_if_modified(|config| {
                if !config.paused {
                    config.paused = true;
                    true // The value was modified
                } else {
                    false // The value was not modified
                }
            });
        }
    }

    /// Resumes a paused track, allowing it to forward packets again.
    pub fn resume_track(&self, track_id: &Arc<TrackId>) {
        if let Some(tx) = self.tracks.get(track_id) {
            tx.send_if_modified(|config| {
                if config.paused {
                    config.paused = false;
                    true
                } else {
                    false
                }
            });
        }
    }

    /// Removes a track from the manager.
    /// This will stop polling the track and cause its underlying stream task to terminate.
    pub fn remove_track(&mut self, track_id: &Arc<TrackId>) {
        // By removing the sender from the map, we drop it.
        // This causes the `rx.changed().await` call in the stream to return an error,
        // which gracefully terminates the stream task.
        if self.tracks.remove(track_id).is_some() {
            tracing::debug!(?track_id, "Removed downstream track");
        }
    }
}

// Implement the Stream trait for our manager.
// This allows the user of the manager to treat it as a single, unified stream of packets.
impl Stream for DownstreamManager {
    type Item = (Arc<TrackMeta>, Arc<RtpPacket>);

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // We simply delegate the poll to the underlying `SelectAll` stream.
        self.streams.poll_next_unpin(cx)
    }
}
