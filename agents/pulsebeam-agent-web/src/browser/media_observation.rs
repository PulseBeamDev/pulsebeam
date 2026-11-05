use std::{
    cell::Cell,
    collections::BTreeMap,
    rc::{Rc, Weak},
};

use agent_core::{MediaObservationControl, MediaObservationId, ResourceCommand, ResourceResult};
use futures_channel::oneshot;
use js_sys::Promise;
use wasm_bindgen::{JsValue, prelude::wasm_bindgen};
use wasm_bindgen_futures::{future_to_promise, spawn_local};
use web_sys::{MediaStreamTrack, RtcPeerConnectionState};

use super::{LocalOperationError, RuntimeInner, input_is_live, same_js_object};

type Reply = oneshot::Sender<Result<BrowserMediaObservation, JsValue>>;

#[derive(Default)]
pub(super) struct Observations {
    beginning: usize,
    early: BTreeMap<MediaObservationId, MediaObservationControl>,
    entries: BTreeMap<MediaObservationId, Entry>,
}

struct Entry {
    control: Option<MediaObservationControl>,
    reply: Option<Reply>,
    lease: Weak<Lease>,
    attempting: bool,
}

struct Lease {
    inner: Weak<RuntimeInner>,
    control: MediaObservationControl,
    source: MediaStreamTrack,
    track: MediaStreamTrack,
    closed: Cell<bool>,
}

impl Lease {
    fn stop(&self) -> bool {
        if self.closed.replace(true) {
            return false;
        }
        self.track.stop();
        true
    }

    fn retire(&self) {
        if !self.stop() {
            return;
        }
        let inner = self.inner.clone();
        let control = self.control.clone();
        spawn_local(async move {
            if let Some(inner) = inner.upgrade() {
                inner.release_observation(control).await;
            }
        });
    }
}

#[wasm_bindgen]
pub struct BrowserMediaObservation {
    lease: Rc<Lease>,
}

#[wasm_bindgen]
impl BrowserMediaObservation {
    pub fn track(&self) -> Option<MediaStreamTrack> {
        (!self.lease.closed.get()).then(|| Clone::clone(&self.lease.track))
    }

    pub fn is_closed(&self) -> bool {
        self.lease.closed.get()
    }

    #[wasm_bindgen(unchecked_return_type = "Promise<void>")]
    pub fn close(&self) -> Promise {
        let first = self.lease.stop();
        let inner = self.lease.inner.clone();
        let control = self.lease.control.clone();
        future_to_promise(async move {
            if first && let Some(inner) = inner.upgrade() {
                inner.release_observation(control).await;
            }
            Ok(JsValue::UNDEFINED)
        })
    }
}

impl Drop for BrowserMediaObservation {
    fn drop(&mut self) {
        self.lease.retire();
    }
}

fn closed_error() -> JsValue {
    super::resources::coded_error("closed", "media observation is closed")
}

impl RuntimeInner {
    pub(super) fn begin_observation(
        self: &Rc<Self>,
        owner: u64,
        source: Option<String>,
    ) -> Promise {
        self.observations.borrow_mut().beginning += 1;
        let weak = Rc::downgrade(self);
        future_to_promise(async move {
            let inner = weak.upgrade().ok_or_else(closed_error)?;
            let command = match source {
                Some(source) => ResourceCommand::BeginAudioSourceObservation { owner, source },
                None => ResourceCommand::BeginMediaObservation { owner },
            };
            let result = inner.input_resource(command).await;
            let (sender, receiver) = oneshot::channel();
            let mut observations = inner.observations.borrow_mut();
            observations.beginning -= 1;
            let result = match result {
                Ok(ResourceResult::MediaObservation(id)) => {
                    if inner.is_closed() {
                        let weak = Rc::downgrade(&inner);
                        spawn_local(async move {
                            if let Some(inner) = weak.upgrade() {
                                let _ = inner
                                    .input_resource(ResourceCommand::Release {
                                        owner: id.request(),
                                    })
                                    .await;
                            }
                        });
                        Err(closed_error())
                    } else {
                        let control = observations.early.remove(&id);
                        observations.entries.insert(
                            id,
                            Entry {
                                control: None,
                                reply: Some(sender),
                                lease: Weak::new(),
                                attempting: false,
                            },
                        );
                        Ok(control)
                    }
                }
                Ok(_) => Err(LocalOperationError::runtime(
                    "Core did not begin a media observation",
                )
                .into_js()),
                Err(_) if inner.is_closed() => Err(closed_error()),
                Err(error) => Err(error.into_js()),
            };
            if observations.beginning == 0 {
                observations.early.clear();
            }
            drop(observations);
            let control = result?;
            if let Some(control) = control {
                inner.observation_control(control);
            }
            let result = receiver.await.map_err(|_| closed_error())??;
            Ok(JsValue::from(result))
        })
    }

