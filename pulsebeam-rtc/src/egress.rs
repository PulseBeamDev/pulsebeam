use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    time::Instant,
};

use crate::{
    EgressSlot, MediaKind, PacerDecision, PacingClass, PacketPacer, ProbeDecision, SendId,
};

const HISTORY_CAPACITY: usize = 2_048;
const MAPPING_CAPACITY: usize = 8_192;
const QUEUE_CAPACITY: usize = 2_048;
const PROBE_PADDING_BYTES: usize = 224;

#[derive(Clone)]
pub(crate) struct EgressCodecConfig {
    pub(crate) name: Box<str>,
    pub(crate) primary_payload_type: u8,
    pub(crate) rtx_payload_type: Option<u8>,
}

#[derive(Clone)]
pub(crate) struct EgressSlotConfig {
    pub(crate) slot: EgressSlot,
    pub(crate) kind: MediaKind,
    pub(crate) mid: Box<[u8]>,
    pub(crate) primary_ssrc: u32,
    pub(crate) rtx_ssrc: Option<u32>,
    pub(crate) codecs: Box<[EgressCodecConfig]>,
    pub(crate) mid_extension: Option<u8>,
    pub(crate) twcc_extension: Option<u8>,
    pub(crate) absolute_capture_time_extension: Option<u8>,
    pub(crate) audio_level_extension: Option<u8>,
    pub(crate) dependency_descriptor_extension: Option<u8>,
}

pub(crate) struct ForwardAdmission<'a> {
    pub(crate) codec: &'a str,
    pub(crate) logical_sequence: u64,
    pub(crate) timestamp: u64,
    pub(crate) marker: bool,
    pub(crate) payload: &'a [u8],
    pub(crate) absolute_capture_time: Option<&'a [u8]>,
    pub(crate) audio_level: Option<i8>,
    pub(crate) dependency_descriptor: Option<&'a [u8]>,
    pub(crate) ingress_at: Instant,
    pub(crate) admitted_at: Instant,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct EgressLifecycle {
    pub(crate) ingress_at: Instant,
    pub(crate) admitted_at: Instant,
    pub(crate) eligible_at: Instant,
}

pub(crate) struct ReadyRtp {
    pub(crate) bytes: Vec<u8>,
    pub(crate) extended_sequence: u64,
    pub(crate) send_id: SendId,
    pub(crate) twcc_offset: Option<usize>,
    pub(crate) probe_id: Option<u32>,
    pub(crate) completed_probe: Option<u32>,
    pub(crate) lifecycle: Option<EgressLifecycle>,
}

#[derive(Clone)]
struct StoredPacket {
    bytes: Vec<u8>,
    extended_sequence: u64,
    sequence: u16,
    timestamp: u32,
    marker: bool,
    payload: Vec<u8>,
    extensions: Vec<(u8, Vec<u8>)>,
    twcc_offset: Option<usize>,
    codec: EgressCodecConfig,
}

struct QueuedRtp {
    bytes: Vec<u8>,
    extended_sequence: u64,
    twcc_offset: Option<usize>,
    class: PacingClass,
    probe_eligible: bool,
    lifecycle: Option<EgressLifecycle>,
}

#[derive(Default)]
struct LogicalSequenceMap {
    logical_to_wire: BTreeMap<u64, u64>,
    used_wire: BTreeSet<u64>,
    next_wire: u64,
}

impl LogicalSequenceMap {
    fn map(&mut self, logical: u64) -> u64 {
        if let Some(wire) = self.logical_to_wire.get(&logical).copied() {
            return wire;
        }
        let predecessor = self
            .logical_to_wire
            .range(..logical)
            .next_back()
            .map(|(logical, wire)| (*logical, *wire));
        let successor = self
            .logical_to_wire
            .range(logical.saturating_add(1)..)
            .next()
            .map(|(logical, wire)| (*logical, *wire));
        let preferred = predecessor
            .and_then(|(previous_logical, previous_wire)| {
                previous_wire.checked_add(logical.saturating_sub(previous_logical))
            })
            .or_else(|| {
                successor.and_then(|(next_logical, next_wire)| {
                    next_wire.checked_sub(next_logical.saturating_sub(logical))
                })
            })
            .unwrap_or(self.next_wire);
        let upper = successor.map(|(_, wire)| wire).unwrap_or(u64::MAX);
        let mut wire = preferred;
        while wire < upper && self.used_wire.contains(&wire) {
            wire = wire.saturating_add(1);
        }
        if wire >= upper || self.used_wire.contains(&wire) {
            wire = self.next_unused();
        }
        let inserted = self.used_wire.insert(wire);
        debug_assert!(inserted, "wire media sequences never collide");
        let previous = self.logical_to_wire.insert(logical, wire);
        debug_assert!(previous.is_none(), "a logical sequence maps once");
        self.next_wire = self.next_wire.max(wire.saturating_add(1));
        self.prune();
        wire
    }

    fn allocate_padding(&mut self) -> u64 {
        let wire = self.next_unused();
        let inserted = self.used_wire.insert(wire);
        debug_assert!(inserted, "primary padding consumes a fresh wire sequence");
        self.next_wire = wire.saturating_add(1);
        wire
    }

