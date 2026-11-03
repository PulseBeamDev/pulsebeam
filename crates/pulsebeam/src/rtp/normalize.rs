//! Turning raw RTP into the SFU's normalized representation.
//!
//! This is the work that must happen exactly once per node, wherever raw RTP
//! first arrives — from a local client's `Rtc` today, from another node's UDP
//! once inter-node forwarding lands. Keeping it in one place is what lets a
//! future ingress middleware present both sources as the same stream.
//!
//! Normalizing is the **only** stage allowed to mutate a packet. Measurement
//! reads the result and never writes to it, so the two can run on different
//! nodes without either duplicating the other's work.

use crate::media::{Mid, Rid};

use pulsebeam_core::dd::{
    DdReadError, DecodeTargetIndication, DependencyDescriptorReader, TemplateDependencyStructure,
    read_mandatory,
};

use crate::rtp::monitor::MAX_LADDER_TARGETS;

use crate::rtp::{RtpPacket, cache::PACKET_WINDOW_CAPACITY, cache::PacketWindow};

/// What normalizing a packet taught us about its stream.
///
/// These are the sender's *declarations*, not our measurements: they are
/// identical on every node that sees the stream, which is exactly why a
/// downstream shard must never re-derive them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StreamFacts {
    /// Decode targets the Dependency Descriptor structure advertises, present
    /// only once a scalable keyframe has taught the structure.
    pub decode_targets: Option<u8>,
    /// Output height for each known decode target. Zero means the structure
    /// cannot establish the target's spatial resolution.
    pub decode_target_heights: Option<[u16; MAX_LADDER_TARGETS]>,
    /// All decode targets use one spatial layer; the encoding's measured height
    /// can stand in when the DD structure omits explicit resolutions.
    pub temporal_only: Option<bool>,
}

fn decode_target_heights(structure: &TemplateDependencyStructure) -> [u16; MAX_LADDER_TARGETS] {
    let mut heights = [0; MAX_LADDER_TARGETS];
    for (dt, height) in heights.iter_mut().enumerate() {
        if dt >= usize::from(structure.decode_target_count) {
            break;
        }
        let mut known = true;
        for template in &structure.templates {
            if template
                .dtis
                .get(dt)
                .is_some_and(|dti| *dti != DecodeTargetIndication::NotPresent)
            {
                if let Some(resolution) =
                    structure.resolutions.get(usize::from(template.spatial_id))
                {
                    *height = (*height).max(resolution.height);
                } else if structure.spatial_layer_count() == 1 {
                    // Nonzero marks a real target; the allocator uses the
                    // encoding's measured height for a single spatial layer.
                    *height = 1;
                } else {
                    known = false;
                }
            }
        }
        if !known {
            *height = 0;
        }
    }
    heights
}

pub struct Normalization {
    pub first: Option<(RtpPacket, StreamFacts)>,
    pub remaining: Vec<(RtpPacket, StreamFacts)>,
    pub request_keyframe: bool,
}

/// Per-RTP-stream decoding state that normalization needs to carry between
/// packets.
#[derive(Debug)]
pub struct StreamNormalizer {
    mid: Mid,
    rid: Option<Rid>,
    /// Templates only arrive on keyframes and are referenced by later packets.
    dd: DependencyDescriptorReader,
    pending_dd: PacketWindow,
    cold_keyframe_requested: bool,
    cold_packets_since_request: usize,
    dd_errors: u64,
    /// The Video Layers Allocation simulcast-stream index this stream is sent
    /// on, learned from its own packets, so a VLA carried on any sibling's
    /// packet can address this stream.
    vla_index: Option<u8>,
    vla_height: Option<(u64, u32)>,
    highest_timestamp: u64,
    dd_height_epoch: Option<u64>,
}

impl StreamNormalizer {
    pub fn new(mid: Mid, rid: Option<Rid>) -> Self {
        Self {
            mid,
            rid,
            dd: DependencyDescriptorReader::new(),
            pending_dd: PacketWindow::new(),
            cold_keyframe_requested: false,
            cold_packets_since_request: 0,
            dd_errors: 0,
            vla_index: None,
            vla_height: None,
            highest_timestamp: 0,
            dd_height_epoch: None,
        }
    }

    pub fn vla_index(&self) -> Option<u8> {
        self.vla_index
    }

