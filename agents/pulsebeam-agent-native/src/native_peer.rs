//! The single RTC/media implementation, independent of runtime services.
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::task::{Context, Poll};

use agent_core::{
    ChannelId, DataChannelBinding, DataChannelEffect, DataChannelReliability, DataChannelSpec,
    Generation, HostEvent, MediaSlot, MediaTopology, OfferResources, RtcEvent,
};
use pulsebeam_webrtc_sys as rtc;

use crate::native_media::{
    self, MediaConfig, NativeStatistics, ReceiveMode, SourceIdentity, VideoSourceConfig,
};
use crate::native_receiver::NativeReceiver;
use crate::native_source::{NativeSource, SourceKind};

const EVENTS_PER_TURN: usize = 64;
const PENDING_SENDS: usize = 256;
// Pinned SDK bound, including completed outcomes not yet consumed.
const NATIVE_CHANNEL_SENDS: usize = 64;
const DD: &str =
    "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension";
const VLA: &str = "http://www.webrtc.org/experiments/rtp-hdrext/video-layers-allocation00";

#[derive(Clone, Copy)]
enum Stage {
    Creating(rtc::OperationId),
    SettingLocal(rtc::OperationId),
    Gathering,
    Offered,
    SettingRemote(rtc::OperationId),
    Ready,
}

pub(crate) enum Observation {
    Track {
        slot: MediaSlot,
        track: crate::RemoteTrack,
    },
    TrackRemoved {
        slot: MediaSlot,
    },
    Statistics(NativeStatistics),
    ChannelMessagesDropped {
        channel: ChannelId,
        messages: u64,
        bytes: u64,
    },
}

#[derive(Default)]
pub(crate) struct Progress {
    pub events: Vec<HostEvent>,
    pub observations: Vec<Observation>,
    pub more: bool,
}

pub(crate) struct NativePeer<R: rtc::SessionRuntime> {
    pub generation: Generation,
    session: rtc::Session<R>,
    peer: rtc::SessionPeerId,
    stage: Stage,
    gathered: bool,
    connection: rtc::ConnectionState,
    channels: BTreeMap<ChannelId, rtc::SessionChannelId>,
    signaling_channel: ChannelId,
    channel_bindings: Vec<DataChannelBinding>,
    pending_sends: BTreeMap<(ChannelId, u64), agent_core::OperationId>,
    queued_sends: VecDeque<DataChannelEffect>,
    stats: Option<rtc::OperationId>,
    transceivers: BTreeMap<MediaSlot, rtc::SessionTransceiverId>,
    bindings: BTreeMap<MediaSlot, agent_core::SlotBinding>,
    pub sources: BTreeMap<MediaSlot, NativeSource>,
    pub receivers: BTreeMap<MediaSlot, NativeReceiver>,
}

fn error(message: impl Into<String>) -> rtc::PeerError {
    rtc::PeerError {
        kind: rtc::PeerErrorKind::Operation,
        message: message.into(),
    }
}

pub(crate) fn slots(topology: &MediaTopology) -> Vec<MediaSlot> {
    (0..topology.local_video)
        .map(|i| MediaSlot::LocalVideo(format!("v{i}")))
        .chain((0..topology.local_audio).map(|i| MediaSlot::LocalAudio(format!("a{i}"))))
        .chain((0..topology.remote_video).map(MediaSlot::RemoteVideo))
        .chain((0..topology.remote_audio).map(MediaSlot::RemoteAudio))
        .collect()
}

fn codecs_and_extensions<R: rtc::SessionRuntime>(
    session: &mut rtc::Session<R>,
    transceiver: rtc::SessionTransceiverId,
    receiving: bool,
    dependency_descriptor: bool,
) -> Result<(), rtc::PeerError> {
    let kind = session.transceiver_media_type(transceiver)?;
    if receiving {
        let capabilities = session.get_rtp_receiver_capabilities(kind)?;
        let codecs: Vec<_> = capabilities
            .codecs()
            .into_iter()
            .filter(|codec| {
                let name = codec.base_codec().name_bytes();
                if kind == rtc::MediaType::Audio {
                    name.eq_ignore_ascii_case(b"opus")
                } else {
                    name.eq_ignore_ascii_case(b"H264") || name.eq_ignore_ascii_case(b"rtx")
                }
            })
            .collect();
        session
            .set_rtp_codec_preferences(transceiver, &codecs)
            .map_err(|e| error(e.to_string()))?;
    }
    if kind == rtc::MediaType::Video {
        let mut extensions = session.header_extensions_to_negotiate(transceiver)?;
        for uri in [DD, VLA] {
            let extension = extensions
                .iter_mut()
                .find(|extension| extension.uri() == uri)
                .ok_or_else(|| error(format!("native H264 capability lacks {uri}")))?;
            extension.direction = if uri == DD && !dependency_descriptor {
                rtc::RtpHeaderExtensionDirection::Stopped
            } else {
                rtc::RtpHeaderExtensionDirection::SendReceive
            };
        }
        session.set_header_extensions_to_negotiate(transceiver, &extensions)?;
    }
    Ok(())
}