    fn next_unused(&self) -> u64 {
        let mut wire = self.next_wire;
        while self.used_wire.contains(&wire) {
            wire = wire.saturating_add(1);
        }
        wire
    }

    fn prune(&mut self) {
        while self.logical_to_wire.len() > MAPPING_CAPACITY {
            let Some((&logical, &wire)) = self.logical_to_wire.first_key_value() else {
                break;
            };
            self.logical_to_wire.remove(&logical);
            self.used_wire.remove(&wire);
        }
        debug_assert!(self.logical_to_wire.len() <= MAPPING_CAPACITY);
    }
}

struct SlotState {
    config: EgressSlotConfig,
    sequences: LogicalSequenceMap,
    next_rtx_sequence: u64,
    history: VecDeque<StoredPacket>,
}

pub(crate) struct EgressEngine {
    slots: HashMap<EgressSlot, SlotState>,
    by_ssrc: HashMap<u32, EgressSlot>,
    audio: VecDeque<QueuedRtp>,
    retransmission: VecDeque<QueuedRtp>,
    video: VecDeque<QueuedRtp>,
    padding: VecDeque<QueuedRtp>,
    probe_starter: Option<QueuedRtp>,
    pacer: PacketPacer,
    next_send_id: u64,
    next_internal_sequence: u64,
}

impl EgressEngine {
    pub(crate) fn new(now: Instant, configs: impl IntoIterator<Item = EgressSlotConfig>) -> Self {
        let mut slots = HashMap::new();
        let mut by_ssrc = HashMap::new();
        for config in configs {
            debug_assert_ne!(
                config.primary_ssrc, 0,
                "application media never owns SSRC 0"
            );
            let previous = by_ssrc.insert(config.primary_ssrc, config.slot);
            debug_assert!(previous.is_none(), "primary SSRCs are unique");
            if let Some(rtx_ssrc) = config.rtx_ssrc {
                debug_assert_ne!(rtx_ssrc, 0, "RTX never owns SSRC 0");
                let previous = by_ssrc.insert(rtx_ssrc, config.slot);
                debug_assert!(previous.is_none(), "RTX SSRCs are unique");
            }
            let slot = config.slot;
            let previous = slots.insert(
                slot,
                SlotState {
                    config,
                    sequences: LogicalSequenceMap::default(),
                    next_rtx_sequence: 0,
                    history: VecDeque::with_capacity(HISTORY_CAPACITY),
                },
            );
            debug_assert!(previous.is_none(), "egress slots are unique");
        }
        Self {
            slots,
            by_ssrc,
            audio: VecDeque::new(),
            retransmission: VecDeque::new(),
            video: VecDeque::new(),
            padding: VecDeque::new(),
            probe_starter: None,
            pacer: PacketPacer::new(now, crate::DEFAULT_INITIAL_BITRATE_BPS),
            next_send_id: 0,
            next_internal_sequence: 0,
        }
    }

    pub(crate) fn set_pacing_rate(&mut self, now: Instant, bitrate_bps: u64) {
        self.pacer.set_rate(now, bitrate_bps);
    }

    pub(crate) fn start_probe(&mut self, now: Instant, probe: ProbeDecision) {
        self.probe_starter = self.probe_starter_packet(now);
        debug_assert!(
            self.probe_starter.is_some(),
            "TWCC probing has a negotiated RTP namespace"
        );
        self.pacer.start_probe(
            now,
            probe.id(),
            probe.target_bitrate_bps(),
            probe.packet_count(),
            probe.min_duration(),
        );
    }

    fn probe_starter_packet(&mut self, now: Instant) -> Option<QueuedRtp> {
        self.rtx_padding_packet(now, 1)
            .or_else(|| {
                (self.audio.is_empty() && self.retransmission.is_empty() && self.video.is_empty())
                    .then(|| self.primary_padding_packet(now, 1))
                    .flatten()
            })
            .or_else(|| self.internal_padding_packet(now, 1))
    }

    pub(crate) fn has_active_probe(&self) -> bool {
        self.pacer.has_active_probe()
    }

    pub(crate) fn slot_for_ssrc(&self, ssrc: u32) -> Option<EgressSlot> {
        self.by_ssrc.get(&ssrc).copied()
    }

