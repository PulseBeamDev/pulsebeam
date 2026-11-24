//! PRODUCTION WIRE CONTRACT — EDIT WITH EXTREME CARE. Changes can break deployed clients, mixed-version nodes, and production traffic. Require compatibility review and wire-vector tests.
//!
//! [FabricPacket v1 §5.3 and Appendix A][rfc]: use authenticated RTP v2 only.
//! RTC checks CSRC bounds, RFC 8285 framing and trailing padding. RTX carries a
//! big-endian u16 OSN before the original codec bytes. Endpoint extension IDs
//! are translated by the installed receive extmap, never by numeric ID alone.
//! The v1 codec owns canonical packing, value validation and the 1152-byte cap.
//!
//! [rfc]: https://github.com/PulseBeamDev/pulsebeam/issues/275

use std::{num::NonZeroU16, time::Instant};

use arrayvec::ArrayVec;
use pulsebeam_core::fabric::{
    Extension, ExtensionKind, ExtensionWidth, Profile,
    media_source::{
        Admission, Anchors, Input, Limits, MediaCopy, MediaIdentity, MediaSource, Position,
        Rejected,
    },
};
use pulsebeam_rtc::{
    AuthenticatedRtp, CodecProfile, RtpHeader, RtpReceiveCodec, RtpReceiveExtension,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtensionForm {
    OneByte,
    TwoByte,
    Mixed,
}

impl ExtensionForm {
    fn accepts(self, profile: Option<u16>) -> bool {
        match profile {
            None => true,
            Some(0xbede) => self != Self::TwoByte,
            Some(0x1000..=0x100f) => self != Self::OneByte,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepairBinding {
    pub ssrc: u32,
    pub payload_type: u8,
}

/// Exact endpoint facts approved by the publication owner for one source
/// incarnation. Callers also qualify packetization, any codec limits and each
/// selected hint for that producer. A new binding requires a new shadow.
pub struct EndpointBinding {
    pub mid: Box<str>,
    pub rid: Option<Box<str>>,
    pub primary_ssrc: u32,
    pub codec: RtpReceiveCodec,
    pub repair: Option<RepairBinding>,
    pub receive_extensions: Box<[RtpReceiveExtension]>,
    pub extension_form: ExtensionForm,
    pub allow_mixed_extensions: bool,
    pub qualified_extensions: ArrayVec<ExtensionKind, 8>,
    /// ACT can only survive if the binding names its capture clock. The first
    /// CSRC takes precedence over the primary SSRC, including on RTX.
    pub capture_clock_ssrc: Option<u32>,
    /// False when stripping a contributing-source list would lose required attribution.
    pub allow_csrc_stripping: bool,
}

impl EndpointBinding {
    fn matches_repair(&self, payload_type: Option<u8>, declared_ssrc: Option<u32>) -> bool {
        payload_type == self.repair.map(|repair| repair.payload_type)
            && declared_ssrc.is_none_or(|ssrc| Some(ssrc) == self.repair.map(|repair| repair.ssrc))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindError {
    Encoding,
    Codec,
    Extensions,
    Limits,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DropReason {
    Binding,
    ExtensionForm,
    DuplicateExtension,
    MissingDependencyDescriptor,
    InvalidDependencyDescriptor(pulsebeam_core::dd::DdReadError),
    MixerAttribution,
    EmptyMedia,
    PaddingWithDependencyDescriptor,
    RepairPayload,
    Admission(Rejected),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Observation {
    Padding,
    Media {
        sequence: u64,
        first: bool,
        wan_plaintext_bytes: usize,
    },
}

/// Disabled-by-default diagnostic path. This owns only shadow identity state,
/// never subscriber queues or the RTC receive path. Its caller must scope it to
/// one authorized transport and discard it when the binding retires.
pub struct RtpIngressShadow {
    binding: EndpointBinding,
    selected: ArrayVec<(u8, ExtensionKind), 8>,
    profile: Profile,
    observer: Option<MediaSource>,
    requires_descriptor: bool,
}

impl RtpIngressShadow {
    pub(crate) fn bind_local_endpoint(
        profile: Profile,
        binding: pulsebeam_rtc::RtpReceiveBinding<'_>,
    ) -> Option<Self> {
        Self::bind_whole(
            profile,
            binding,
            if binding.allow_mixed_extensions {
                ExtensionForm::Mixed
            } else {
                ExtensionForm::OneByte
            },
            if profile == Profile::Opus
                && binding.receive_extensions.iter().any(|mapping| {
                    mapping.uri.as_ref() == AUDIO_LEVEL_URI
                        && matches!(mapping.attributes.as_deref(), None | Some("vad=on"))
                })
            {
                &[ExtensionKind::AudioLevel]
            } else if profile == Profile::Av1
                && binding.receive_extensions.iter().any(|mapping| {
                    mapping.uri.as_ref() == pulsebeam_core::dd::URI && mapping.attributes.is_none()
                })
            {
                &[ExtensionKind::DependencyDescriptor]
            } else {
                &[]
            },
            Anchors::default(),
            Limits {
                positions: std::num::NonZeroU16::new(64)?,
                max_age: std::time::Duration::from_millis(120),
                max_idle: std::time::Duration::from_secs(2),
            },
        )
        .ok()
    }

    /// Bind an encoding that must not deliver media without negotiated DD.
    pub(crate) fn bind_dd_encoding(binding: pulsebeam_rtc::RtpReceiveBinding<'_>) -> Option<Self> {
        let form = if binding.allow_mixed_extensions {
            ExtensionForm::Mixed
        } else {
            ExtensionForm::OneByte
        };
        let mut ingress = Self::bind_whole(
            Profile::H264,
            binding,
            form,
            &[ExtensionKind::DependencyDescriptor],
            Anchors::default(),
            Limits {
                positions: std::num::NonZeroU16::new(64)?,
                max_age: std::time::Duration::from_millis(120),
                max_idle: std::time::Duration::from_secs(2),
            },
        )
        .ok()?;
        ingress.require_descriptor().then_some(ingress)
    }

    fn require_descriptor(&mut self) -> bool {
        self.requires_descriptor = self
            .selected
            .iter()
            .any(|(_, kind)| *kind == ExtensionKind::DependencyDescriptor);
        self.requires_descriptor
    }

    /// Bind an approved core-only whole source to RTC's negotiated facts. The
    /// owner chooses the profile and allowed extension form before forwarding
    /// and must separately authorize the publication for this MID and SSRC.
    pub(crate) fn bind_whole(
        profile: Profile,
        binding: pulsebeam_rtc::RtpReceiveBinding<'_>,
        extension_form: ExtensionForm,
        qualified_extensions: &[ExtensionKind],
        anchors: Anchors,
        limits: Limits,
    ) -> Result<Self, BindError> {
        let repair = match (binding.repair_payload_type, binding.declared_repair_ssrc) {
            (Some(payload_type), Some(ssrc)) => Some(RepairBinding { ssrc, payload_type }),
            (None, None) => None,
            // An RTX PT alone cannot identify the physical repair stream.
            _ => return Err(BindError::Encoding),
        };
        let mut selected = ArrayVec::new();
        for &kind in qualified_extensions {
            selected.try_push(kind).map_err(|_| BindError::Extensions)?;
        }
        let ingress = Self::new(
            EndpointBinding {
                mid: binding.mid.into(),
                rid: binding.rid.map(Into::into),
                primary_ssrc: binding.primary_ssrc,
                codec: binding.primary_codec.clone(),
                repair,
                receive_extensions: binding.receive_extensions.into(),
                extension_form,
                allow_mixed_extensions: binding.allow_mixed_extensions,
                qualified_extensions: selected,
                capture_clock_ssrc: None,
                allow_csrc_stripping: false,
            },
            anchors,
            limits,
        )?;
        if ingress.profile != profile {
            return Err(BindError::Codec);
        }
        Ok(ingress)
    }

    pub fn new(
        binding: EndpointBinding,
        anchors: Anchors,
        limits: Limits,
    ) -> Result<Self, BindError> {
        if binding.mid.is_empty()
            || binding.primary_ssrc == 0
            || binding.codec.payload_type > 127
            || binding.repair.is_some_and(|repair| {
                repair.ssrc == 0
                    || repair.ssrc == binding.primary_ssrc
                    || repair.payload_type > 127
                    || repair.payload_type == binding.codec.payload_type
            })
        {
            return Err(BindError::Encoding);
        }
        let profile = match (
            binding.codec.name.as_ref(),
            binding.codec.profile,
            binding.codec.channels,
        ) {
            ("opus", Some(CodecProfile::Opus), Some(2)) => Profile::Opus,
            (
                "h264",
                Some(CodecProfile::H264 {
                    packetization_mode: 1,
                    ..
                }),
                None,
            ) => Profile::H264,
            (
                "av1",
                Some(CodecProfile::Av1 {
                    profile: 0..=2,
                    level_idx: 0..=23,
                    tier: 0..=1,
                }),
                None,
            ) => Profile::Av1,
            _ => return Err(BindError::Codec),
        };
        if binding.codec.clock_rate != profile.clock_rate() {
            return Err(BindError::Codec);
        }
        if binding.extension_form == ExtensionForm::Mixed && !binding.allow_mixed_extensions {
            return Err(BindError::Extensions);
        }
        let mut ids = [false; 256];
        let mut selected = ArrayVec::<_, 8>::new();
        for extension in &binding.receive_extensions {
            let id = extension.id;
            let used = ids.get_mut(usize::from(id)).ok_or(BindError::Extensions)?;
            if *used || id == 0 || (binding.extension_form == ExtensionForm::OneByte && id > 14) {
                return Err(BindError::Extensions);
            }
            *used = true;
            let Some(kind) = (!extension.is_encrypted())
                .then(|| extension_kind(extension.uri.as_ref()))
                .flatten()
                .filter(|kind| {
                    kind.is_permitted_on(profile) && binding.qualified_extensions.contains(kind)
                })
            else {
                continue;
            };
            if selected.iter().any(|(_, old)| *old == kind)
                || (kind == ExtensionKind::AudioLevel
                    && !matches!(
                        extension.attributes.as_deref(),
                        None | Some("vad=on" | "vad=off")
                    ))
            {
                return Err(BindError::Extensions);
            }
            selected
                .try_push((id, kind))
                .map_err(|_| BindError::Extensions)?;
        }
        let source = MediaSource::new(profile, anchors, limits).ok_or(BindError::Limits)?;
        Ok(Self {
            binding,
            selected,
            profile,
            observer: Some(source),
            requires_descriptor: false,
        })
    }

    pub(crate) fn emission(&self) -> Option<crate::route::fabric::local::Emission> {
        let extensions: ArrayVec<_, 8> = self.selected.iter().map(|(_, kind)| *kind).collect();
        crate::route::fabric::local::Emission::new(self.profile, &extensions)
    }

    pub(crate) fn source_encoding(&self) -> Option<crate::route::fabric::RtpSourceEncoding> {
        Some(crate::route::fabric::RtpSourceEncoding {
            primary: self.binding.primary_ssrc.into(),
            rid: self
                .binding
                .rid
                .as_deref()
                .map(crate::media::Rid::new)
                .transpose()
                .ok()?,
            payload_type: self.binding.codec.payload_type.into(),
        })
    }

    pub(crate) fn mid(&self) -> Option<crate::media::Mid> {
        crate::media::Mid::new(&self.binding.mid).ok()
    }

    pub(crate) const fn primary_ssrc(&self) -> u32 {
        self.binding.primary_ssrc
    }

    pub fn observe(
        &mut self,
        now: Instant,
        packet: AuthenticatedRtp<'_>,
    ) -> Result<Observation, DropReason> {
        Ok(match self.admit(now, packet)? {
            Admission::Padding => Observation::Padding,
            Admission::Media {
                sequence,
                first,
                packet,
                ..
            } => Observation::Media {
                sequence,
                first,
                wan_plaintext_bytes: packet.wan_plaintext_len(),
            },
        })
    }

    fn admit(
        &mut self,
        now: Instant,
        packet: AuthenticatedRtp<'_>,
    ) -> Result<Admission<'_>, DropReason> {
        let arrival = packet.arrival;
        let Some((sequence, input)) = self.normalize_packet(packet)? else {
            return Ok(Admission::Padding);
        };
        self.observer
            .as_mut()
            .ok_or(DropReason::Binding)?
            .admit(now, arrival, sequence, input.as_input())
            .map_err(DropReason::Admission)
    }

    pub(crate) fn take_identity(&mut self) -> Option<MediaIdentity> {
        self.observer.take().map(MediaSource::into_identity)
    }

    pub(crate) fn prepare(
        &self,
        packet: AuthenticatedRtp<'_>,
    ) -> Result<Option<MediaIngress>, DropReason> {
        // The receiver may omit all extensions and use a full SRTP tag even
        // when the unseen source primary is reconstructed from RTX.
        if self.profile == Profile::Av1 && !packet.has_direct_full_tag_protection() {
            return Err(DropReason::Binding);
        }
        let ready_at = Instant::now();
        let arrival = packet.arrival;
        let primary_index = packet.authenticated_primary_index();
        let protected_bytes = (!packet.is_repair)
            .then(|| packet.protected_bytes())
            .flatten()
            .and_then(|bytes| u16::try_from(bytes).ok())
            .and_then(NonZeroU16::new);
        Ok(self
            .normalize_packet(packet)?
            .map(|(sequence, input)| MediaIngress {
                arrival,
                ready_at,
                position: primary_index.map_or(
                    Position::LowSequence(sequence),
                    Position::AuthenticatedPrimary,
                ),
                protected_bytes,
                media: (!input.payload.is_empty()).then(|| OwnedCopy {
                    payload: input.payload.into(),
                    is_repair: input.is_repair,
                    marker: input.marker,
                    timestamp: input.timestamp,
                    extensions: input
                        .extensions
                        .into_iter()
                        .map(|e| (e.kind, e.value.into()))
                        .collect(),
                    extension_width: input.extension_width,
                }),
            }))
    }

    fn normalize_packet<'a>(
        &self,
        packet: AuthenticatedRtp<'a>,
    ) -> Result<Option<(u16, Normalized<'a>)>, DropReason> {
        if packet.mid != self.binding.mid.as_ref()
            || packet
                .rid
                .is_some_and(|rid| Some(rid) != self.binding.rid.as_deref())
            || packet.primary_ssrc != self.binding.primary_ssrc
            || packet.primary_codec != &self.binding.codec
            || !self
                .binding
                .matches_repair(packet.repair_payload_type, packet.declared_repair_ssrc)
            || packet.receive_extensions != self.binding.receive_extensions.as_ref()
            || packet.allow_mixed_extensions != self.binding.allow_mixed_extensions
        {
            return Err(DropReason::Binding);
        }
        self.normalize_encoding(
            Wire {
                header: packet.wire_header(),
                padding: packet.padding_len(),
                first_csrc: packet.first_csrc(),
                extension_profile: packet.extension_profile(),
                is_repair: packet.is_repair,
                payload: packet.wire_payload(),
            },
            packet.wire_extensions(),
        )
    }

    fn normalize_encoding<'a>(
        &self,
        wire: Wire<'a>,
        extensions: impl Iterator<Item = (u8, &'a [u8])>,
    ) -> Result<Option<(u16, Normalized<'a>)>, DropReason> {
        let packet = normalize(
            &self.binding,
            self.profile,
            &self.selected,
            wire,
            extensions,
        )?;
        if self.requires_descriptor
            && packet.as_ref().is_some_and(|(_, media)| {
                !media.payload.is_empty()
                    && !media
                        .extensions
                        .iter()
                        .any(|ext| ext.kind == ExtensionKind::DependencyDescriptor)
            })
        {
            return Err(DropReason::MissingDependencyDescriptor);
        }
        Ok(packet)
    }
}

pub(crate) struct MediaIngress {
    pub arrival: Instant,
    pub ready_at: Instant,
    pub position: Position,
    pub protected_bytes: Option<NonZeroU16>,
    media: Option<OwnedCopy>,
}

struct OwnedCopy {
    payload: Box<[u8]>,
    is_repair: bool,
    marker: bool,
    timestamp: u32,
    extensions: ArrayVec<(ExtensionKind, Box<[u8]>), 8>,
    extension_width: Option<ExtensionWidth>,
}

impl MediaIngress {
    pub(crate) fn with_input<T>(&self, admit: impl FnOnce(Input<'_>) -> T) -> T {
        let Some(media) = &self.media else {
            return admit(Input::PrimaryPadding);
        };
        let extensions: ArrayVec<_, 8> = media
            .extensions
            .iter()
            .map(|(kind, value)| Extension { kind: *kind, value })
            .collect();
        admit(Input::Media(MediaCopy {
            is_repair: media.is_repair,
            marker: media.marker,
            timestamp: media.timestamp,
            payload: &media.payload,
            extensions: &extensions,
            extension_width: media.extension_width,
        }))
    }
}

struct Wire<'a> {
    header: RtpHeader,
    padding: usize,
    first_csrc: Option<u32>,
    extension_profile: Option<u16>,
    is_repair: bool,
    payload: &'a [u8],
}

struct Normalized<'a> {
    payload: &'a [u8],
    is_repair: bool,
    marker: bool,
    timestamp: u32,
    extensions: ArrayVec<Extension<'a>, 8>,
    extension_width: Option<ExtensionWidth>,
}

impl Normalized<'_> {
    fn as_input(&self) -> Input<'_> {
        if self.payload.is_empty() {
            Input::PrimaryPadding
        } else {
            Input::Media(MediaCopy {
                is_repair: self.is_repair,
                marker: self.marker,
                timestamp: self.timestamp,
                payload: self.payload,
                extensions: &self.extensions,
                extension_width: self.extension_width,
            })
        }
    }
}

fn normalize<'a>(
    binding: &EndpointBinding,
    profile: Profile,
    selected: &[(u8, ExtensionKind)],
    wire: Wire<'a>,
    elements: impl Iterator<Item = (u8, &'a [u8])>,
) -> Result<Option<(u16, Normalized<'a>)>, DropReason> {
    if !binding.extension_form.accepts(wire.extension_profile) {
        return Err(DropReason::ExtensionForm);
    }
    let expected = if wire.is_repair {
        binding.repair.ok_or(DropReason::Binding)?
    } else {
        RepairBinding {
            ssrc: binding.primary_ssrc,
            payload_type: binding.codec.payload_type,
        }
    };
    if wire.header.ssrc != expected.ssrc || wire.header.payload_type != expected.payload_type {
        return Err(DropReason::Binding);
    }
    if wire.first_csrc.is_some() && !binding.allow_csrc_stripping {
        return Err(DropReason::MixerAttribution);
    }
    let mut seen = 0_u16;
    let mut extensions = ArrayVec::<_, 8>::new();
    for (id, value) in elements {
        let Some(&(_, kind)) = selected.iter().find(|(selected, _)| *selected == id) else {
            continue;
        };
        let bit = 1 << kind as u8;
        if seen & bit != 0 {
            return Err(DropReason::DuplicateExtension);
        }
        seen |= bit;
        if kind == ExtensionKind::AbsoluteCaptureTime
            && binding.capture_clock_ssrc != Some(wire.first_csrc.unwrap_or(binding.primary_ssrc))
        {
            continue;
        }
        if kind == ExtensionKind::DependencyDescriptor {
            kind.validate(profile, value)
                .map_err(|error| DropReason::Admission(Rejected::Wire(error)))?;
            pulsebeam_core::dd::evidence::parse_independent(value)
                .map_err(DropReason::InvalidDependencyDescriptor)?;
        }
        extensions
            .try_push(Extension { kind, value })
            .map_err(|_| DropReason::DuplicateExtension)?;
    }
    if wire.payload.is_empty() {
        if wire.padding == 0 {
            return Err(DropReason::EmptyMedia);
        }
        if !wire.is_repair
            && extensions
                .iter()
                .any(|extension| extension.kind == ExtensionKind::DependencyDescriptor)
        {
            return Err(DropReason::PaddingWithDependencyDescriptor);
        }
        for ext in extensions {
            ext.kind
                .validate(profile, ext.value)
                .map_err(|error| DropReason::Admission(Rejected::Wire(error)))?;
        }
        return if wire.is_repair {
            Ok(None)
        } else {
            Ok(Some((
                wire.header.sequence,
                Normalized {
                    payload: &[],
                    is_repair: false,
                    marker: wire.header.marker,
                    timestamp: wire.header.timestamp,
                    extensions: ArrayVec::new(),
                    extension_width: None,
                },
            )))
        };
    }
    let (sequence, payload) = if wire.is_repair {
        let [high, low, payload @ ..] = wire.payload else {
            return Err(DropReason::RepairPayload);
        };
        if payload.is_empty() {
            return Err(DropReason::RepairPayload);
        }
        (u16::from_be_bytes([*high, *low]), payload)
    } else {
        (wire.header.sequence, wire.payload)
    };
    Ok(Some((
        sequence,
        Normalized {
            is_repair: wire.is_repair,
            marker: wire.header.marker,
            timestamp: wire.header.timestamp,
            payload,
            extensions,
            extension_width: wire.extension_profile.map(|profile| {
                if profile == 0xbede {
                    ExtensionWidth::One
                } else {
                    ExtensionWidth::Two
                }
            }),
        },
    )))
}

pub(crate) const AUDIO_LEVEL_URI: &str = "urn:ietf:params:rtp-hdrext:ssrc-audio-level";

pub(super) fn extension_kind(uri: &str) -> Option<ExtensionKind> {
    if uri == pulsebeam_core::dd::URI {
        return Some(ExtensionKind::DependencyDescriptor);
    }
    match uri {
        AUDIO_LEVEL_URI => Some(ExtensionKind::AudioLevel),
        "urn:3gpp:video-orientation" => Some(ExtensionKind::VideoOrientation),
        _ => match uri.strip_prefix("http://www.webrtc.org/experiments/rtp-hdrext/")? {
            "abs-capture-time" => Some(ExtensionKind::AbsoluteCaptureTime),
            "video-layers-allocation00" => Some(ExtensionKind::VideoLayersAllocation),
            "video-content-type" => Some(ExtensionKind::VideoContentType),
            "color-space" => Some(ExtensionKind::ColorSpace),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests;