    pub fn observe_vla(&mut self, vla: &pulsebeam_core::vla::VideoLayersAllocation) {
        let Some(index) = self
            .vla_index
            .filter(|index| *index != vla.current_simulcast_stream_index)
        else {
            return;
        };
        if let Some(height) = crate::track::vla_stream_height_px(vla, usize::from(index))
            && self
                .vla_height
                .is_some_and(|(_, previous)| previous != height)
        {
            // A sibling's RTP clock cannot anchor this encoding's new dimensions.
            self.vla_height = Some((self.highest_timestamp.saturating_add(1), 0));
        }
    }

    /// Dependency-descriptor parse failures seen so far, for tests and logs.
    pub fn dd_errors(&self) -> u64 {
        self.dd_errors
    }

    /// Bring `pkt` into the SFU's internal form and report what it declared.
    pub fn normalize(&mut self, mut pkt: RtpPacket) -> Normalization {
        let carries_dd = pkt.ext_vals.raw_dependency_descriptor().is_some();
        let cold = self.dd.structure().is_none();
        let mut parse_error = None;
        let Some(facts) = self.normalize_ready(&mut pkt, &mut parse_error) else {
            let Some(error) = parse_error else {
                debug_assert!(false, "DD normalization failed without an error");
                return Normalization {
                    first: None,
                    remaining: Vec::new(),
                    request_keyframe: true,
                };
            };
            return match error {
                DdReadError::NoStructure if cold => {
                    let _ = self.pending_dd.push(pkt);
                    self.cold_packets_since_request =
                        self.cold_packets_since_request.saturating_add(1);
                    let periodic_retry = self.cold_packets_since_request >= PACKET_WINDOW_CAPACITY;
                    if periodic_retry {
                        self.pending_dd.clear();
                        self.cold_packets_since_request = 0;
                    }
                    let request_keyframe = if !self.cold_keyframe_requested || periodic_retry {
                        self.cold_keyframe_requested = true;
                        true
                    } else {
                        false
                    };
                    Normalization {
                        first: None,
                        remaining: Vec::new(),
                        request_keyframe,
                    }
                }
                error if cold => {
                    self.note_dd_error(&error);
                    self.pending_dd.clear();
                    Normalization {
                        first: None,
                        remaining: Vec::new(),
                        request_keyframe: true,
                    }
                }
                error => {
                    self.note_dd_error(&error);
                    Normalization {
                        first: Some((pkt, StreamFacts::default())),
                        remaining: Vec::new(),
                        request_keyframe: false,
                    }
                }
            };
        };
        if cold && carries_dd {
            self.cold_keyframe_requested = false;
            self.cold_packets_since_request = 0;
            self.release_cold_start(pkt, facts)
        } else {
            Normalization {
                first: Some((pkt, facts)),
                remaining: Vec::new(),
                request_keyframe: false,
            }
        }
    }

    fn learn_vla_index(&mut self, pkt: &RtpPacket) {
        if let Some(vla) = pkt.ext_vals.video_layers_allocation() {
            self.vla_index = Some(vla.current_simulcast_stream_index);
        }
    }

