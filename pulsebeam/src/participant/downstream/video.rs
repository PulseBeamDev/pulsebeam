use crate::rtp::monitor::StreamQuality;
use crate::rtp::switcher::Switcher;
use crate::rtp::{self, RtpPacket};
use futures::stream::SelectAll;
use futures::{Stream, StreamExt};
use pulsebeam_runtime::sync::spmc;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use str0m::bwe::Bitrate;
use str0m::media::{KeyframeRequest, KeyframeRequestKind, Mid};
use tokio::sync::watch;

use crate::entity::TrackId;
use crate::track::{SimulcastQuality, SimulcastReceiver, TrackReceiver};

type SlotStreamItem = (Mid, RtpPacket);
type SlotStream = Pin<Box<dyn Stream<Item = SlotStreamItem> + Send>>;

#[derive(Default)]
pub struct VideoAllocator {
    tracks: HashMap<Arc<TrackId>, TrackState>,
    slots: Vec<Slot>,

    ticks: u32,
    select_all: SelectAll<SlotStream>,
}

impl VideoAllocator {
    pub fn set_slot_max_height(&mut self, mid: Mid, max_height: u32) {
        if let Some(slot) = self.slots.iter_mut().find(|s| s.mid == mid) {
            slot.max_height = max_height;
        }
    }

    pub fn add_track(&mut self, track: TrackReceiver) {
        if self.tracks.contains_key(&track.meta.id) {
            return;
        }

        tracing::info!(
            track = %track.meta.id,
            "video track added"
        );
        self.tracks.insert(
            track.meta.id.clone(),
            TrackState {
                track,
                assigned_mid: None,
            },
        );
        self.rebalance();
    }

    pub fn remove_track(&mut self, track_id: &Arc<TrackId>) {
        if self.tracks.remove(track_id).is_some() {
            tracing::info!(
                track = %track_id,
                "video track removed"
            );
            self.rebalance();
        }
    }

    pub fn add_slot(&mut self, mid: Mid) {
        tracing::info!(%mid, "video slot added");

        let (tx, rx) = watch::channel(SlotConfig {
            paused: true,
            target: None,
        });

        let mut reader = SlotReader::new(mid, rx);
        let stream = async_stream::stream! {
            while let Some(item) = reader.next().await {
                yield item;
            }
        };
        self.select_all.push(stream.boxed());
        self.slots.push(Slot::new(mid, tx));
        self.rebalance();
    }

    fn rebalance(&mut self) {
        let mut unassigned_tracks = self.tracks.iter_mut().filter(|(_, state)| {
            assert!(state.track.meta.kind.is_video());
            state.assigned_mid.is_none()
        });

        for slot in &mut self.slots {
            if !slot.is_idle() {
                continue;
            }

            let Some(next_track) = unassigned_tracks.next() else {
                break;
            };

            next_track.1.assigned_mid.replace(slot.mid);
            slot.switch_to(next_track.1.track.lowest_quality().clone());
        }
    }

    // Update allocations based on the following events:
    //  1. Available bandwidth
    //  2. Video slots
    //  3. update_allocations get polled every 500ms
    pub fn update_allocations(&mut self, available_bandwidth: Bitrate) -> (Bitrate, Bitrate) {
        const DOWNGRADE_HYSTERESIS_FACTOR: f64 = 0.85;
        const UPGRADE_HYSTERESIS_FACTOR: f64 = 1.25;

        if self.slots.is_empty() {
            return (Bitrate::from(0), Bitrate::from(0));
        }

        let budget = available_bandwidth.as_f64().max(300_000.0);
        self.slots.sort_by_key(|s| s.max_height);

        let mut total_allocated = 0.0;
        let mut total_desired = 0.0;
        let mut upgraded = true;

        while upgraded {
            upgraded = false;
            total_allocated = 0.0;
            total_desired = 0.0;

            for slot in &mut self.slots {
                let Some(current_receiver) = slot.target_receiver() else {
                    // next slot assignment will handle this
                    continue;
                };

                let Some(state) = self.tracks.get_mut(&current_receiver.meta.id) else {
                    continue;
                };

                let track = &state.track;
                let paused = slot.is_paused();

                let desired = if current_receiver.state.is_inactive() {
                    // very likely the sender can't keep up with sending higher resolution.
                    track.lowest_quality()
                } else {
                    // Conservative upgrade logic: only upgrade if higher layer is excellent
                    match current_receiver.state.quality() {
                        StreamQuality::Bad => track.lowest_quality(),
                        StreamQuality::Good => current_receiver, // Stay at current when good
                        StreamQuality::Excellent => track
                            .higher_quality(current_receiver.quality)
                            .filter(|next| {
                                next.state.quality() == StreamQuality::Excellent
                                    && !next.state.is_inactive()
                            })
                            .unwrap_or(current_receiver),
                    }
                };

                let is_upgrade = desired.quality > current_receiver.quality;
                let is_downgrade = desired.quality < current_receiver.quality;

                // Apply hysteresis
                let desired_bitrate = if is_upgrade {
                    desired.state.bitrate_bps() * UPGRADE_HYSTERESIS_FACTOR
                } else if is_downgrade {
                    desired.state.bitrate_bps() * DOWNGRADE_HYSTERESIS_FACTOR
                } else {
                    desired.state.bitrate_bps()
                };

                total_desired += desired_bitrate;

                if total_allocated + desired_bitrate <= budget
                    && (paused || desired.rid != current_receiver.rid)
                {
                    upgraded = true;
                    total_allocated += desired.state.bitrate_bps();
                    slot.prepare(desired.clone());
                } else {
                    // Can't afford upgrade or change - stay at current
                    total_allocated += current_receiver.state.bitrate_bps();
                }
            }
        }

        for slot in &mut self.slots {
            slot.commit();
        }

        let total_allocated = Bitrate::from(total_allocated);
        let total_desired = Bitrate::from(total_desired);

        if self.ticks >= 30 {
            tracing::debug!(
                available = %available_bandwidth,
                budget = %Bitrate::from(budget),
                allocated = %total_allocated,
                desired = %total_desired,
                "allocation summary"
            );
            self.ticks = 0;
        }
        self.ticks += 1;

        (total_allocated, total_desired)
    }