    pub(crate) fn admit(
        &mut self,
        slot: EgressSlot,
        packet: ForwardAdmission<'_>,
    ) -> Result<(), ()> {
        debug_assert!(packet.admitted_at >= packet.ingress_at);
        debug_assert!(
            !packet.payload.is_empty(),
            "forwarded media contains a payload"
        );
        let state = self.slots.get_mut(&slot).ok_or(())?;
        let codec = state
            .config
            .codecs
            .iter()
            .find(|codec| codec.name.eq_ignore_ascii_case(packet.codec))
            .cloned()
            .ok_or(())?;
        let extended_sequence = state.sequences.map(packet.logical_sequence);
        let extensions = media_extensions(&state.config, &packet);
        let encoded = encode_rtp(
            codec.primary_payload_type,
            packet.marker,
            extended_sequence,
            packet.timestamp,
            state.config.primary_ssrc,
            &extensions,
            packet.payload,
            0,
        )?;
        let stored = StoredPacket {
            bytes: encoded.bytes.clone(),
            extended_sequence,
            sequence: low_u16(extended_sequence),
            timestamp: low_u32(packet.timestamp),
            marker: packet.marker,
            payload: packet.payload.to_vec(),
            extensions,
            twcc_offset: encoded.twcc_offset,
            codec,
        };
        state.history.push_back(stored);
        while state.history.len() > HISTORY_CAPACITY {
            state.history.pop_front();
        }
        debug_assert!(state.history.len() <= HISTORY_CAPACITY);
        let queued = QueuedRtp {
            bytes: encoded.bytes,
            extended_sequence,
            twcc_offset: encoded.twcc_offset,
            class: if state.config.kind == MediaKind::Audio {
                PacingClass::Audio
            } else {
                PacingClass::Video
            },
            probe_eligible: true,
            lifecycle: Some(EgressLifecycle {
                ingress_at: packet.ingress_at,
                admitted_at: packet.admitted_at,
                eligible_at: packet.admitted_at,
            }),
        };
        self.push(queued)
    }

    pub(crate) fn handle_nack(&mut self, media_ssrc: u32, sequences: &[u16], now: Instant) {
        let Some(slot) = self.by_ssrc.get(&media_ssrc).copied() else {
            return;
        };
        let Some(state) = self.slots.get_mut(&slot) else {
            debug_assert!(false, "an indexed SSRC has a slot");
            return;
        };
        for sequence in sequences {
            let Some(stored) = state
                .history
                .iter()
                .rev()
                .find(|packet| packet.sequence == *sequence)
                .cloned()
            else {
                continue;
            };
            let queued = if let (Some(rtx_ssrc), Some(rtx_payload_type)) =
                (state.config.rtx_ssrc, stored.codec.rtx_payload_type)
            {
                let rtx_sequence = state.next_rtx_sequence;
                state.next_rtx_sequence = state.next_rtx_sequence.saturating_add(1);
                let mut payload = Vec::with_capacity(stored.payload.len().saturating_add(2));
                payload.extend_from_slice(&stored.sequence.to_be_bytes());
                payload.extend_from_slice(&stored.payload);
                let Ok(encoded) = encode_rtp(
                    rtx_payload_type,
                    stored.marker,
                    rtx_sequence,
                    u64::from(stored.timestamp),
                    rtx_ssrc,
                    &stored.extensions,
                    &payload,
                    0,
                ) else {
                    debug_assert!(false, "negotiated RTX extensions encode");
                    continue;
                };
                QueuedRtp {
                    bytes: encoded.bytes,
                    extended_sequence: rtx_sequence,
                    twcc_offset: encoded.twcc_offset,
                    class: PacingClass::Retransmission,
                    probe_eligible: true,
                    lifecycle: Some(EgressLifecycle {
                        ingress_at: now,
                        admitted_at: now,
                        eligible_at: now,
                    }),
                }
            } else {
                QueuedRtp {
                    bytes: stored.bytes,
                    extended_sequence: stored.extended_sequence,
                    twcc_offset: stored.twcc_offset,
                    class: PacingClass::Retransmission,
                    probe_eligible: true,
                    lifecycle: Some(EgressLifecycle {
                        ingress_at: now,
                        admitted_at: now,
                        eligible_at: now,
                    }),
                }
            };
            if self.retransmission.len() < QUEUE_CAPACITY {
                self.retransmission.push_back(queued);
            }
        }
    }

    pub(crate) fn ensure_probe_fallback(&mut self, now: Instant) {
        if !self.pacer.has_active_probe() || self.has_probe_eligible_traffic() {
            return;
        }
        if self.queue_payload_rtx_probe(now) {
            return;
        }
        if self.queue_primary_padding(now) {
            return;
        }
        self.queue_internal_padding(now);
    }

    pub(crate) fn next_ready(&mut self, now: Instant) -> Option<Instant> {
        if !self.pacer.has_active_probe() {
            self.probe_starter = None;
            self.padding.clear();
        }
        let probing = self.pacer.has_active_probe();
        let queued = self.front_for(probing)?;
        let class = if probing && queued.probe_eligible {
            PacingClass::Video
        } else {
            queued.class
        };
        Some(self.pacer.next_ready(now, queued.bytes.len(), class))
    }