    #[allow(
        clippy::disallowed_types,
        reason = "parsed descriptor is anchored in an Arc<dyn Any> extension map entry, core-local; see rtp::mod"
    )]
    fn normalize_ready(
        &mut self,
        pkt: &mut RtpPacket,
        parse_error: &mut Option<DdReadError>,
    ) -> Option<StreamFacts> {
        self.learn_vla_index(pkt);
        let timestamp = pkt.rtp_ts.numer();
        self.highest_timestamp = self.highest_timestamp.max(timestamp);
        let own_height = pkt.ext_vals.video_layers_allocation().and_then(|vla| {
            crate::track::vla_stream_height_px(vla, usize::from(vla.current_simulcast_stream_index))
        });
        if let Some(height) = own_height
            && self.vla_height.is_none_or(|(epoch, _)| timestamp >= epoch)
        {
            self.vla_height = Some((timestamp, height));
        }
        // Older packets must not inherit dimensions from a later declaration.
        pkt.reported_height = own_height
            .or_else(|| {
                self.vla_height
                    .filter(|(epoch, _)| timestamp >= *epoch)
                    .map(|(_, height)| height)
            })
            .unwrap_or(0);
        let Some(raw) = pkt.ext_vals.raw_dependency_descriptor() else {
            return Some(StreamFacts::default());
        };
        let stale = self.dd_height_epoch.is_some_and(|epoch| timestamp < epoch)
            || self.vla_height.is_some_and(|(epoch, _)| timestamp < epoch);
        // An old attached structure may describe its packet, not replace the
        // current parser context or invalidate newer VLA dimensions.
        let parsed = if stale {
            DependencyDescriptorReader::new().read(&raw.0)
        } else {
            self.dd.read(&raw.0)
        };
        match parsed {
            Ok(dd) => {
                if !stale && dd.attached_structure.is_some() {
                    self.dd_height_epoch = Some(timestamp);
                }
                if let Some(resolution) = dd.resolution {
                    let height = u32::from(resolution.height);
                    pkt.reported_height = height;
                    if !stale
                        && self.vla_height.is_some_and(|(epoch, full)| {
                            timestamp >= epoch && full > 0 && full < height
                        })
                    {
                        self.vla_height = Some((self.highest_timestamp.saturating_add(1), 0));
                    }
                }
                // A resolved DD overrides codec hints, including opaque payloads
                // and structure refreshes on dependent frames.
                pkt.is_keyframe = dd.frame_dependencies.frame_diffs.is_empty();
                pkt.is_frame_start = dd.start_of_frame;
                pkt.ext_vals.set_descriptor_arc(std::sync::Arc::new(dd));
                if stale {
                    return Some(StreamFacts::default());
                }
                Some(StreamFacts {
                    decode_targets: self.dd.structure().map(|s| s.decode_target_count),
                    decode_target_heights: self.dd.structure().map(decode_target_heights),
                    temporal_only: self
                        .dd
                        .structure()
                        .map(|s| s.spatial_layer_count() == 1 && s.resolutions.is_empty()),
                })
            }
            Err(error) => {
                *parse_error = Some(error);
                None
            }
        }
    }

    fn release_cold_start(&mut self, packet: RtpPacket, facts: StreamFacts) -> Normalization {
        let frame_number = packet
            .ext_vals
            .raw_dependency_descriptor()
            .and_then(|raw| read_mandatory(&raw.0).ok())
            .map(|mandatory| mandatory.frame_number);
        let Some(frame_number) = frame_number else {
            self.pending_dd.clear();
            return Normalization {
                first: None,
                remaining: Vec::new(),
                request_keyframe: true,
            };
        };

        let mut packets = vec![(packet, facts)];
        for held in self.pending_dd.take_all_sorted() {
            let belongs_to_template = held
                .ext_vals
                .raw_dependency_descriptor()
                .and_then(|raw| read_mandatory(&raw.0).ok())
                .is_some_and(|mandatory| mandatory.frame_number == frame_number);
            if !belongs_to_template {
                continue;
            }
            let mut held = held;
            let mut parse_error = None;
            if let Some(facts) = self.normalize_ready(&mut held, &mut parse_error) {
                packets.push((held, facts));
            } else if let Some(error) = parse_error {
                self.note_dd_error(&error);
            } else {
                debug_assert!(false, "held DD packet failed without an error");
            }
        }
        packets.sort_unstable_by_key(|(packet, _)| *packet.seq_no);
        if packets.is_empty() {
            debug_assert!(false, "the template packet must be released");
            return Normalization {
                first: None,
                remaining: Vec::new(),
                request_keyframe: true,
            };
        }
        let first = packets.remove(0);
        Normalization {
            first: Some(first),
            remaining: packets,
            request_keyframe: false,
        }
    }

    fn note_dd_error(&mut self, error: &DdReadError) {
        self.dd_errors = self.dd_errors.saturating_add(1);
        if self.dd_errors.is_power_of_two() {
            tracing::warn!(
                mid = %self.mid,
                rid = ?self.rid,
                errors = self.dd_errors,
                %error,
                "dependency descriptor parse failed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_core::dd::RawDependencyDescriptor;
    use pulsebeam_core::dd::temporal::TemporalDdSource;

    fn packet(seq: u64, raw: RawDependencyDescriptor) -> RtpPacket {
        let mut packet = RtpPacket {
            seq_no: seq.into(),
            ..RtpPacket::default()
        };
        packet.ext_vals.set_raw_descriptor(raw);
        packet
    }

    #[test]
    fn decode_target_output_heights_follow_spatial_templates() {
        use pulsebeam_core::dd::{FrameDependencyTemplate, RenderResolution};
        let mut structure = TemplateDependencyStructure {
            decode_target_count: 3,
            ..TemplateDependencyStructure::default()
        };
        structure.resolutions.push(RenderResolution {
            width: 320,
            height: 180,
        });
        structure.resolutions.push(RenderResolution {
            width: 1280,
            height: 720,
        });
        for (spatial_id, dtis) in [(0, [1, 1, 1]), (1, [0, 0, 1])] {
            let mut template = FrameDependencyTemplate {
                spatial_id,
                ..FrameDependencyTemplate::default()
            };
            for dti in dtis {
                template.dtis.push(if dti == 0 {
                    DecodeTargetIndication::NotPresent
                } else {
                    DecodeTargetIndication::Required
                });
            }
            structure.templates.push(template);
        }
        assert_eq!(decode_target_heights(&structure), [180, 180, 720]);
        let mut writer = pulsebeam_core::dd::DependencyDescriptorWriter::new();
        let mut normalizer = StreamNormalizer::new(Mid::from("v0"), None);
        let mut high = None;
        for (template_id, height) in [(0, 180), (1, 720)] {
            let descriptor = pulsebeam_core::dd::DependencyDescriptor {
                start_of_frame: true,
                end_of_frame: true,
                template_id,
                frame_number: u16::from(template_id),
                frame_dependencies: structure
                    .templates
                    .get(usize::from(template_id))
                    .unwrap()
                    .clone(),
                attached_structure: Some(Box::new(structure.clone())),
                ..Default::default()
            };
            let mut bytes = [0; pulsebeam_core::dd::MAX_DD_LEN];
            let len = writer.write(&descriptor, &mut bytes).unwrap();
            let mut input = packet(
                u64::from(template_id),
                RawDependencyDescriptor(bytes.get(..len).unwrap().iter().copied().collect()),
            );
            input.rtp_ts =
                crate::rtp::MediaTime::new(u64::from(template_id), crate::rtp::VIDEO_FREQUENCY);
            if height == 720 {
                high = Some(input.clone());
            }
            assert_eq!(
                normalizer.normalize(input).first.unwrap().0.reported_height,
                height
            );
        }
        let mut latest = RtpPacket {
            rtp_ts: crate::rtp::MediaTime::new(300, crate::rtp::VIDEO_FREQUENCY),
            ..Default::default()
        };
        latest
            .ext_vals
            .set_video_layers_allocation(allocation(0, 180));
        assert_eq!(
            normalizer
                .normalize(latest)
                .first
                .unwrap()
                .0
                .reported_height,
            180
        );
        assert_eq!(
            normalizer
                .normalize(high.unwrap())
                .first
                .unwrap()
                .0
                .reported_height,
            720
        );
        let next = RtpPacket {
            rtp_ts: crate::rtp::MediaTime::new(400, crate::rtp::VIDEO_FREQUENCY),
            ..Default::default()
        };
        assert_eq!(
            normalizer.normalize(next).first.unwrap().0.reported_height,
            180
        );
        let mut delayed = structure.clone();
        delayed.resolutions[0].height = 360;
        for (timestamp, attached_structure, expected) in
            [(200, Some(Box::new(delayed)), 360), (500, None, 180)]
        {
            let descriptor = pulsebeam_core::dd::DependencyDescriptor {
                start_of_frame: true,
                end_of_frame: true,
                frame_number: u16::try_from(timestamp).unwrap(),
                frame_dependencies: structure.templates[0].clone(),
                attached_structure,
                ..Default::default()
            };
            let mut bytes = [0; pulsebeam_core::dd::MAX_DD_LEN];
            let len = writer.write(&descriptor, &mut bytes).unwrap();
            let mut input = packet(
                timestamp,
                RawDependencyDescriptor(bytes[..len].iter().copied().collect()),
            );
            input.rtp_ts = crate::rtp::MediaTime::new(timestamp, crate::rtp::VIDEO_FREQUENCY);
            assert_eq!(
                normalizer.normalize(input).first.unwrap().0.reported_height,
                expected
            );
        }
        structure.resolutions.pop();
        assert_eq!(decode_target_heights(&structure), [180, 180, 0]);
    }

    #[test]
    fn buffers_reordered_fragments_until_the_template_arrives() {
        let mut source = TemporalDdSource::new(1);
        let descriptors = source
            .next_frame(true, 3)
            .expect("keyframe descriptors encode");
        let mut normalizer = StreamNormalizer::new(Mid::from("v0"), None);

        let delayed_head = packet(10, descriptors[1].clone());
        let result = normalizer.normalize(delayed_head);
        assert!(result.first.is_none());
        assert!(result.request_keyframe);

        let result = normalizer.normalize(packet(11, descriptors[2].clone()));
        assert!(result.first.is_none());
        assert!(!result.request_keyframe);

        let result = normalizer.normalize(packet(9, descriptors[0].clone()));
        assert!(!result.request_keyframe);
        let packets: Vec<_> = result.first.into_iter().chain(result.remaining).collect();
        assert_eq!(packets.len(), 3, "the template releases held fragments");
        assert_eq!(
            packets
                .iter()
                .map(|(packet, _)| *packet.seq_no)
                .collect::<Vec<_>>(),
            vec![9, 10, 11]
        );
        assert!(packets[0].0.is_keyframe);
        let raw = source.next_frame(false, 1).unwrap().remove(0);
        let mut dependent = packet(12, raw);
        dependent.is_keyframe = true;
        let dependent = normalizer.normalize(dependent).first.unwrap().0;
        assert!(!dependent.is_keyframe, "usable DD overrides the codec hint");
        assert!(packets[0].0.is_frame_start);
        assert_eq!(packets[0].1.temporal_only, Some(true));
        assert_eq!(packets[0].1.decode_target_heights, Some([1, 0, 0]));

        let result = normalizer.normalize(packet(11, descriptors[2].clone()));
        assert!(result.first.is_some());
        assert!(result.remaining.is_empty());
        assert!(!result.request_keyframe);
    }

    #[test]
    fn retries_a_cold_keyframe_request_after_a_bounded_run() {
        let mut source = TemporalDdSource::new(1);
        let descriptors = source
            .next_frame(true, 2)
            .expect("keyframe descriptors encode");
        let mut normalizer = StreamNormalizer::new(Mid::from("v0"), None);

        assert!(
            normalizer
                .normalize(packet(1, descriptors[1].clone()))
                .request_keyframe
        );
        for seq in 2..PACKET_WINDOW_CAPACITY {
            assert!(
                !normalizer
                    .normalize(packet(seq as u64, descriptors[1].clone()))
                    .request_keyframe
            );
        }
        assert!(
            normalizer
                .normalize(packet(
                    PACKET_WINDOW_CAPACITY as u64 + 1,
                    descriptors[1].clone()
                ))
                .request_keyframe
        );

        let result = normalizer.normalize(packet(1000, descriptors[0].clone()));
        assert!(!result.request_keyframe);
        assert!(result.first.is_some());
    }

    fn allocation(current: u8, height: u32) -> pulsebeam_core::vla::VideoLayersAllocation {
        use pulsebeam_core::vla::{
            ResolutionAndFramerate, SimulcastStreamAllocation, SpatialLayerAllocation,
            TemporalLayerAllocation, VideoLayersAllocation,
        };
        VideoLayersAllocation {
            current_simulcast_stream_index: current,
            simulcast_streams: vec![
                SimulcastStreamAllocation {
                    spatial_layers: vec![SpatialLayerAllocation {
                        temporal_layers: vec![TemporalLayerAllocation {
                            cumulative_kbps: 50
                        }],
                        resolution_and_framerate: Some(ResolutionAndFramerate {
                            width: 1280,
                            height,
                            framerate: 30
                        }),
                    }]
                };
                2
            ],
        }
    }

    #[test]
    fn declared_heights_stay_with_packets_across_reordering_and_sibling_updates() {
        let vla = allocation;
        let push = |normalizer: &mut StreamNormalizer, timestamp, declaration| {
            let mut packet = RtpPacket {
                rtp_ts: crate::rtp::MediaTime::new(timestamp, crate::rtp::VIDEO_FREQUENCY),
                ..Default::default()
            };
            if let Some(declaration) = declaration {
                packet.ext_vals.set_video_layers_allocation(declaration);
            }
            normalizer.normalize(packet).first.unwrap().0
        };
        let mut normalizer = StreamNormalizer::new(Mid::from("v0"), None);
        assert_eq!(push(&mut normalizer, 0, None).reported_height, 0);
        let old = push(&mut normalizer, 100, Some(vla(0, 180)));
        assert_eq!(old.reported_height, 180);
        assert_eq!(push(&mut normalizer, 200, None).reported_height, 180);
        assert_eq!(
            push(&mut normalizer, 300, Some(vla(0, 720))).reported_height,
            720
        );
        assert_eq!(old.reported_height, 180);
        assert_eq!(push(&mut normalizer, 150, None).reported_height, 0);
        assert_eq!(
            push(&mut normalizer, 100, Some(vla(0, 180))).reported_height,
            180
        );
        assert_eq!(push(&mut normalizer, 400, None).reported_height, 720);
        normalizer.observe_vla(&vla(1, 180));
        assert_eq!(push(&mut normalizer, 450, None).reported_height, 0);
        assert_eq!(
            push(&mut normalizer, 300, Some(vla(0, 720))).reported_height,
            720
        );
        assert_eq!(push(&mut normalizer, 500, None).reported_height, 0);
        assert_eq!(
            push(&mut normalizer, 600, Some(vla(0, 180))).reported_height,
            180
        );
        assert_eq!(push(&mut normalizer, 700, None).reported_height, 180);
    }
}