impl<R: rtc::SessionRuntime> NativePeer<R> {
    pub fn new(
        runtime: R,
        config: &MediaConfig,
        generation: Generation,
        topology: &MediaTopology,
        specs: &[DataChannelSpec],
    ) -> Result<Self, rtc::PeerError> {
        let mut profiles = HashMap::new();
        let slots = slots(topology);
        for profile in &config.video_publications {
            if !slots.contains(&MediaSlot::LocalVideo(profile.slot.clone()))
                || profiles
                    .insert(profile.slot.clone(), profile.source.clone())
                    .is_some()
            {
                return Err(error(
                    "video profile names must identify distinct local publication slots",
                ));
            }
        }
        let mut session = native_media::session(runtime, config.audio_input, config.video_input)?;
        let peer = session.create_peer(rtc::PeerConfiguration::default())?;
        let mut channels = BTreeMap::new();
        let mut channel_bindings = Vec::new();
        let mut signaling_channel = None;
        for spec in specs {
            let mut settings = rtc::DataChannelConfiguration {
                ordered: spec.ordered,
                ..rtc::DataChannelConfiguration::default()
            };
            match spec.reliability {
                DataChannelReliability::Reliable => {}
                DataChannelReliability::MaxRetransmits(count) => {
                    settings.max_retransmits = Some(i32::from(count));
                }
                DataChannelReliability::MaxPacketLifetime(ms) => {
                    settings.max_retransmit_time_ms = Some(i32::from(ms));
                }
            }
            let native = session
                .create_channel(peer, &spec.label, settings)
                .map_err(|e| error(e.to_string()))?;
            let channel = ChannelId::new(native.get())
                .ok_or_else(|| error("zero native channel identity"))?;
            channels.insert(channel, native);
            if signaling_channel.is_none() {
                signaling_channel = Some(channel);
            } else {
                channel_bindings.push(DataChannelBinding {
                    label: spec.label.clone(),
                    channel,
                });
            }
        }
        let signaling_channel =
            signaling_channel.ok_or_else(|| error("core did not request a signaling channel"))?;
        let mut transceivers = BTreeMap::new();
        let mut sources = BTreeMap::new();
        for slot in slots {
            let (kind, receiving) = match &slot {
                MediaSlot::LocalAudio(_) => (rtc::MediaType::Audio, false),
                MediaSlot::LocalVideo(_) => (rtc::MediaType::Video, false),
                MediaSlot::RemoteAudio(_) => (rtc::MediaType::Audio, true),
                MediaSlot::RemoteVideo(_) => (rtc::MediaType::Video, true),
            };
            let transceiver = if receiving {
                let mut init = rtc::RtpTransceiverInit::default();
                init.set_direction(rtc::RtpTransceiverDirectionValue::ReceiveOnly);
                session
                    .add_transceiver(peer, kind, &init)
                    .map_err(|e| error(e.to_string()))?
            } else {
                let (MediaSlot::LocalAudio(name) | MediaSlot::LocalVideo(name)) = &slot else {
                    return Err(error("source has a remote slot"));
                };
                let source_kind = if kind == rtc::MediaType::Audio {
                    SourceKind::Audio(config.audio_input)
                } else {
                    SourceKind::Video(config.video_input)
                };
                let source = NativeSource::create(
                    &mut session,
                    peer,
                    name,
                    source_kind,
                    &profiles
                        .get(name)
                        .cloned()
                        .unwrap_or_else(VideoSourceConfig::default),
                )?;
                let transceiver = source.transceiver;
                sources.insert(slot.clone(), source);
                transceiver
            };
            let dependency_descriptor = match &slot {
                MediaSlot::LocalVideo(name) => profiles
                    .get(name)
                    .is_none_or(|source| source.dependency_descriptor),
                _ => true,
            };
            codecs_and_extensions(&mut session, transceiver, receiving, dependency_descriptor)?;
            transceivers.insert(slot, transceiver);
        }
        let operation = session.create_offer(peer, false)?;
        Ok(Self {
            generation,
            session,
            peer,
            stage: Stage::Creating(operation),
            gathered: false,
            connection: rtc::ConnectionState::New,
            channels,
            signaling_channel,
            channel_bindings,
            pending_sends: BTreeMap::new(),
            queued_sends: VecDeque::new(),
            stats: None,
            transceivers,
            bindings: BTreeMap::new(),
            sources,
            receivers: BTreeMap::new(),
        })
    }