    pub(crate) fn poll_ready(&mut self, now: Instant) -> Option<ReadyRtp> {
        if !self.pacer.has_active_probe() {
            self.probe_starter = None;
            self.padding.clear();
        }
        let probing = self.pacer.has_active_probe();
        let queued = self.front_for(probing)?;
        let class = if probing && queued.probe_eligible {
            PacingClass::Video
        } else {
            queued.class
        };
        match self.pacer.admit(now, queued.bytes.len(), class) {
            PacerDecision::Deferred { eligible_at } => {
                debug_assert!(
                    eligible_at > now,
                    "deferred packets have a future eligibility"
                );
                None
            }
            PacerDecision::Admitted {
                eligible_at,
                probe_id,
                probe_complete,
            } => {
                debug_assert!(eligible_at <= now, "admitted packets are eligible");
                let mut queued = self.pop_front_for(probing)?;
                if let Some(lifecycle) = queued.lifecycle.as_mut() {
                    lifecycle.eligible_at = now;
                    debug_assert!(lifecycle.eligible_at >= lifecycle.admitted_at);
                }
                let send_id = SendId::new(self.next_send_id);
                self.next_send_id = self.next_send_id.wrapping_add(1);
                Some(ReadyRtp {
                    bytes: queued.bytes,
                    extended_sequence: queued.extended_sequence,
                    send_id,
                    twcc_offset: queued.twcc_offset,
                    probe_id,
                    completed_probe: probe_complete.then_some(probe_id).flatten(),
                    lifecycle: queued.lifecycle,
                })
            }
        }
    }

    fn has_probe_eligible_traffic(&self) -> bool {
        self.probe_starter
            .iter()
            .chain(&self.audio)
            .chain(&self.retransmission)
            .chain(&self.video)
            .chain(&self.padding)
            .any(|packet| packet.probe_eligible)
    }

    fn queue_payload_rtx_probe(&mut self, now: Instant) -> bool {
        for state in self.slots.values_mut() {
            let Some(stored) = state
                .history
                .iter()
                .max_by_key(|packet| packet.payload.len())
                .cloned()
            else {
                continue;
            };
            let (Some(rtx_ssrc), Some(rtx_payload_type)) =
                (state.config.rtx_ssrc, stored.codec.rtx_payload_type)
            else {
                continue;
            };
            let rtx_sequence = state.next_rtx_sequence;
            state.next_rtx_sequence = state.next_rtx_sequence.saturating_add(1);
            let mut payload = Vec::with_capacity(stored.payload.len().saturating_add(2));
            payload.extend_from_slice(&stored.sequence.to_be_bytes());
            payload.extend_from_slice(&stored.payload);
            let Ok(encoded) = encode_rtp(
                rtx_payload_type,
                stored.marker,
                rtx_sequence,
                u64::from(stored.timestamp),
                rtx_ssrc,
                &stored.extensions,
                &payload,
                0,
            ) else {
                continue;
            };
            self.padding
                .push_back(probe_packet(encoded, rtx_sequence, now));
            return true;
        }
        false
    }

    fn rtx_padding_packet(&mut self, now: Instant, padding_bytes: usize) -> Option<QueuedRtp> {
        debug_assert!(padding_bytes > 0);
        for state in self.slots.values_mut() {
            let Some(stored) = state.history.back() else {
                continue;
            };
            let (Some(rtx_ssrc), Some(rtx_payload_type)) =
                (state.config.rtx_ssrc, stored.codec.rtx_payload_type)
            else {
                continue;
            };
            let sequence = state.next_rtx_sequence;
            state.next_rtx_sequence = state.next_rtx_sequence.saturating_add(1);
            let Ok(encoded) = encode_rtp(
                rtx_payload_type,
                false,
                sequence,
                u64::from(stored.timestamp),
                rtx_ssrc,
                &base_extensions(&state.config),
                &[],
                padding_bytes,
            ) else {
                continue;
            };
            return Some(probe_packet(encoded, sequence, now));
        }
        None
    }

    fn queue_primary_padding(&mut self, now: Instant) -> bool {
        let Some(packet) = self.primary_padding_packet(now, PROBE_PADDING_BYTES) else {
            return false;
        };
        self.padding.push_back(packet);
        true
    }

    fn primary_padding_packet(&mut self, now: Instant, padding_bytes: usize) -> Option<QueuedRtp> {
        debug_assert!(padding_bytes > 0);
        for state in self.slots.values_mut() {
            let Some(stored) = state.history.back() else {
                continue;
            };
            let sequence = state.sequences.allocate_padding();
            let Ok(encoded) = encode_rtp(
                stored.codec.primary_payload_type,
                false,
                sequence,
                u64::from(stored.timestamp),
                state.config.primary_ssrc,
                &base_extensions(&state.config),
                &[],
                padding_bytes,
            ) else {
                continue;
            };
            return Some(probe_packet(encoded, sequence, now));
        }
        None
    }

    fn queue_internal_padding(&mut self, now: Instant) {
        if let Some(packet) = self.internal_padding_packet(now, PROBE_PADDING_BYTES) {
            self.padding.push_back(packet);
        }
    }

    fn internal_padding_packet(&mut self, now: Instant, padding_bytes: usize) -> Option<QueuedRtp> {
        debug_assert!(padding_bytes > 0);
        let Some(config) = self
            .slots
            .values()
            .map(|state| &state.config)
            .find(|config| config.twcc_extension.is_some())
        else {
            return None;
        };
        let sequence = self.next_internal_sequence;
        self.next_internal_sequence = self.next_internal_sequence.saturating_add(1);
        let extensions = config
            .twcc_extension
            .map(|id| vec![(id, vec![0, 0])])
            .unwrap_or_default();
        let Ok(encoded) = encode_rtp(
            config
                .codecs
                .first()
                .map(|codec| codec.primary_payload_type)
                .unwrap_or(96),
            false,
            sequence,
            0,
            0,
            &extensions,
            &[],
            padding_bytes,
        ) else {
            return None;
        };
        Some(probe_packet(encoded, sequence, now))
    }

