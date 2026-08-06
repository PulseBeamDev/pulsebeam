//! Intra-encoding frame selection.
//!
//! The [`Switcher`](crate::rtp::switcher::Switcher) chooses *which encoding* of a
//! track to forward; a `FrameSelector` chooses *which frames within that encoding*
//! to forward. With a scalable stream that is how the SFU sheds bitrate finely —
//! dropping temporal (and later spatial) layers frame by frame instead of dropping
//! a whole simulcast layer at once.
//!
//! Two implementations:
//!   * [`MarkerSelector`] — no scalability info; forward everything. This is the
//!     fallback for streams that do not carry a Dependency Descriptor.
//!   * [`DependencyDescriptorSelector`] — forward a frame iff it belongs to the
//!     currently targeted decode target, read from each packet's parsed
//!     Dependency Descriptor.

use pulsebeam_core::dd::DependencyDescriptor;

use crate::rtp::RtpPacket;

/// Per-packet forwarding decision within one encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameDecision {
    /// Forward this packet to the subscriber.
    Forward,
    /// Drop it as an intentional layer shed — the egress stream renumbers around
    /// it so the subscriber never sees the gap as loss.
    Drop,
}

/// Decides whether each packet of the active encoding is forwarded.
pub trait FrameSelector: std::fmt::Debug {
    fn decide(&mut self, pkt: &RtpPacket) -> FrameDecision;
}

/// Forward every packet. Used when the encoding carries no Dependency Descriptor,
/// so there is no layer structure to shed against — behaviour identical to the
/// pre-DD forwarder.
#[derive(Debug, Default, Clone, Copy)]
pub struct MarkerSelector;

impl FrameSelector for MarkerSelector {
    #[inline]
    fn decide(&mut self, _pkt: &RtpPacket) -> FrameDecision {
        FrameDecision::Forward
    }
}

/// The decode target a [`DependencyDescriptorSelector`] forwards toward.
///
/// `Full` forwards every frame (highest quality, the default); `Target(dt)` keeps
/// only frames that contribute to decode-target index `dt`, shedding the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeTargetSelection {
    Full,
    Target(usize),
}

impl Default for DecodeTargetSelection {
    fn default() -> Self {
        Self::Full
    }
}

/// Forwards frames belonging to the targeted decode target, dropping the rest.
///
/// For a temporally scalable stream the decode targets are nested (`dt0` = base
/// layer only, `dt1` = base + first temporal enhancement, …), so forwarding
/// exactly the frames a target contains is decodable by construction: a forwarded
/// frame never depends on a dropped one. Lowering or raising the target therefore
/// takes effect immediately — the finer, keyframe-free bitrate control DD exists
/// to provide.
///
/// A packet without a parsed descriptor is forwarded unchanged, so a stream that
/// only intermittently carries DD still plays.
#[derive(Debug, Default)]
pub struct DependencyDescriptorSelector {
    target: DecodeTargetSelection,
}

impl DependencyDescriptorSelector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_target(target: DecodeTargetSelection) -> Self {
        Self { target }
    }

    /// Change the decode target this selector forwards toward.
    pub fn set_target(&mut self, target: DecodeTargetSelection) {
        self.target = target;
    }

    pub fn target(&self) -> DecodeTargetSelection {
        self.target
    }

    fn keep(&self, dd: &DependencyDescriptor) -> bool {
        match self.target {
            DecodeTargetSelection::Full => true,
            DecodeTargetSelection::Target(dt) => dd.is_in_decode_target(dt),
        }
    }
}