    pub fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if self
            .queued_sends
            .iter()
            .any(|effect| self.can_dispatch(effect))
        {
            Poll::Ready(())
        } else {
            self.session.poll_ready(cx)
        }
    }

    pub fn answer(&mut self, answer: String) -> Result<(), rtc::PeerError> {
        if !matches!(self.stage, Stage::Offered) {
            return Err(error("answer is not pending for this native generation"));
        }
        let operation = self.session.set_remote_description(
            self.peer,
            rtc::SessionDescription {
                kind: rtc::SessionDescriptionType::Answer,
                sdp: answer,
            },
        )?;
        self.stage = Stage::SettingRemote(operation);
        Ok(())
    }

    pub fn send(&mut self, effect: DataChannelEffect) -> Option<HostEvent> {
        let DataChannelEffect::Send {
            operation,
            generation,
            channel,
            ..
        } = &effect;
        let failure = if *generation != self.generation {
            Some("native generation has been retired")
        } else if !self.channels.contains_key(channel) {
            Some("unknown native data channel")
        } else if self
            .pending_sends
            .len()
            .saturating_add(self.queued_sends.len())
            >= PENDING_SENDS
        {
            Some("native send operation queue is full")
        } else {
            None
        };
        if let Some(message) = failure {
            return Some(HostEvent::DataChannel(
                agent_core::DataChannelEvent::SendFailed {
                    operation: *operation,
                    generation: *generation,
                    channel: *channel,
                    message: message.into(),
                },
            ));
        }
        // Core replay effects can arrive in a burst larger than the SDK's
        // per-channel bound. Stage only host operations within the existing
        // total budget. Native SCTP, reliability and payloads are unchanged.
        self.queued_sends.push_back(effect);
        None
    }

    fn can_dispatch(&self, effect: &DataChannelEffect) -> bool {
        let DataChannelEffect::Send { channel, .. } = effect;
        self.pending_sends
            .keys()
            .filter(|(id, _)| id == channel)
            .count()
            < NATIVE_CHANNEL_SENDS
    }

    fn dispatch_sends(&mut self) -> Vec<HostEvent> {
        let mut failures = Vec::new();
        // The first eligible operation preserves order within each channel
        // without letting one full channel block unrelated channels.
        while let Some(index) = self
            .queued_sends
            .iter()
            .position(|effect| self.can_dispatch(effect))
        {
            let Some(effect) = self.queued_sends.remove(index) else {
                break;
            };
            if let Some(failure) = self.send_now(effect) {
                failures.push(failure);
            }
        }
        failures
    }

    fn send_now(&mut self, effect: DataChannelEffect) -> Option<HostEvent> {
        let DataChannelEffect::Send {
            operation,
            generation,
            channel,
            binary,
            payload,
        } = effect;
        let failed = |message| {
            HostEvent::DataChannel(agent_core::DataChannelEvent::SendFailed {
                operation,
                generation,
                channel,
                message,
            })
        };
        if generation != self.generation {
            return Some(failed("native generation has been retired".into()));
        }
        if self.pending_sends.len() >= PENDING_SENDS {
            return Some(failed("native send completion queue is full".into()));
        }
        let Some(&native) = self.channels.get(&channel) else {
            return Some(failed("unknown native data channel".into()));
        };
        let message = rtc::DataChannelMessage {
            kind: if binary {
                rtc::DataChannelMessageKind::Binary
            } else {
                rtc::DataChannelMessageKind::Text
            },
            bytes: payload,
        };
        match self.session.send_async(native, message) {
            Ok(id) => {
                self.pending_sends.insert((channel, id.get()), operation);
                None
            }
            Err(e) => {
                let message = e.to_string();
                #[cfg(feature = "sim")]
                let message = format!(
                    "{message}; channel={channel:?} pending_on_channel={} pending_total={} buffered={:?} counters={:?}",
                    self.pending_sends
                        .keys()
                        .filter(|(id, _)| *id == channel)
                        .count(),
                    self.pending_sends.len(),
                    self.session.channel_buffered_amount(native),
                    self.session.channel_counters(native),
                );
                Some(failed(message))
            }
        }
    }

    pub fn request_stats(&mut self) -> Result<(), rtc::PeerError> {
        if matches!(self.stage, Stage::Ready) && self.stats.is_none() {
            self.stats = Some(self.session.request_stats(self.peer)?);
        }
        Ok(())
    }

    pub fn progress(&mut self) -> Result<Progress, rtc::PeerError> {
        let mut progress = Progress::default();
        for index in 0..EVENTS_PER_TURN {
            let Some(event) = self.session.try_peer_event(self.peer)? else {
                break;
            };
            progress.more |= index.saturating_add(1) == EVENTS_PER_TURN;
            match event {
                rtc::SessionEvent::OperationComplete(done) => {
                    if self.stats == Some(done.operation_id) {
                        self.stats = None;
                        done.result?;
                        continue;
                    }
                    let result = done.result?;
                    match self.stage {
                        Stage::Creating(operation) if operation == done.operation_id => {
                            let offer = result.ok_or_else(|| {
                                error("native CreateOffer returned no description")
                            })?;
                            let operation = self.session.set_local_description(self.peer, offer)?;
                            self.stage = Stage::SettingLocal(operation);
                        }
                        Stage::SettingLocal(operation) if operation == done.operation_id => {
                            self.stage = Stage::Gathering;
                        }
                        Stage::SettingRemote(operation) if operation == done.operation_id => {
                            self.stage = Stage::Ready;
                            progress
                                .events
                                .push(HostEvent::Rtc(RtcEvent::AnswerApplied {
                                    generation: self.generation,
                                }));
                            self.connection_event(&mut progress.events);
                        }
                        _ => return Err(error("unattributed native signaling completion")),
                    }
                }
                rtc::SessionEvent::IceGatheringStateChanged(rtc::IceGatheringState::Complete) => {
                    self.gathered = true;
                }
                rtc::SessionEvent::ConnectionStateChanged(state) => {
                    self.connection = state;
                    if matches!(self.stage, Stage::Ready) {
                        self.connection_event(&mut progress.events);
                    }
                }
                rtc::SessionEvent::Track {
                    transceiver,
                    receiver,
                } => self.refresh_receiver(transceiver, receiver, &mut progress.observations)?,
                rtc::SessionEvent::TrackRemoved { receiver } => {
                    if let Some(slot) = self
                        .receivers
                        .iter()
                        .find(|(_, value)| value.id == receiver)
                        .map(|(slot, _)| slot.clone())
                    {
                        if let Some(mut receiver) = self.receivers.remove(&slot) {
                            receiver.detach(&mut self.session)?;
                        }
                        progress
                            .observations
                            .push(Observation::TrackRemoved { slot });
                    }
                }
                rtc::SessionEvent::Stats(stats) => {
                    if self.stats != Some(stats.operation_id) {
                        return Err(error("unattributed native stats completion"));
                    }
                    self.stats = None;
                    progress
                        .observations
                        .push(Observation::Statistics(NativeStatistics {
                            generation: self.generation.get(),
                            timestamp_us: stats.timestamp_us,
                            report_json: stats
                                .report
                                .to_native_json()
                                .map_err(|e| error(e.to_string()))?,
                        }));
                }
                _ => {}
            }
        }
        if matches!(self.stage, Stage::Gathering) && self.gathered {
            let offer = self
                .session
                .descriptions(self.peer)?
                .local()
                .ok_or_else(|| error("native local description is absent"))?
                .sdp
                .clone();
            let mut mids = Vec::new();
            for (slot, &transceiver) in &self.transceivers {
                let mid = self.session.transceiver_mid(transceiver)?.ok_or_else(|| {
                    error("native transceiver MID is absent after local description")
                })?;
                mids.push((slot.clone(), mid));
            }
            let bindings = agent_core::negotiated_slot_bindings(&offer, mids).map_err(error)?;
            self.bindings = bindings
                .iter()
                .map(|binding| (binding.slot.clone(), binding.clone()))
                .collect();
            // Native receiver/track identity is available before media arrives.
            // A later Track callback refreshes the same receiver's observation.
            let remote: Vec<_> = self
                .transceivers
                .iter()
                .filter(|(slot, _)| {
                    matches!(slot, MediaSlot::RemoteAudio(_) | MediaSlot::RemoteVideo(_))
                })
                .map(|(slot, &id)| (slot.clone(), id))
                .collect();
            for (_, transceiver) in remote {
                let receiver = self.session.receiver(transceiver)?;
                self.refresh_receiver(transceiver, receiver, &mut progress.observations)?;
            }
            progress.events.push(HostEvent::Rtc(RtcEvent::OfferCreated {
                generation: self.generation,
                offer,
                resources: OfferResources {
                    slots: bindings,
                    signaling_channel: self.signaling_channel,
                    data_channels: self.channel_bindings.clone(),
                },
            }));
            self.stage = Stage::Offered;
        }
        let channels: Vec<_> = self
            .channels
            .iter()
            .map(|(&id, &native)| (id, native))
            .collect();
        for (channel, native) in channels {
            for index in 0..EVENTS_PER_TURN {
                let Some(event) = self.session.try_channel_event(native)? else {
                    break;
                };
                progress.more |= index.saturating_add(1) == EVENTS_PER_TURN;
                let event = match event {
                    rtc::DataChannelEvent::StateChanged(rtc::DataChannelState::Open) => {
                        Some(agent_core::DataChannelEvent::Opened {
                            generation: self.generation,
                            channel,
                        })
                    }
                    rtc::DataChannelEvent::StateChanged(rtc::DataChannelState::Closed) => {
                        Some(agent_core::DataChannelEvent::Closed {
                            generation: self.generation,
                            channel,
                        })
                    }
                    rtc::DataChannelEvent::Message(message) => {
                        Some(agent_core::DataChannelEvent::Message {
                            generation: self.generation,
                            channel,
                            payload: message.bytes,
                        })
                    }
                    rtc::DataChannelEvent::SendComplete(completion) => {
                        let operation = self
                            .pending_sends
                            .remove(&(channel, completion.send_id.get()))
                            .ok_or_else(|| error("unattributed native channel send completion"))?;
                        Some(match completion.result {
                            Ok(()) => agent_core::DataChannelEvent::Sent {
                                generation: self.generation,
                                channel,
                                operation,
                            },
                            Err(e) => agent_core::DataChannelEvent::SendFailed {
                                generation: self.generation,
                                channel,
                                operation,
                                message: e.to_string(),
                            },
                        })
                    }
                    rtc::DataChannelEvent::MessagesDropped { messages, bytes } => {
                        progress
                            .observations
                            .push(Observation::ChannelMessagesDropped {
                                channel,
                                messages,
                                bytes,
                            });
                        None
                    }
                    _ => None,
                };
                if let Some(event) = event {
                    progress.events.push(HostEvent::DataChannel(event));
                }
            }
        }
        // Consume terminal native outcomes before admitting the next burst.
        progress.events.extend(self.dispatch_sends());
        Ok(progress)
    }

    fn connection_event(&self, events: &mut Vec<HostEvent>) {
        let event = match self.connection {
            rtc::ConnectionState::Connected => Some(RtcEvent::Connected {
                generation: self.generation,
            }),
            rtc::ConnectionState::Disconnected => Some(RtcEvent::Disconnected {
                generation: self.generation,
            }),
            rtc::ConnectionState::Failed => Some(RtcEvent::Failed {
                generation: self.generation,
                message: "native ICE/DTLS connection failed".into(),
            }),
            _ => None,
        };
        if let Some(event) = event {
            events.push(HostEvent::Rtc(event));
        }
    }

    fn refresh_receiver(
        &mut self,
        transceiver: rtc::SessionTransceiverId,
        receiver: rtc::SessionReceiverId,
        observations: &mut Vec<Observation>,
    ) -> Result<(), rtc::PeerError> {
        let Some(slot) = self
            .transceivers
            .iter()
            .find(|(_, id)| **id == transceiver)
            .map(|(slot, _)| slot.clone())
        else {
            return Err(error("native track references an unknown transceiver"));
        };
        if !matches!(slot, MediaSlot::RemoteAudio(_) | MediaSlot::RemoteVideo(_)) {
            return Ok(());
        }
        let Some(binding) = self.bindings.get(&slot) else {
            return Ok(());
        };
        let mut value = NativeReceiver::new(
            &mut self.session,
            receiver,
            self.generation.get(),
            binding.mid.clone(),
            binding.media_index,
        )?;
        if self
            .receivers
            .get(&slot)
            .is_some_and(|old| old.id != receiver)
        {
            if let Some(mut old) = self.receivers.remove(&slot) {
                old.detach(&mut self.session)?;
            }
            observations.push(Observation::TrackRemoved { slot: slot.clone() });
        }
        value.mode = self.receivers.get(&slot).and_then(|value| value.mode);
        observations.push(Observation::Track {
            slot: slot.clone(),
            track: value.track.clone(),
        });
        self.receivers.insert(slot, value);
        Ok(())
    }

    #[cfg(feature = "sim")]
    pub fn media_indices(&self) -> BTreeMap<MediaSlot, u32> {
        self.bindings
            .iter()
            .map(|(slot, binding)| (slot.clone(), binding.media_index))
            .collect()
    }

    #[cfg(test)]
    pub fn source_identity(&self, slot: &MediaSlot) -> Option<SourceIdentity> {
        self.sources
            .get(slot)
            .filter(|source| source.active)
            .map(|source| SourceIdentity {
                generation: self.generation.get(),
                source: source.id.get(),
            })
    }

    pub fn replace_source(&mut self, slot: &MediaSlot) -> Result<SourceIdentity, rtc::PeerError> {
        let source = self
            .sources
            .get_mut(slot)
            .ok_or_else(|| error("unknown native publication slot"))?;
        let (MediaSlot::LocalAudio(name) | MediaSlot::LocalVideo(name)) = slot else {
            return Err(error("not a local source"));
        };
        source.replace(&mut self.session, name)?;
        Ok(SourceIdentity {
            generation: self.generation.get(),
            source: source.id.get(),
        })
    }

    fn source(
        &self,
        slot: &MediaSlot,
        identity: &SourceIdentity,
    ) -> Result<&NativeSource, rtc::PeerError> {
        self.sources
            .get(slot)
            .filter(|source| {
                source.active
                    && identity.generation == self.generation.get()
                    && identity.source == source.id.get()
            })
            .ok_or_else(|| rtc::PeerError {
                kind: rtc::PeerErrorKind::Closed,
                message: "source identity has been retired".into(),
            })
    }

    pub fn submit(
        &mut self,
        slot: &MediaSlot,
        identity: &SourceIdentity,
        frame: crate::InputFrame,
    ) -> Result<(), rtc::PeerError> {
        self.source(slot, identity)?;
        self.sources
            .get(slot)
            .ok_or_else(|| error("unknown source"))?
            .push(&mut self.session, frame)
    }
    pub fn close_source(
        &mut self,
        slot: &MediaSlot,
        identity: &SourceIdentity,
    ) -> Result<(), rtc::PeerError> {
        self.source(slot, identity)?;
        self.sources
            .get_mut(slot)
            .ok_or_else(|| error("unknown source"))?
            .close(&mut self.session)
    }
    pub fn enable(
        &mut self,
        slot: &MediaSlot,
        identity: &SourceIdentity,
        enabled: bool,
    ) -> Result<bool, rtc::PeerError> {
        self.source(slot, identity)?;
        self.sources
            .get(slot)
            .ok_or_else(|| error("unknown source"))?
            .set_enabled(&mut self.session, enabled)
    }
    pub fn capture_time_us(
        &self,
        slot: &MediaSlot,
        identity: &SourceIdentity,
    ) -> Result<i64, rtc::PeerError> {
        self.source(slot, identity)?;
        self.session
            .clock_timestamp()?
            .microseconds()
            .map_err(|value| error(format!("invalid native capture clock: {value}")))
    }
    pub fn encoding_state(
        &mut self,
        slot: &MediaSlot,
        identity: &SourceIdentity,
    ) -> Result<Vec<crate::EncodingState>, rtc::PeerError> {
        self.source(slot, identity)?;
        self.sources
            .get(slot)
            .ok_or_else(|| error("unknown source"))?
            .encoding_state(&mut self.session)
    }
    pub fn set_limits(
        &mut self,
        slot: &MediaSlot,
        identity: &SourceIdentity,
        limits: &[crate::EncodingLimits],
    ) -> Result<Vec<crate::EncodingState>, rtc::PeerError> {
        self.source(slot, identity)?;
        self.sources
            .get(slot)
            .ok_or_else(|| error("unknown source"))?
            .set_limits(&mut self.session, limits)
    }
    pub fn feedback(
        &mut self,
        slot: &MediaSlot,
        identity: &SourceIdentity,
    ) -> Result<crate::ProducerFeedback, rtc::PeerError> {
        self.source(slot, identity)?;
        self.sources
            .get(slot)
            .ok_or_else(|| error("unknown source"))?
            .feedback(&mut self.session)
    }
    pub fn attach(
        &mut self,
        slot: &MediaSlot,
        mode: ReceiveMode,
        observer: Option<&crate::DecodedVideoObserver>,
    ) -> Result<crate::RemoteTrack, rtc::PeerError> {
        let receiver = self
            .receivers
            .get_mut(slot)
            .ok_or_else(|| error("native remote track is not available"))?;
        receiver.attach(&mut self.session, mode, observer)?;
        Ok(receiver.track.clone())
    }
    pub fn detach(&mut self, slot: &MediaSlot) -> Result<(), rtc::PeerError> {
        self.receivers
            .get_mut(slot)
            .ok_or_else(|| error("unknown receiver"))?
            .detach(&mut self.session)
    }
    pub fn frame(
        &mut self,
        slot: &MediaSlot,
        playout_due: bool,
    ) -> Result<Option<crate::RemoteFrame>, rtc::PeerError> {
        self.receivers
            .get_mut(slot)
            .ok_or_else(|| error("unknown receiver"))?
            .next(&mut self.session, playout_due)
    }
    pub fn dropped_frames(&mut self, slot: &MediaSlot) -> Result<u64, rtc::PeerError> {
        let receiver = self
            .receivers
            .get(slot)
            .ok_or_else(|| error("unknown receiver"))?;
        if receiver.mode == Some(ReceiveMode::EncodedAndDecoded) {
            self.session.dropped_encoded_video_frames(receiver.id)
        } else {
            self.session.dropped_frames(receiver.id)
        }
    }
    pub fn dropped_decoded_frames(&mut self, slot: &MediaSlot) -> Result<u64, rtc::PeerError> {
        let receiver = self
            .receivers
            .get(slot)
            .ok_or_else(|| error("unknown receiver"))?;
        self.session.dropped_raw_video_frames(receiver.id)
    }
    pub fn request_keyframe(&mut self, slot: &MediaSlot) -> Result<(), rtc::PeerError> {
        let receiver = self
            .receivers
            .get(slot)
            .ok_or_else(|| error("unknown receiver"))?;
        self.session.request_keyframe(receiver.id)
    }
    pub fn shutdown(&mut self) -> Result<Vec<HostEvent>, rtc::PeerError> {
        self.session.shutdown()?;
        let mut retired: Vec<_> = std::mem::take(&mut self.pending_sends)
            .into_iter()
            .map(|((channel, _), operation)| {
                HostEvent::DataChannel(agent_core::DataChannelEvent::SendFailed {
                    generation: self.generation,
                    channel,
                    operation,
                    message: "native generation retired before send completion was observed".into(),
                })
            })
            .collect();
        retired.extend(self.queued_sends.drain(..).map(|effect| {
            let DataChannelEffect::Send {
                operation,
                generation,
                channel,
                ..
            } = effect;
            HostEvent::DataChannel(agent_core::DataChannelEvent::SendFailed {
                generation,
                channel,
                operation,
                message: "native generation retired before send admission".into(),
            })
        }));
        Ok(retired)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn offer<R: rtc::SessionRuntime>(runtime: R, config: MediaConfig, mut drive: impl FnMut()) {
        let topology = MediaTopology {
            local_video: 1,
            local_audio: 1,
            remote_video: 1,
            remote_audio: 1,
        };
        let mut core = agent_core::Agent::new(agent_core::AgentConfig {
            endpoint: "http://pulsebeam.test".into(),
            token: "token".into(),
            topology,
            retry: agent_core::RetryPolicy::default(),
            log_level: agent_core::LogLevel::default(),
        })
        .unwrap();
        core.command(agent_core::AgentCommand::ReplaceDesired(
            agent_core::DesiredState {
                revision: 1,
                connected: true,
                ..agent_core::DesiredState::default()
            },
        ))
        .unwrap();
        let (generation, topology, specs) = loop {
            match core.next_effect().expect("core requested an offer") {
                agent_core::Effect::Rtc(agent_core::RtcEffect::CreateOffer {
                    generation,
                    topology,
                    data_channels,
                }) => break (generation, topology, data_channels),
                _ => continue,
            }
        };
        let mut peer = NativePeer::new(runtime, &config, generation, &topology, &specs).unwrap();
        let resources = loop {
            let progress = peer.progress().unwrap();
            let mut found = None;
            for event in progress.events {
                if let HostEvent::Rtc(RtcEvent::OfferCreated {
                    generation: observed,
                    offer,
                    resources,
                }) = event
                {
                    assert_eq!(observed, generation);
                    assert!(offer.contains(DD));
                    let local_video = offer
                        .split("m=video ")
                        .nth(1)
                        .unwrap()
                        .split("\r\nm=")
                        .next()
                        .unwrap();
                    let expected_dd = config
                        .video_publications
                        .iter()
                        .find(|value| value.slot == "v0")
                        .is_none_or(|value| value.source.dependency_descriptor);
                    assert_eq!(
                        local_video.contains(DD),
                        expected_dd,
                        "native per-publication DD negotiation"
                    );
                    assert!(offer.contains("H264/90000"));
                    assert!(!offer.contains("VP8/90000"));
                    assert!(offer.contains("opus/48000"));
                    found = Some(resources);
                }
            }
            if let Some(resources) = found {
                break resources;
            }
            drive();
        };
        assert_eq!(resources.slots.len(), 4);
        assert_eq!(peer.sources.len(), 2);
        assert_eq!(peer.receivers.len(), 2);
        let local = MediaSlot::LocalVideo("v0".into());
        let old = peer.source_identity(&local).unwrap();
        let before = peer
            .session
            .clock_timestamp()
            .unwrap()
            .microseconds()
            .unwrap();
        let capture = peer.capture_time_us(&local, &old).unwrap();
        let after = peer
            .session
            .clock_timestamp()
            .unwrap()
            .microseconds()
            .unwrap();
        assert!(before <= capture && capture <= after);
        assert!(
            capture > 0,
            "native capture clock must not be relative PTS zero"
        );
        let current = peer.replace_source(&local).unwrap();
        assert_ne!(old, current);
        assert!(peer.enable(&local, &old, true).is_err());
        assert!(peer.capture_time_us(&local, &old).is_err());
        assert!(peer.capture_time_us(&local, &current).is_ok());
        assert!(!peer.enable(&local, &current, false).unwrap());
        assert!(peer.enable(&local, &current, true).unwrap());
        let remote = MediaSlot::RemoteAudio(0);
        let track = peer.attach(&remote, ReceiveMode::Decoded, None).unwrap();
        assert_eq!(track.generation, generation.get());
        assert!(!track.track_id.is_empty());
        assert!(peer.frame(&remote, false).unwrap().is_none());
        assert!(peer.attach(&remote, ReceiveMode::Encoded, None).is_err());
        peer.detach(&remote).unwrap();
        // Native PCM attachment can precede negotiation. Encoded interception
        // additionally requires Opus in the receiver's actual GetParameters.
        let refused = peer
            .attach(&remote, ReceiveMode::Encoded, None)
            .unwrap_err();
        assert_eq!(refused.kind, rtc::PeerErrorKind::InvalidState);
        assert_eq!(peer.receivers.get(&remote).unwrap().mode, None);
        assert!(peer.dropped_frames(&remote).is_err());
        let refused = peer
            .attach(&remote, ReceiveMode::EncodedAndDecoded, None)
            .unwrap_err();
        assert_eq!(refused.kind, rtc::PeerErrorKind::UnsupportedParameter);
        assert_eq!(peer.receivers.get(&remote).unwrap().mode, None);
        peer.close_source(&local, &current).unwrap();
        assert!(peer.encoding_state(&local, &current).is_err());
        assert!(peer.capture_time_us(&local, &current).is_err());
        assert!(peer.source_identity(&local).is_none());
        let next = peer.replace_source(&local).unwrap();
        assert_ne!(next, current);
        assert_eq!(peer.session.transceivers(peer.peer).unwrap().len(), 4);
        peer.shutdown().unwrap();
    }

    // Native signaling and graph ownership only. Real media and SFU acceptance
    // must additionally prove encode/forward/decode, not just these offers.
    #[test]
    fn same_native_peer_owns_offers_sources_and_receivers_in_both_runtimes() {
        let _guard = native_media::NATIVE_TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for config in [
            MediaConfig::default(),
            MediaConfig {
                audio_input: crate::AudioInput::EncodedOpus { channels: 1 },
                video_input: crate::VideoInput::EncodedH264,
                video_publications: vec![crate::VideoPublicationConfig {
                    slot: "v0".into(),
                    source: VideoSourceConfig {
                        dependency_descriptor: false,
                        ..VideoSourceConfig::default()
                    },
                }],
            },
            MediaConfig {
                audio_input: crate::AudioInput::EncodedOpus { channels: 2 },
                video_input: crate::VideoInput::EncodedH264,
                video_publications: vec![crate::VideoPublicationConfig {
                    slot: "v0".into(),
                    source: VideoSourceConfig {
                        encodings: vec![crate::Encoding {
                            temporal_layers: 3,
                            ..crate::Encoding::default()
                        }],
                        ..VideoSourceConfig::default()
                    },
                }],
            },
        ] {
            let deadline = Instant::now() + Duration::from_secs(15);
            offer(rtc::ThreadedRuntime, config.clone(), || {
                assert!(Instant::now() < deadline, "native gathered offer stalled");
                std::thread::yield_now();
            });
            let world = rtc::ControlledWorld::acquire(17, Duration::from_secs(3)).unwrap();
            let network = world.create_network().unwrap();
            let endpoint = network
                .register_endpoint("192.0.2.1".parse().unwrap())
                .unwrap();
            let runtime = world.runtime().unwrap().with_endpoint(&endpoint).unwrap();
            let mut steps = 0;
            offer(runtime, config, || {
                steps += 1;
                assert!(steps < 4096, "managed gathered offer stalled");
                let progress = world.pump(128);
                assert!(!progress.blocked);
                if progress.dispatched == 0 {
                    let next = world
                        .next_deadline()
                        .expect("managed offer needs scheduled native work");
                    world.advance(next.saturating_sub(world.now())).unwrap();
                }
            });
        }
    }
}
