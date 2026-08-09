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
//! Shared-state exception, imposed by str0m: the parsed descriptor goes
//! back into an extension map that stores `Arc<dyn Any>`. Core-local; see
//! `rtp` for the full note.
#![allow(clippy::disallowed_types)]

use str0m::media::{Mid, Rid};
use str0m::rtp::vla::VideoLayersAllocation;

use pulsebeam_core::dd::{DependencyDescriptorReader, RawDependencyDescriptor};

use crate::rtp::RtpPacket;

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

/// Per-RTP-stream decoding state that normalization needs to carry between
/// packets.
#[derive(Debug)]
pub struct StreamNormalizer {
    mid: Mid,
    rid: Option<Rid>,
    /// Templates only arrive on keyframes and are referenced by later packets.
    dd: DependencyDescriptorReader,
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
    pub fn normalize(&mut self, pkt: &mut RtpPacket) -> StreamFacts {
        self.learn_vla_index(pkt);
        StreamFacts {
            decode_targets: self.read_dependency_descriptor(pkt),
        }
    }

    fn learn_vla_index(&mut self, pkt: &RtpPacket) {
        if let Some(vla) = pkt.ext_vals.user_values.get::<VideoLayersAllocation>() {
            self.vla_index = Some(vla.current_simulcast_stream_index);
        }
    }

    fn read_dependency_descriptor(&mut self, pkt: &mut RtpPacket) -> Option<u8> {
        let raw = pkt.ext_vals.user_values.get::<RawDependencyDescriptor>()?;
        match self.dd.read(&raw.0) {
            Ok(dd) => {
                // Under SFrame/E2EE the media payload is opaque, so the H.264
                // IDR probe in `from_str0m` sees nothing. The Dependency
                // Descriptor rides in the clear and carries the template
                // structure on every keyframe, so it is the authoritative
                // keyframe signal whenever present.
                pkt.is_keyframe = dd.attached_structure.is_some();
                pkt.ext_vals.user_values.set_arc(std::sync::Arc::new(dd));
                self.dd.structure().map(|s| s.decode_target_count)
            }
            Err(err) => {
                self.dd_errors += 1;
                if self.dd_errors.is_power_of_two() {
                    tracing::warn!(
                        mid = %self.mid,
                        rid = ?self.rid,
                        errors = self.dd_errors,
                        %err,
                        "dependency descriptor parse failed"
                    );
                }
                None
            }
        }
    }
}