impl FrameSelector for DependencyDescriptorSelector {
    fn decide(&mut self, pkt: &RtpPacket) -> FrameDecision {
        match pkt.ext_vals.user_values.get::<DependencyDescriptor>() {
            Some(dd) if !self.keep(dd) => FrameDecision::Drop,
            // In target, or no descriptor to reason about: forward.
            _ => FrameDecision::Forward,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use pulsebeam_core::dd::{
        DecodeTargetIndication, DependencyDescriptor, FrameDependencyTemplate,
    };
    use std::sync::Arc;

    /// A packet whose frame declares the given per-decode-target indications.
    fn pkt_with_dtis(dtis: &[DecodeTargetIndication]) -> RtpPacket {
        let mut dd = DependencyDescriptor::default();
        dd.frame_dependencies = FrameDependencyTemplate {
            dtis: dtis.iter().copied().collect(),
            ..Default::default()
        };
        let mut pkt = RtpPacket::default();
        pkt.ext_vals.user_values.set_arc(Arc::new(dd));
        pkt
    }

    fn plain_pkt() -> RtpPacket {
        RtpPacket::default()
    }

    use DecodeTargetIndication::{Discardable, NotPresent, Required, Switch};

    #[test]
    fn marker_selector_forwards_everything() {
        let mut sel = MarkerSelector;
        assert_eq!(sel.decide(&plain_pkt()), FrameDecision::Forward);
        assert_eq!(
            sel.decide(&pkt_with_dtis(&[NotPresent])),
            FrameDecision::Forward,
            "the marker fallback never sheds layers"
        );
    }

    #[test]
    fn full_target_forwards_every_frame() {
        let mut sel = DependencyDescriptorSelector::new();
        // dt0 base frame and a dt2-only enhancement frame both pass at Full.
        assert_eq!(
            sel.decide(&pkt_with_dtis(&[Required, Required, Required])),
            FrameDecision::Forward
        );
        assert_eq!(
            sel.decide(&pkt_with_dtis(&[NotPresent, NotPresent, Discardable])),
            FrameDecision::Forward
        );
    }

    #[test]
    fn lower_target_sheds_higher_temporal_frames() {
        // Three temporal decode targets. A base (T0) frame is Required for all; a
        // T1 frame is NotPresent for dt0; a T2 frame is NotPresent for dt0 and dt1.
        let base = pkt_with_dtis(&[Required, Required, Required]);
        let t1 = pkt_with_dtis(&[NotPresent, Switch, Required]);
        let t2 = pkt_with_dtis(&[NotPresent, NotPresent, Discardable]);

        let mut sel = DependencyDescriptorSelector::with_target(DecodeTargetSelection::Target(0));
        assert_eq!(sel.decide(&base), FrameDecision::Forward, "base always kept");
        assert_eq!(sel.decide(&t1), FrameDecision::Drop, "T1 shed at dt0");
        assert_eq!(sel.decide(&t2), FrameDecision::Drop, "T2 shed at dt0");

        sel.set_target(DecodeTargetSelection::Target(1));
        assert_eq!(sel.decide(&base), FrameDecision::Forward);
        assert_eq!(sel.decide(&t1), FrameDecision::Forward, "T1 kept at dt1");
        assert_eq!(sel.decide(&t2), FrameDecision::Drop, "T2 still shed at dt1");

        sel.set_target(DecodeTargetSelection::Target(2));
        assert_eq!(sel.decide(&t2), FrameDecision::Forward, "T2 kept at dt2");
    }

    #[test]
    fn a_packet_without_a_descriptor_is_forwarded() {
        let mut sel = DependencyDescriptorSelector::with_target(DecodeTargetSelection::Target(0));
        assert_eq!(
            sel.decide(&plain_pkt()),
            FrameDecision::Forward,
            "cannot reason without DD, so never drop"
        );
    }

    #[test]
    fn target_beyond_declared_targets_drops_uncovered_frames() {
        // Frame only declares two decode targets; asking for dt5 means "not in it".
        let mut sel = DependencyDescriptorSelector::with_target(DecodeTargetSelection::Target(5));
        assert_eq!(
            sel.decide(&pkt_with_dtis(&[Required, Required])),
            FrameDecision::Drop
        );
    }
}