    fn push(&mut self, packet: QueuedRtp) -> Result<(), ()> {
        let queue = match packet.class {
            PacingClass::Audio => &mut self.audio,
            PacingClass::Retransmission => &mut self.retransmission,
            PacingClass::Video => &mut self.video,
            PacingClass::Padding => &mut self.padding,
        };
        if queue.len() >= QUEUE_CAPACITY {
            return Err(());
        }
        queue.push_back(packet);
        Ok(())
    }

    fn front_for(&self, probing: bool) -> Option<&QueuedRtp> {
        if probing {
            self.probe_starter
                .as_ref()
                .or_else(|| self.audio.front())
                .or_else(|| self.video.front())
                .or_else(|| self.retransmission.front())
                .or_else(|| self.padding.front())
        } else {
            self.audio
                .front()
                .or_else(|| self.retransmission.front())
                .or_else(|| self.video.front())
                .or_else(|| self.padding.front())
        }
    }

    fn pop_front_for(&mut self, probing: bool) -> Option<QueuedRtp> {
        if probing {
            self.probe_starter
                .take()
                .or_else(|| self.audio.pop_front())
                .or_else(|| self.video.pop_front())
                .or_else(|| self.retransmission.pop_front())
                .or_else(|| self.padding.pop_front())
        } else {
            self.audio
                .pop_front()
                .or_else(|| self.retransmission.pop_front())
                .or_else(|| self.video.pop_front())
                .or_else(|| self.padding.pop_front())
        }
    }
}

fn probe_packet(encoded: EncodedRtp, sequence: u64, now: Instant) -> QueuedRtp {
    QueuedRtp {
        bytes: encoded.bytes,
        extended_sequence: sequence,
        twcc_offset: encoded.twcc_offset,
        class: PacingClass::Padding,
        probe_eligible: true,
        lifecycle: Some(EgressLifecycle {
            ingress_at: now,
            admitted_at: now,
            eligible_at: now,
        }),
    }
}

fn media_extensions(
    config: &EgressSlotConfig,
    packet: &ForwardAdmission<'_>,
) -> Vec<(u8, Vec<u8>)> {
    let mut extensions = base_extensions(config);
    if let (Some(id), Some(value)) = (
        config.absolute_capture_time_extension,
        packet.absolute_capture_time,
    ) {
        extensions.push((id, value.to_vec()));
    }
    if let (Some(id), Some(value)) = (config.audio_level_extension, packet.audio_level) {
        extensions.push((id, vec![value.unsigned_abs().min(127)]));
    }
    if let (Some(id), Some(value)) = (
        config.dependency_descriptor_extension,
        packet.dependency_descriptor,
    ) {
        extensions.push((id, value.to_vec()));
    }
    extensions
}

fn base_extensions(config: &EgressSlotConfig) -> Vec<(u8, Vec<u8>)> {
    let mut extensions = Vec::with_capacity(2);
    if let Some(id) = config.mid_extension {
        extensions.push((id, config.mid.to_vec()));
    }
    if let Some(id) = config.twcc_extension {
        extensions.push((id, vec![0, 0]));
    }
    extensions
}

struct EncodedRtp {
    bytes: Vec<u8>,
    twcc_offset: Option<usize>,
}

