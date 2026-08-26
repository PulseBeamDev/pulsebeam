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

use pulsebeam_core::dd::{DdReadError, DependencyDescriptorReader, read_mandatory};

use crate::rtp::{EncodingId as Rid, MediaSectionId as Mid, RtpPacket, cache::PacketWindow};

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
    dd_errors: u64,
    /// The Video Layers Allocation simulcast-stream index this stream is sent
    /// on, learned from its own packets, so a VLA carried on any sibling's
    /// packet can address this stream.
    vla_index: Option<u8>,
}

impl StreamNormalizer {
    pub fn new(mid: Mid, rid: Option<Rid>) -> Self {
        Self {
            mid,
            rid,
            dd: DependencyDescriptorReader::new(),
            pending_dd: PacketWindow::new(),
            dd_errors: 0,
            vla_index: None,
        }
    }

    pub fn vla_index(&self) -> Option<u8> {
        self.vla_index
    }

    /// Dependency-descriptor parse failures seen so far, for tests and logs.
    pub fn dd_errors(&self) -> u64 {
        self.dd_errors
    }

    /// Bring `pkt` into the SFU's internal form and report what it declared.
    pub fn normalize(&mut self, mut pkt: RtpPacket) -> Normalization {
        let carries_dd = pkt.extensions.raw_dependency_descriptor.is_some();
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
                    let request_keyframe = self.pending_dd.push(pkt).is_some();
                    if request_keyframe {
                        self.pending_dd.clear();
                    }
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
        if let Some(vla) = &pkt.extensions.video_layers_allocation {
            self.vla_index = Some(vla.current_simulcast_stream_index);
        }
    }

    fn normalize_ready(
        &mut self,
        pkt: &mut RtpPacket,
        parse_error: &mut Option<DdReadError>,
    ) -> Option<StreamFacts> {
        self.learn_vla_index(pkt);
        let Some(raw) = pkt.extensions.raw_dependency_descriptor.as_ref() else {
            return Some(StreamFacts::default());
        };
        match self.dd.read(&raw.0) {
            Ok(dd) => {
                // Under SFrame/E2EE the media payload is opaque, so the H.264
                // IDR probe sees nothing. The Dependency
                // Descriptor rides in the clear and carries the template
                // structure on a keyframe's first packet, so it is the authoritative
                // keyframe signal whenever present.
                pkt.is_keyframe |= dd.attached_structure.is_some();
                pkt.is_frame_start = dd.start_of_frame;
                pkt.extensions.dependency_descriptor = Some(dd);
                Some(StreamFacts {
                    decode_targets: self.dd.structure().map(|s| s.decode_target_count),
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
            .extensions
            .raw_dependency_descriptor
            .as_ref()
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
                .extensions
                .raw_dependency_descriptor
                .as_ref()
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
        packet.extensions.raw_dependency_descriptor = Some(raw);
        packet
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
        assert!(!result.request_keyframe);

        let result = normalizer.normalize(packet(9, descriptors[0].clone()));
        assert!(!result.request_keyframe);
        let packets: Vec<_> = result.first.into_iter().chain(result.remaining).collect();
        assert_eq!(packets.len(), 2, "the template releases held fragments");
        assert_eq!(
            packets
                .iter()
                .map(|(packet, _)| *packet.seq_no)
                .collect::<Vec<_>>(),
            vec![9, 10]
        );
        assert!(packets[0].0.is_keyframe);
        assert!(packets[0].0.is_frame_start);

        let result = normalizer.normalize(packet(11, descriptors[2].clone()));
        assert!(result.first.is_some());
        assert!(result.remaining.is_empty());
        assert!(!result.request_keyframe);
    }
}