    pub fn handle_keyframe_request(&self, req: KeyframeRequest) {
        let Some(slot) = self.slots.iter().find(|e| e.mid == req.mid) else {
            tracing::warn!(?req, "no video slot found for keyframe request");
            return;
        };
        slot.request_keyframe(req.kind);
    }

    pub async fn next(&mut self) -> Option<SlotStreamItem> {
        self.select_all.next().await
    }
}

#[derive(Debug)]
struct TrackState {
    track: TrackReceiver,
    assigned_mid: Option<Mid>,
}

#[derive(Debug, Clone)]
struct SlotConfig {
    paused: bool,
    target: Option<SimulcastReceiver>,
}

struct Slot {
    mid: Mid,
    max_height: u32,

    config_tx: watch::Sender<SlotConfig>,
}

impl Slot {
    fn new(mid: Mid, config_tx: watch::Sender<SlotConfig>) -> Self {
        Self {
            mid,
            max_height: 0,
            config_tx,
        }
    }

    fn request_keyframe(&self, kind: KeyframeRequestKind) {}
}

enum SlotState {
    Idle,
    Paused {
        active: SimulcastReceiver,
    },
    Resuming {
        staging: SimulcastReceiver,
    },
    Streaming {
        active: SimulcastReceiver,
    },
    Switching {
        active: SimulcastReceiver,
        staging: SimulcastReceiver,
    },
}

struct SlotReader {
    mid: Mid,
    switcher: Switcher,
    state: Option<SlotState>,

    config_rx: watch::Receiver<SlotConfig>,
}

impl SlotReader {
    fn new(mid: Mid, config_rx: watch::Receiver<SlotConfig>) -> Self {
        Self {
            mid,
            switcher: Switcher::new(rtp::VIDEO_FREQUENCY),
            state: Some(SlotState::Idle),
            config_rx,
        }
    }

    async fn next(&mut self) -> Option<(Mid, RtpPacket)> {
        loop {
            if let Some(pkt) = self.switcher.pop() {
                return Some((self.mid, pkt));
            }

            let state = self.state.take().expect("SlotReader state invalid");

            let next_state = match state {
                SlotState::Idle => {
                    self.switcher.drain();
                    if self.config_rx.changed().await.is_ok() {
                        self.reconcile(SlotState::Idle)
                    } else {
                        SlotState::Idle
                    }
                }

                SlotState::Paused { active } => {
                    self.switcher.drain();
                    if self.config_rx.changed().await.is_ok() {
                        self.reconcile(SlotState::Paused { active })
                    } else {
                        SlotState::Paused { active }
                    }
                }

                SlotState::Resuming { mut staging } => {
                    tokio::select! {
                        biased;
                        Ok(_) = self.config_rx.changed() => self.reconcile(SlotState::Resuming { staging }),
                        res = staging.channel.recv() => self.handle_staging(res, staging, None),
                    }
                }

                SlotState::Streaming { mut active } => {
                    tokio::select! {
                        biased;
                        Ok(_) = self.config_rx.changed() => self.reconcile(SlotState::Streaming { active }),
                        res = active.channel.recv() => self.handle_active(res, active, None),
                    }
                }

                SlotState::Switching {
                    mut active,
                    mut staging,
                } => {
                    tokio::select! {
                        biased;
                        Ok(_) = self.config_rx.changed() => self.reconcile(SlotState::Switching { active, staging }),
                        res = staging.channel.recv() => self.handle_staging(res, staging, Some(active)),
                        res = active.channel.recv() => self.handle_active(res, active, Some(staging)),
                    }
                }
            };

            self.state = Some(next_state);
        }
    }