#[allow(
    clippy::too_many_arguments,
    reason = "RTP serialization consumes the fixed wire-header fields and payload"
)]
fn encode_rtp(
    payload_type: u8,
    marker: bool,
    sequence: u64,
    timestamp: u64,
    ssrc: u32,
    extensions: &[(u8, Vec<u8>)],
    payload: &[u8],
    padding_bytes: usize,
) -> Result<EncodedRtp, ()> {
    debug_assert!(payload_type < 128);
    debug_assert!(padding_bytes == 0 || payload.is_empty());
    if padding_bytes > usize::from(u8::MAX) {
        return Err(());
    }
    let one_byte = extensions
        .iter()
        .all(|(id, value)| (1..=14).contains(id) && (1..=16).contains(&value.len()));
    let mut encoded_extensions = Vec::new();
    let mut twcc_extension_offset = None;
    for (id, value) in extensions {
        if *id == 0 || value.is_empty() || value.len() > usize::from(u8::MAX) {
            return Err(());
        }
        if one_byte {
            let length = u8::try_from(value.len().saturating_sub(1)).map_err(|_| ())?;
            encoded_extensions.push((*id << 4) | length);
        } else {
            encoded_extensions.push(*id);
            encoded_extensions.push(u8::try_from(value.len()).map_err(|_| ())?);
        }
        let value_offset = encoded_extensions.len();
        if twcc_extension_offset.is_none() && value.len() == 2 && value.as_slice() == [0, 0] {
            twcc_extension_offset = Some(value_offset);
        }
        encoded_extensions.extend_from_slice(value);
    }
    while !encoded_extensions.len().is_multiple_of(4) {
        encoded_extensions.push(0);
    }
    let extension_words =
        u16::try_from(encoded_extensions.len().saturating_div(4)).map_err(|_| ())?;
    let extension_len = if extensions.is_empty() {
        0
    } else {
        encoded_extensions.len().saturating_add(4)
    };
    let capacity = 12usize
        .saturating_add(extension_len)
        .saturating_add(payload.len())
        .saturating_add(padding_bytes);
    let mut bytes = Vec::with_capacity(capacity);
    bytes.push(
        0x80 | if extensions.is_empty() { 0 } else { 0x10 }
            | if padding_bytes > 0 { 0x20 } else { 0 },
    );
    bytes.push(payload_type | if marker { 0x80 } else { 0 });
    bytes.extend_from_slice(&low_u16(sequence).to_be_bytes());
    bytes.extend_from_slice(&low_u32(timestamp).to_be_bytes());
    bytes.extend_from_slice(&ssrc.to_be_bytes());
    let twcc_offset = if extensions.is_empty() {
        None
    } else {
        bytes.extend_from_slice(&(if one_byte { 0xbedeu16 } else { 0x1000u16 }).to_be_bytes());
        bytes.extend_from_slice(&extension_words.to_be_bytes());
        let start = bytes.len();
        bytes.extend_from_slice(&encoded_extensions);
        twcc_extension_offset.map(|offset| start.saturating_add(offset))
    };
    bytes.extend_from_slice(payload);
    if padding_bytes > 0 {
        bytes.resize(bytes.len().saturating_add(padding_bytes), 0);
        let last = bytes.last_mut().ok_or(())?;
        *last = u8::try_from(padding_bytes).map_err(|_| ())?;
    }
    debug_assert_eq!(bytes.len(), capacity);
    Ok(EncodedRtp { bytes, twcc_offset })
}

pub(crate) fn write_transport_sequence(bytes: &mut [u8], offset: usize, sequence: u16) -> bool {
    let Some(destination) = bytes.get_mut(offset..offset.saturating_add(2)) else {
        debug_assert!(false, "a recorded TWCC offset stays within its RTP packet");
        return false;
    };
    destination.copy_from_slice(&sequence.to_be_bytes());
    true
}

fn low_u16(value: u64) -> u16 {
    u16::try_from(value & u64::from(u16::MAX)).unwrap_or(u16::MAX)
}