    pub(super) fn observation_control(self: &Rc<Self>, control: MediaObservationControl) {
        let id = control.observation;
        let mut observations = self.observations.borrow_mut();
        let Some(entry) = observations.entries.get_mut(&id) else {
            if observations.beginning != 0 && !self.is_closed() {
                observations.early.insert(id, control);
            }
            return;
        };
        if control.closed {
            let Some(mut entry) = observations.entries.remove(&id) else {
                return;
            };
            drop(observations);
            if let Some(lease) = entry.lease.upgrade() {
                lease.stop();
            }
            if let Some(reply) = entry.reply.take() {
                let _ = reply.send(Err(closed_error()));
            }
            let weak = Rc::downgrade(self);
            spawn_local(async move {
                if let Some(inner) = weak.upgrade() {
                    let _ = inner
                        .input_resource(ResourceCommand::Release {
                            owner: id.request(),
                        })
                        .await;
                }
            });
            return;
        }
        entry.control = Some(control);
        drop(observations);
        self.progress_observations();
    }

    fn observation_track(&self, control: &MediaObservationControl) -> Option<MediaStreamTrack> {
        let target = control.target.as_ref()?;
        let (connection, mid, transceivers, track) = {
            let peers = self.peers.borrow();
            let peer = peers.get(&target.generation.get())?;
            let mid = peer.receiver_mids.get(&target.binding.media_index)?.clone();
            let track = Clone::clone(&peer.remote_tracks.get(&mid)?.track);
            let transceivers: Vec<_> = peer
                .transceivers
                .iter()
                .filter(|(slot, _)| slot == &target.binding.slot)
                .map(|(_, transceiver)| transceiver.clone())
                .collect();
            (peer.connection.clone(), mid, transceivers, track)
        };
        if matches!(
            connection.connection_state(),
            RtcPeerConnectionState::Closed | RtcPeerConnectionState::Failed
        ) || mid != target.binding.mid
            || !transceivers
                .iter()
                .any(|transceiver| transceiver.mid().as_deref() == Some(mid.as_str()))
            || !input_is_live(&track)
        {
            return None;
        }
        let (current_connection, current_track) = {
            let peers = self.peers.borrow();
            let peer = peers.get(&target.generation.get())?;
            if peer.receiver_mids.get(&target.binding.media_index) != Some(&mid) {
                return None;
            }
            (
                peer.connection.clone(),
                Clone::clone(&peer.remote_tracks.get(&mid)?.track),
            )
        };
        (same_js_object(&connection, &current_connection) && same_js_object(&track, &current_track))
            .then_some(track)
    }

    fn observation_matches(
        &self,
        control: &MediaObservationControl,
        track: &MediaStreamTrack,
    ) -> bool {
        self.observation_track(control)
            .is_some_and(|current| same_js_object(&current, track))
    }

    pub(super) fn progress_observations(self: &Rc<Self>) {
        if self.is_closed() {
            return;
        }
        let candidates: Vec<_> = self
            .observations
            .borrow()
            .entries
            .iter()
            .filter_map(|(id, entry)| {
                if entry.reply.is_none() || entry.attempting {
                    return None;
                }
                let control = entry.control.clone()?;
                Some((*id, control))
            })
            .collect();
        for (id, control) in candidates {
            let Some(track) = self.observation_track(&control) else {
                continue;
            };
            let mut observations = self.observations.borrow_mut();
            let Some(entry) = observations.entries.get_mut(&id) else {
                continue;
            };
            if entry.attempting || entry.control.as_ref() != Some(&control) {
                continue;
            }
            entry.attempting = true;
            drop(observations);
            let weak = Rc::downgrade(self);
            spawn_local(async move {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                inner.attempt_observation(control, track).await;
            });
        }
    }

    fn attempt_current(&self, control: &MediaObservationControl, track: &MediaStreamTrack) -> bool {
        let current = self
            .observations
            .borrow()
            .entries
            .get(&control.observation)
            .is_some_and(|entry| entry.reply.is_some() && entry.control.as_ref() == Some(control));
        !self.is_closed()
            && current
            && self.observation_matches(control, track)
            && !self.is_closed()
            && self
                .observations
                .borrow()
                .entries
                .get(&control.observation)
                .is_some_and(|entry| {
                    entry.reply.is_some() && entry.control.as_ref() == Some(control)
                })
    }

