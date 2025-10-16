use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::stream::{SelectAll, Stream, StreamExt};
use futures::task::noop_waker_ref;
use pulsebeam_runtime::sync::spmc;
use str0m::media::Rid;
use str0m::rtp::RtpPacket;
use tokio::sync::watch;

use crate::entity::TrackId;
use crate::track::{TrackMeta, TrackReceiver};

type TrackDownstream = Pin<Box<dyn Stream<Item = (Arc<TrackMeta>, Arc<spmc::Slot<RtpPacket>>)> + Send>>;

#[derive(Debug, Clone, PartialEq)]
struct DownstreamConfig {
    paused: bool,
    target_rid: Option<Rid>,
    generation: u64,
}

pub struct DownstreamManager {
    streams: SelectAll<TrackDownstream>,
    tracks: HashMap<Arc<TrackId>, watch::Sender<DownstreamConfig>>,
}

impl DownstreamManager {
    pub fn new() -> Self {
        Self {
            streams: SelectAll::new(),
            tracks: HashMap::new(),
        }
    }

    pub fn add_track(&mut self, track: TrackReceiver) {
        if self.tracks.contains_key(&track.meta.id) {
            tracing::warn!(?track.meta.id, "Attempted to add a track that already exists.");
            return;
        }

        let find_default_index = |simulcast: &[crate::track::SimulcastReceiver]| -> Option<usize> {
            simulcast
                .iter()
                .position(|s| s.rid.is_none() || s.rid.unwrap().starts_with('f'))
        };

        let initial_rid = find_default_index(&track.simulcast)
            .and_then(|i| track.simulcast.get(i))
            .and_then(|s| s.rid);

        let initial_config = DownstreamConfig {
            paused: true,
            target_rid: initial_rid,
            generation: 0,
        };

        tracing::debug!(?track.meta.id, ?initial_config, "Adding downstream track");
        let (tx, rx) = watch::channel(initial_config);
        self.tracks.insert(track.meta.id.clone(), tx);

        let stream = Self::create_track_stream(track, rx, find_default_index);
        self.streams.push(stream);
    }