fn low_u32(value: u64) -> u32 {
    u32::try_from(value & u64::from(u32::MAX)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn config(rtx: bool) -> EgressSlotConfig {
        EgressSlotConfig {
            slot: EgressSlot::new(1),
            kind: MediaKind::Video,
            mid: Vec::from(*b"v").into_boxed_slice(),
            primary_ssrc: 7,
            rtx_ssrc: rtx.then_some(8),
            codecs: Box::new([EgressCodecConfig {
                name: Box::from("H264"),
                primary_payload_type: 96,
                rtx_payload_type: rtx.then_some(97),
            }]),
            mid_extension: Some(1),
            twcc_extension: Some(3),
            absolute_capture_time_extension: None,
            audio_level_extension: None,
            dependency_descriptor_extension: None,
        }
    }

    fn audio_config() -> EgressSlotConfig {
        EgressSlotConfig {
            slot: EgressSlot::new(1),
            kind: MediaKind::Audio,
            mid: Vec::from(*b"a").into_boxed_slice(),
            primary_ssrc: 9,
            rtx_ssrc: None,
            codecs: Box::new([EgressCodecConfig {
                name: Box::from("opus"),
                primary_payload_type: 111,
                rtx_payload_type: None,
            }]),
            mid_extension: Some(1),
            twcc_extension: Some(3),
            absolute_capture_time_extension: None,
            audio_level_extension: Some(2),
            dependency_descriptor_extension: None,
        }
    }

    fn admission(now: Instant, logical_sequence: u64) -> ForwardAdmission<'static> {
        ForwardAdmission {
            codec: "H264",
            logical_sequence,
            timestamp: logical_sequence.saturating_mul(3_000),
            marker: true,
            payload: &[0x61, 1, 2],
            absolute_capture_time: None,
            audio_level: None,
            dependency_descriptor: None,
            ingress_at: now,
            admitted_at: now,
        }
    }

    fn ssrc(bytes: &[u8]) -> u32 {
        u32::from_be_bytes(
            bytes
                .get(8..12)
                .expect("RTP SSRC")
                .try_into()
                .expect("SSRC"),
        )
    }

    fn rtp_payload(bytes: &[u8]) -> &[u8] {
        let mut offset = 12usize;
        if bytes.first().is_some_and(|first| first & 0x10 != 0) {
            let words = u16::from_be_bytes(
                bytes
                    .get(14..16)
                    .expect("extension words")
                    .try_into()
                    .expect("words"),
            );
            offset = 16usize.saturating_add(usize::from(words).saturating_mul(4));
        }
        let padding = if bytes.first().is_some_and(|first| first & 0x20 != 0) {
            usize::from(*bytes.last().expect("padding length"))
        } else {
            0
        };
        bytes
            .get(offset..bytes.len().saturating_sub(padding))
            .expect("RTP payload")
    }

    fn one_byte_extension(bytes: &[u8], target: u8) -> Option<&[u8]> {
        let words = u16::from_be_bytes(bytes.get(14..16)?.try_into().ok()?);
        let extensions = bytes.get(16..16usize.checked_add(usize::from(words).checked_mul(4)?)?)?;
        let mut offset = 0usize;
        while offset < extensions.len() {
            let header = *extensions.get(offset)?;
            offset = offset.checked_add(1)?;
            if header == 0 {
                continue;
            }
            let id = header >> 4;
            if id == 15 {
                return None;
            }
            let length = usize::from(header & 0x0f).checked_add(1)?;
            let value = extensions.get(offset..offset.checked_add(length)?)?;
            if id == target {
                return Some(value);
            }
            offset = offset.checked_add(length)?;
        }
        None
    }

    #[test]
    fn logical_mapping_is_stable_across_gaps_reordering_padding_and_wrap() {
        let mut map = LogicalSequenceMap {
            next_wire: u64::from(u16::MAX).saturating_sub(1),
            ..LogicalSequenceMap::default()
        };
        let first = map.map(10);
        let gap = map.map(12);
        let reordered = map.map(11);
        let padding = map.allocate_padding();
        let later = map.map(13);

        assert_eq!(map.map(10), first);
        assert_eq!(gap, first.saturating_add(2));
        assert_eq!(reordered, first.saturating_add(1));
        assert_ne!(padding, later);
        assert_eq!(low_u16(gap), 0);
        assert_eq!(map.used_wire.len(), 5);
    }

    #[test]
    fn rtp_encoder_supports_two_byte_extensions_and_locates_twcc() {
        let mut encoded = encode_rtp(
            96,
            true,
            u64::from(u16::MAX).saturating_add(1),
            u64::from(u32::MAX).saturating_add(1),
            7,
            &[(20, vec![1, 2, 3]), (3, vec![0, 0])],
            &[4, 5],
            0,
        )
        .expect("valid RTP");
        assert_eq!(&encoded.bytes[2..4], &[0, 0]);
        assert_eq!(&encoded.bytes[4..8], &[0, 0, 0, 0]);
        let offset = encoded.twcc_offset.expect("TWCC offset");
        assert!(write_transport_sequence(&mut encoded.bytes, offset, 42));
        assert_eq!(
            encoded.bytes.get(offset..offset + 2),
            Some([0, 42].as_slice())
        );
    }

    #[test]
    fn nack_uses_bounded_history_and_the_independent_rtx_namespace() {
        let now = Instant::now();
        let mut engine = EgressEngine::new(now, [config(true)]);
        engine
            .admit(EgressSlot::new(1), admission(now, 65_535))
            .expect("admission");
        let primary = engine.poll_ready(now).expect("primary");
        let primary_sequence = u16::from_be_bytes(
            primary
                .bytes
                .get(2..4)
                .expect("sequence")
                .try_into()
                .expect("sequence"),
        );
        engine.handle_nack(
            7,
            &[primary_sequence, primary_sequence.wrapping_sub(1)],
            now,
        );

        let retransmission = engine.poll_ready(now).expect("RTX");
        assert_eq!(ssrc(&retransmission.bytes), 8);
        assert_eq!(
            retransmission.bytes.get(1).map(|value| value & 0x7f),
            Some(97)
        );
        assert_eq!(
            rtp_payload(&retransmission.bytes).get(..2),
            Some(primary_sequence.to_be_bytes().as_slice())
        );
        assert!(engine.poll_ready(now).is_none());
    }

    #[test]
    fn rtx_preserves_media_extensions_required_to_recover_the_frame() {
        let now = Instant::now();
        let mut slot = config(true);
        slot.absolute_capture_time_extension = Some(4);
        slot.dependency_descriptor_extension = Some(5);
        let capture = [1, 2, 3, 4, 5, 6, 7, 8];
        let dependency = [0xc0, 9, 10];
        let mut engine = EgressEngine::new(now, [slot]);
        let mut packet = admission(now, 7);
        packet.absolute_capture_time = Some(&capture);
        packet.dependency_descriptor = Some(&dependency);
        engine.admit(EgressSlot::new(1), packet).expect("media");
        let primary = engine.poll_ready(now).expect("primary");
        let sequence = u16::from_be_bytes(primary.bytes[2..4].try_into().expect("sequence"));

        engine.handle_nack(7, &[sequence], now);
        let retransmission = engine.poll_ready(now).expect("RTX");

        assert_eq!(
            one_byte_extension(&retransmission.bytes, 4),
            Some(capture.as_slice())
        );
        assert_eq!(
            one_byte_extension(&retransmission.bytes, 5),
            Some(dependency.as_slice())
        );
    }

    #[test]
    fn probe_fallback_requires_media_history_before_using_media_namespaces() {
        let now = Instant::now();
        let mut internal = EgressEngine::new(now, [config(false)]);
        internal
            .pacer
            .start_probe(now, 1, 1_000_000, 1, Duration::ZERO);
        internal.ensure_probe_fallback(now);
        let packet = internal.poll_ready(now).expect("internal padding");
        assert_eq!(ssrc(&packet.bytes), 0);
        assert_ne!(packet.bytes[0] & 0x20, 0);
        assert!(packet.twcc_offset.is_some());

        let mut primary = EgressEngine::new(now, [config(false)]);
        primary
            .admit(EgressSlot::new(1), admission(now, 1))
            .expect("media");
        let _ = primary.poll_ready(now).expect("media departure");
        primary
            .pacer
            .start_probe(now, 2, 1_000_000, 1, Duration::ZERO);
        primary.ensure_probe_fallback(now);
        assert_eq!(
            ssrc(&primary.poll_ready(now).expect("primary padding").bytes),
            7
        );

        let mut rtx = EgressEngine::new(now, [config(true)]);
        rtx.pacer.start_probe(now, 3, 1_000_000, 1, Duration::ZERO);
        rtx.ensure_probe_fallback(now);
        assert_eq!(
            ssrc(&rtx.poll_ready(now).expect("internal padding").bytes),
            0
        );

        let mut cached_rtx = EgressEngine::new(now, [config(true)]);
        cached_rtx
            .admit(EgressSlot::new(1), admission(now, 1))
            .expect("media");
        let _ = cached_rtx.poll_ready(now).expect("media departure");
        cached_rtx
            .pacer
            .start_probe(now, 4, 1_000_000, 1, Duration::ZERO);
        cached_rtx.ensure_probe_fallback(now);
        assert_eq!(
            ssrc(&cached_rtx.poll_ready(now).expect("payload RTX").bytes),
            8
        );
    }

    #[test]
    fn probe_starter_is_small_and_does_not_overtake_primary_media() {
        let now = Instant::now();
        let mut rtx = EgressEngine::new(now, [config(true)]);
        rtx.admit(EgressSlot::new(1), admission(now, 1))
            .expect("queued media");
        let starter = rtx.probe_starter_packet(now).expect("RTX starter");
        assert_eq!(ssrc(&starter.bytes), 8);
        assert!(rtp_payload(&starter.bytes).is_empty());
        assert_eq!(starter.bytes.last(), Some(&1));

        let mut queued_primary = EgressEngine::new(now, [config(false)]);
        queued_primary
            .admit(EgressSlot::new(1), admission(now, 1))
            .expect("queued media");
        let starter = queued_primary
            .probe_starter_packet(now)
            .expect("internal starter");
        assert_eq!(ssrc(&starter.bytes), 0);

        let mut idle_primary = EgressEngine::new(now, [config(false)]);
        idle_primary
            .admit(EgressSlot::new(1), admission(now, 1))
            .expect("media");
        let _ = idle_primary.poll_ready(now).expect("media departure");
        let starter = idle_primary
            .probe_starter_packet(now)
            .expect("primary starter");
        assert_eq!(ssrc(&starter.bytes), 7);
        assert!(rtp_payload(&starter.bytes).is_empty());
    }

    #[test]
    fn useful_media_precedes_recovery_during_a_probe() {
        let now = Instant::now();
        let mut engine = EgressEngine::new(now, [config(true)]);
        engine
            .admit(EgressSlot::new(1), admission(now, 1))
            .expect("first media");
        let first = engine.poll_ready(now).expect("first media");
        let sequence = u16::from_be_bytes(
            first
                .bytes
                .get(2..4)
                .expect("sequence")
                .try_into()
                .expect("sequence"),
        );
        engine.handle_nack(7, &[sequence], now);
        engine
            .admit(EgressSlot::new(1), admission(now, 2))
            .expect("fresh media");
        engine
            .pacer
            .start_probe(now, 4, 1_000_000, 1, Duration::ZERO);

        assert_eq!(ssrc(&engine.poll_ready(now).expect("fresh media").bytes), 7);
        assert_eq!(ssrc(&engine.poll_ready(now).expect("recovery").bytes), 8);
    }

    #[test]
    fn probe_deadline_matches_probe_admission_for_audio() {
        let now = Instant::now();
        let payload = [7u8; 1_600];
        let mut engine = EgressEngine::new(now, [audio_config()]);
        engine.pacer.start_probe(
            now,
            8,
            crate::DEFAULT_INITIAL_BITRATE_BPS,
            3,
            Duration::from_millis(15),
        );
        for sequence in 0..2 {
            engine
                .admit(
                    EgressSlot::new(1),
                    ForwardAdmission {
                        codec: "opus",
                        logical_sequence: sequence,
                        timestamp: sequence.saturating_mul(960),
                        marker: true,
                        payload: &payload,
                        absolute_capture_time: None,
                        audio_level: Some(-30),
                        dependency_descriptor: None,
                        ingress_at: now,
                        admitted_at: now,
                    },
                )
                .expect("audio admission");
        }

        assert!(engine.poll_ready(now).is_some());
        let deadline = engine.next_ready(now).expect("second audio deadline");
        assert!(deadline > now);
        assert!(engine.poll_ready(now).is_none());
        assert!(engine.poll_ready(deadline).is_some());
    }
}