    fn finish_attempt(
        self: &Rc<Self>,
        control: &MediaObservationControl,
        source: &MediaStreamTrack,
    ) {
        let mut observations = self.observations.borrow_mut();
        let changed = if let Some(entry) = observations.entries.get_mut(&control.observation) {
            entry.attempting = false;
            entry.control.as_ref() != Some(control)
        } else {
            false
        };
        drop(observations);
        if changed
            || self
                .observation_track(control)
                .is_some_and(|current| !same_js_object(&current, source))
        {
            self.progress_observations();
        }
    }

    async fn attempt_observation(
        self: Rc<Self>,
        control: MediaObservationControl,
        source: MediaStreamTrack,
    ) {
        let authorized = self
            .input_resource(ResourceCommand::AuthorizeMediaObservation {
                control: control.clone(),
            })
            .await;
        match authorized {
            Err(error) => {
                self.fail_observation(control, error.into_js()).await;
                return;
            }
            Ok(ResourceResult::Updated) => {}
            Ok(_) => {
                self.finish_attempt(&control, &source);
                return;
            }
        }
        if !self.attempt_current(&control, &source) {
            self.finish_attempt(&control, &source);
            return;
        }
        // This is the WebIDL clone operation, not a clone of the Rust JS handle.
        let track = source.clone();
        let succeeded = input_is_live(&track);
        let completed = self
            .input_resource(ResourceCommand::CompleteMediaObservation {
                control: control.clone(),
                succeeded,
            })
            .await;
        match completed {
            Err(error) => {
                track.stop();
                self.fail_observation(control, error.into_js()).await;
                return;
            }
            Ok(ResourceResult::Updated) => {}
            Ok(_) => {
                track.stop();
                self.finish_attempt(&control, &source);
                return;
            }
        }
        if !succeeded || !self.attempt_current(&control, &source) {
            track.stop();
            self.fail_observation(control, closed_error()).await;
            return;
        }
        let lease = Rc::new(Lease {
            inner: Rc::downgrade(&self),
            control: control.clone(),
            source,
            track,
            closed: Cell::new(false),
        });
        let mut observations = self.observations.borrow_mut();
        let Some(entry) = observations.entries.get_mut(&control.observation) else {
            drop(observations);
            lease.retire();
            return;
        };
        entry.attempting = false;
        entry.lease = Rc::downgrade(&lease);
        let reply = entry.reply.take();
        drop(observations);
        if let Some(reply) = reply {
            let _ = reply.send(Ok(BrowserMediaObservation { lease }));
        } else {
            lease.retire();
        }
    }

    async fn fail_observation(&self, control: MediaObservationControl, error: JsValue) {
        let mut entry = self
            .observations
            .borrow_mut()
            .entries
            .remove(&control.observation);
        if let Some(entry) = entry.as_mut() {
            if let Some(reply) = entry.reply.take() {
                let _ = reply.send(Err(error));
            }
        }
        self.release_observation(control).await;
    }

    async fn release_observation(&self, control: MediaObservationControl) {
        self.observations
            .borrow_mut()
            .entries
            .remove(&control.observation);
        let _ = self
            .input_resource(ResourceCommand::EndMediaObservation {
                control: control.clone(),
            })
            .await;
        let _ = self
            .input_resource(ResourceCommand::Release {
                owner: control.observation.request(),
            })
            .await;
    }

    pub(super) fn retire_observation_tracks(self: &Rc<Self>) {
        let leases: Vec<_> = self
            .observations
            .borrow()
            .entries
            .values()
            .filter_map(|entry| entry.lease.upgrade())
            .collect();
        for lease in leases {
            if !self.observation_matches(&lease.control, &lease.source) {
                lease.retire();
            }
        }
        self.progress_observations();
    }

    pub(super) fn close_observations(&self) {
        let mut observations = self.observations.borrow_mut();
        observations.early.clear();
        let entries = std::mem::take(&mut observations.entries);
        drop(observations);
        let actor = self.actor.borrow().clone();
        for (id, mut entry) in entries {
            if let Some(lease) = entry.lease.upgrade() {
                lease.stop();
            }
            if let Some(reply) = entry.reply.take() {
                let _ = reply.send(Err(closed_error()));
            }
            if let Some(actor) = actor.clone() {
                spawn_local(async move {
                    if let Some(control) = entry.control {
                        if let Ok(completion) = actor
                            .send_resource(ResourceCommand::EndMediaObservation { control })
                            .await
                        {
                            let _ = completion.await;
                        }
                    }
                    if let Ok(completion) = actor
                        .send_resource(ResourceCommand::Release {
                            owner: id.request(),
                        })
                        .await
                    {
                        let _ = completion.await;
                    }
                });
            }
        }
    }
}