    fn create_track_stream(
        track: TrackReceiver,
        mut rx: watch::Receiver<DownstreamConfig>,
        find_default_index: fn(&[crate::track::SimulcastReceiver]) -> Option<usize>,
    ) -> TrackDownstream {
        async_stream::stream! {
            let mut track = track;
            let meta = track.meta.clone();

            let Some(mut active_index) = find_default_index(&track.simulcast) else {
                tracing::error!(?meta.id, "Track has no default receiver. Terminating stream task.");
                return;
            };

            let mut local_generation = 0;
            let mut is_paused = true;

            // Mark initial value as seen
            rx.borrow_and_update();

            loop {
                // If paused, wait for unpause signal
                if is_paused {
                    tracing::trace!(?meta.id, "Track paused, waiting for resume");
                    if rx.changed().await.is_err() {
                        tracing::debug!(?meta.id, "Config channel closed while paused");
                        break;
                    }
                    
                    // Process config change after waking from pause
                    let config = rx.borrow_and_update();
                    
                    if local_generation != config.generation {
                        local_generation = config.generation;
                        
                        // Check if we need to switch quality
                        let current_rid = track.simulcast[active_index].rid;
                        if current_rid != config.target_rid {
                            let new_index = match config.target_rid {
                                Some(target_rid) => track.simulcast.iter().position(|s| s.rid == Some(target_rid)),
                                None => find_default_index(&track.simulcast),
                            };

                            if let Some(new_index) = new_index {
                                if active_index != new_index {
                                    tracing::info!(
                                        ?meta.id,
                                        from = ?current_rid,
                                        to = ?config.target_rid,
                                        "Switching track quality"
                                    );
                                    active_index = new_index;
                                }
                            } else {
                                tracing::warn!(
                                    ?meta.id,
                                    requested_rid = ?config.target_rid,
                                    "Requested RID not found, keeping current quality"
                                );
                            }
                        }

                        // Flush and request keyframe if resuming or changing quality
                        if !config.paused {
                            let receiver = &mut track.simulcast[active_index];
                            tracing::debug!(?meta.id, rid = ?receiver.rid, "Flushing channel and requesting keyframe due to state change.");
                            receiver.request_keyframe();
                        }
                    }
                    
                    is_paused = config.paused;
                    continue;
                }

                // HOT PATH: Active state - wait for either a packet or a config change
                let receiver = &mut track.simulcast[active_index];
                
                tokio::select! {
                    biased;
                    
                    // Check config changes first
                    change_result = rx.changed() => {
                        if change_result.is_err() {
                            tracing::debug!(?meta.id, "Config channel closed");
                            break;
                        }
                        
                        // Process the config change
                        let config = rx.borrow_and_update();
                        
                        if local_generation != config.generation {
                            local_generation = config.generation;
                            
                            // Check if we need to switch quality
                            let current_rid = track.simulcast[active_index].rid;
                            if current_rid != config.target_rid {
                                let new_index = match config.target_rid {
                                    Some(target_rid) => track.simulcast.iter().position(|s| s.rid == Some(target_rid)),
                                    None => find_default_index(&track.simulcast),
                                };

                                if let Some(new_index) = new_index {
                                    if active_index != new_index {
                                        tracing::info!(
                                            ?meta.id,
                                            from = ?current_rid,
                                            to = ?config.target_rid,
                                            "Switching track quality"
                                        );
                                        active_index = new_index;
                                    }
                                } else {
                                    tracing::warn!(
                                        ?meta.id,
                                        requested_rid = ?config.target_rid,
                                        "Requested RID not found, keeping current quality"
                                    );
                                }
                            }

                            // Flush and request keyframe if changing quality
                            if !config.paused {
                                let receiver = &mut track.simulcast[active_index];
                                tracing::debug!(?meta.id, rid = ?receiver.rid, "Requesting keyframe due to config change.");
                                receiver.request_keyframe();
                            }
                        }
                        
                        is_paused = config.paused;
                    }
                    
                    // Then wait for packet
                    result = receiver.channel.recv() => {
                        match result {
                            Ok(pkt) => {
                                // Fast path: just yield the packet
                                yield (meta.clone(), pkt);
                            }
                            Err(spmc::RecvError::Lagged(n)) => {
                                tracing::warn!(
                                    ?meta.id,
                                    rid = ?receiver.rid,
                                    lagged = n,
                                    "Downstream track lagged, requesting keyframe"
                                );
                                receiver.request_keyframe();
                            }
                            Err(spmc::RecvError::Closed) => {
                                tracing::info!(?meta.id, "Channel closed, terminating stream");
                                break;
                            }
                        }
                    }
                }
            }
            
            tracing::debug!(?meta.id, "Downstream track stream ended");
        }
        .boxed()
    }

    pub fn pause_track(&self, track_id: &Arc<TrackId>) {
        if let Some(tx) = self.tracks.get(track_id) {
            tx.send_if_modified(|config| {
                if !config.paused {
                    config.paused = true;
                    true
                } else {
                    false
                }
            });
        }
    }

    pub fn resume_track(&self, track_id: &Arc<TrackId>) {
        if let Some(tx) = self.tracks.get(track_id) {
            tx.send_if_modified(|config| {
                if config.paused {
                    config.paused = false;
                    config.generation = config.generation.wrapping_add(1);
                    true
                } else {
                    false
                }
            });
        }
    }

    pub fn request_keyframe(&self, track_id: &Arc<TrackId>) {
        if let Some(tx) = self.tracks.get(track_id) {
            tx.send_modify(|config| {
                config.paused = false;
                config.generation = config.generation.wrapping_add(1);
            });
        }
    }

    pub fn set_track_quality(&self, track_id: &Arc<TrackId>, rid: Option<Rid>) {
        if let Some(tx) = self.tracks.get(track_id) {
            tx.send_modify(|config| {
                config.paused = false;
                config.target_rid = rid;
                config.generation = config.generation.wrapping_add(1);
            });
        }
    }

    pub fn remove_track(&mut self, track_id: &Arc<TrackId>) {
        if self.tracks.remove(track_id).is_some() {
            tracing::debug!(?track_id, "Removed downstream track");
        }
    }

    pub fn poll_next_packet(&mut self) -> Poll<Option<(Arc<TrackMeta>, Arc<spmc::Slot<RtpPacket>>)>> {
        let waker = noop_waker_ref();
        let mut cx = Context::from_waker(waker);
        self.poll_next_unpin(&mut cx)
    }
}

impl Stream for DownstreamManager {
    type Item = (Arc<TrackMeta>, Arc<spmc::Slot<RtpPacket>>);

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.streams.poll_next_unpin(cx)
    }
}