    #[inline]
    fn handle_active(
        &mut self,
        res: Result<Arc<spmc::Slot<RtpPacket>>, spmc::RecvError>,
        active: SimulcastReceiver,
        staging: Option<SimulcastReceiver>,
    ) -> SlotState {
        match res {
            Ok(pkt) => {
                self.switcher.push(pkt.value.clone());
                match staging {
                    Some(s) => SlotState::Switching { active, staging: s },
                    None => SlotState::Streaming { active },
                }
            }
            Err(spmc::RecvError::Lagged(n)) => {
                tracing::warn!(mid = %self.mid, skipped = n, "Active lagged");
                SlotState::Paused { active }
            }
            Err(spmc::RecvError::Closed) => {
                tracing::info!(mid = %self.mid, "Active closed");
                match staging {
                    Some(s) => SlotState::Resuming { staging: s },
                    None => SlotState::Idle,
                }
            }
        }
    }

    #[inline]
    fn handle_staging(
        &mut self,
        res: Result<Arc<spmc::Slot<RtpPacket>>, spmc::RecvError>,
        staging: SimulcastReceiver,
        active: Option<SimulcastReceiver>,
    ) -> SlotState {
        match res {
            Ok(pkt) => {
                self.switcher.stage(pkt.value.clone());
                if self.switcher.is_ready() {
                    tracing::info!(mid = %self.mid, rid = ?staging.rid, "Switch complete");
                    SlotState::Streaming { active: staging }
                } else {
                    match active {
                        Some(a) => SlotState::Switching { active: a, staging },
                        None => SlotState::Resuming { staging },
                    }
                }
            }
            Err(spmc::RecvError::Lagged(n)) => {
                tracing::warn!(mid = %self.mid, skipped = n, "Staging lagged");
                SlotState::Paused { active: staging }
            }
            Err(spmc::RecvError::Closed) => {
                tracing::warn!(mid = %self.mid, "Staging closed");
                match active {
                    Some(a) => SlotState::Streaming { active: a },
                    None => SlotState::Idle,
                }
            }
        }
    }

    fn reconcile(&mut self, current_state: SlotState) -> SlotState {
        let config = self.config_rx.borrow_and_update();

        // If config requests pause, we unconditionally pause the current/target stream
        if config.paused {
            let active = match current_state {
                SlotState::Idle => config.target.clone(),
                SlotState::Paused { active } => active,
                SlotState::Streaming { active } => active,
                SlotState::Resuming { staging } => staging,
                SlotState::Switching { active, .. } => active, // Fallback to active on pause
            };

            return SlotState::Paused { active };
        }

        // Identify the current "target" of the state machine
        let current_target = match &current_state {
            SlotState::Idle => None,
            SlotState::Paused { active } => Some(active),
            SlotState::Streaming { active } => Some(active),
            SlotState::Resuming { staging } => Some(staging),
            SlotState::Switching { staging, .. } => Some(staging),
        };

        // Check if we are already playing (or trying to play) this exact stream
        if let Some(current) = current_target {
            if current.rid == config.target.rid && current.meta.id == config.target.meta.id {
                // If we are Paused, we must wake up (Unpause).
                // If we are Idle, Streaming, or Switching, we stay as is.
                if !matches!(current_state, SlotState::Paused { .. }) {
                    return current_state;
                }
            }
        }

        // Initiate Switch
        tracing::info!(
            mid = %self.mid,
            to_rid = ?config.target.rid,
            track = %config.target.meta.id,
            "Reconcile: initiating switch"
        );

        let mut new_rx = config.target.clone();
        new_rx.channel.reset();
        new_rx.request_keyframe(KeyframeRequestKind::Fir);

        match current_state {
            SlotState::Idle | SlotState::Paused { .. } => SlotState::Resuming { staging: new_rx },
            SlotState::Streaming { active } => SlotState::Switching {
                active,
                staging: new_rx,
            },
            // TODO: we should buffer transitioning in a transition
            SlotState::Resuming { .. } => SlotState::Resuming { staging: new_rx },
            SlotState::Switching { active, .. } => SlotState::Switching {
                active,
                staging: new_rx,
            },
        }
    }
}
