//! PRODUCTION WIRE CONTRACT — EDIT WITH EXTREME CARE. Changes can break deployed clients, mixed-version nodes, and production traffic. Require compatibility review and wire-vector tests.
//!
//! [FabricPacket v1 §3.2, §6 and Appendix A][rfc]: map only optional canonical
//! hints into the negotiated endpoint RFC 8285 IDs. Never project Fabric ID 1.
//! The RTP writer owns MID, RID, feedback, protection and actual wire size.
//! Until a DD mapping bootstraps, whole-source RTP does not emit DD. Codec
//! bytes remain opaque. ACT preserves capture Q32.32 and uses big-endian signed
//! Q32.32 for a qualified capture-to-output sender offset.
//!
//! [rfc]: https://github.com/PulseBeamDev/pulsebeam/issues/275

use arrayvec::ArrayVec;
use pulsebeam_core::dd::output::{self, Mode, OutputMapper, Retention};
use pulsebeam_core::fabric::{ExtensionKind, FabricPacket, Profile};

use crate::route::fabric::local::{OutputEvidence, PacketView};

use super::fabric_ingress::{ExtensionForm, extension_kind};

pub mod timeline;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Vad {
    On,
    Off,
}

/// All capture options except `Unmapped` require that the output represents
/// the bound capture identity. Clock differences are signed Q32.32 seconds.
#[derive(Clone, Copy)]
pub enum CaptureClock {
    Unmapped,
    SameSender,
    SenderDelta(i64),
}

#[derive(Clone, Copy)]
pub struct SourceMetadata {
    pub vad: Vad,
    pub capture: CaptureClock,
    pub same_topology: bool,
}

/// Only receive-eligible mappings selected for this output. Per-leg transport
/// extensions are not part of the optional Fabric registry.
pub struct Mapping<'a> {
    pub id: u8,
    pub uri: &'a str,
    pub attributes: Option<&'a str>,
}

struct Hint {
    id: u8,
    kind: ExtensionKind,
    uri: Box<str>,
}

pub struct EndpointHints {
    profile: Profile,
    form: ExtensionForm,
    hints: ArrayVec<Hint, 7>,
    dependency: Option<Hint>,
    vad: Vad,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum InvalidMapping {
    Id,
    AmbiguousMeaning,
    AudioLevel,
}

#[derive(Clone, Copy)]
pub struct OutputLeg {
    pub mid: crate::media::Mid,
    pub primary: Option<u32>,
    pub max_wire_bytes: usize,
    pub replace_queued: bool,
}

struct WriteLayout<'a> {
    leg: OutputLeg,
    scope: pulsebeam_rtc::MediaScope,
    dd: Option<&'a [u8]>,
    answer_fid: Option<[u32; 2]>,
}

/// Candidate state for a single negotiated output sender.
pub struct SenderLeg<B> {
    output: OutputLeg,
    hints: EndpointHints,
    timeline: timeline::Timeline<B>,
    dd: Option<OutputMapper<B>>,
    dd_reset: Option<(B, u64)>,
    answer_fid: Option<[u32; 2]>,
    strict_dd: bool,
}

pub(crate) type CandidatePacket<'a, 'p> = (u64, &'a PacketView<'p>, OutputEvidence);

#[derive(Debug, Eq, PartialEq)]
pub enum AdmissionError {
    Timeline(timeline::Rejected),
    DependencyDescriptor(output::Rejected),
    Rtc(pulsebeam_rtc::WriteError),
}

impl SenderLeg<crate::shard::router::FabricLocalBranch> {
    pub fn new(
        output: OutputLeg,
        hints: EndpointHints,
        timeline: timeline::Timeline<crate::shard::router::FabricLocalBranch>,
    ) -> Self {
        Self {
            output,
            hints,
            timeline,
            dd: None,
            dd_reset: None,
            answer_fid: None,
            strict_dd: false,
        }
    }

    /// Select only for a qualified whole-source video leg and applied answer FID.
    pub(crate) fn with_answer_fid(mut self, pair: [u32; 2]) -> Self {
        self.answer_fid = Some(pair);
        self
    }

    pub(crate) fn with_required_dd_evidence(mut self, initial_frame: u16) -> Self {
        self.strict_dd = true;
        self.with_dependency_descriptors(initial_frame)
    }

    pub(crate) fn output(&self) -> OutputLeg {
        self.output
    }

    pub fn with_dependency_descriptors(mut self, initial_frame: u16) -> Self {
        if self.hints.dependency.is_some() {
            self.dd
                .get_or_insert_with(|| OutputMapper::new(initial_frame));
        }
        self
    }

    pub(crate) fn admit(
        &mut self,
        rtc: &mut pulsebeam_rtc::RtcConnection,
        at: std::time::Instant,
        branch: crate::shard::router::FabricLocalBranch,
        source: SourceMetadata,
        (_, packet, evidence): CandidatePacket<'_, '_>,
    ) -> Result<(), AdmissionError> {
        let OutputLeg {
            mid,
            primary,
            replace_queued,
            ..
        } = self.output;
        let (primary, sender_report) =
            rtc.rtp_egress_progress(mid.as_str(), primary)
                .ok_or(AdmissionError::Rtc(
                    pulsebeam_rtc::WriteError::UnknownStream,
                ))?;
        let (output, next) = self
            .timeline
            .prepare(
                branch,
                timeline::Input {
                    sequence: evidence.output_position,
                    timestamp: packet.header().timestamp,
                },
                at,
                timeline::Transmitted {
                    primary,
                    sender_report,
                },
                replace_queued,
            )
            .map_err(AdmissionError::Timeline)?;
        let scope = branch
            .scope()
            .ok_or(AdmissionError::Rtc(pulsebeam_rtc::WriteError::InvalidState))?
            .with_provenance(evidence.ingress);
        if self.answer_fid.is_some()
            && !self.strict_dd
            && (evidence.mode != Mode::Whole
                || !matches!(packet.header().profile, Profile::H264 | Profile::Av1)
                || packet.header().profile == Profile::H264
                    && (self.dd.is_some()
                        || packet
                            .extensions()
                            .any(|ext| ext.kind == ExtensionKind::DependencyDescriptor)))
        {
            return Err(AdmissionError::Rtc(pulsebeam_rtc::WriteError::InvalidRtp));
        }
        let leg = self.output;
        if self.strict_dd && (evidence.mode == Mode::Whole || evidence.dd.is_none()) {
            return Err(AdmissionError::DependencyDescriptor(
                output::Rejected::Bootstrap,
            ));
        }
        let mut enqueue = |dd: Option<&[u8]>| {
            self.hints.queue_rtp(
                rtc,
                at,
                WriteLayout {
                    leg,
                    scope,
                    dd,
                    answer_fid: self.answer_fid,
                },
                source,
                output,
                packet,
            )?;
            #[cfg(feature = "sim")]
            if dd.is_some() && packet.header().profile == Profile::Av1 {
                crate::sim_metrics::record_routing_counter("fabric_rtp_av1_dd_enqueued");
            }
            Ok(())
        };
        if let Some(mapper) = &mut self.dd {
            if let Some(fence) = evidence.reset_after
                && self.dd_reset != Some((branch, fence))
            {
                mapper.lose_context();
                self.dd_reset = Some((branch, fence));
            }
            let input = output::Packet {
                source: branch,
                output_sequence: output.sequence,
                mode: evidence.mode,
                entry: evidence.entry,
                dd: evidence
                    .dd
                    .as_ref()
                    .map(|(frame, qualified)| output::Evidence {
                        frame: *frame,
                        qualified,
                    }),
            };
            let mut write = Ok(());
            // RTC pending and repair lifetimes do not share a definitive expiry.
            let retention = Retention::SenderLifetime;
            let result = mapper.offer(at, retention, input, |dd| {
                write = enqueue(dd);
                write.is_ok()
            });
            let may_omit = match (result, write) {
                (Ok(()), _) => false,
                (
                    Err(output::Rejected::Admission),
                    Err(
                        pulsebeam_rtc::WriteError::InvalidRtp
                        | pulsebeam_rtc::WriteError::RtpTooLarge,
                    ),
                ) if evidence.mode == Mode::Whole && evidence.dd.is_some() => true,
                (_, Err(e)) => return Err(AdmissionError::Rtc(e)),
                (Err(_), Ok(())) if evidence.mode == Mode::Whole => true,
                (Err(e), Ok(())) => return Err(AdmissionError::DependencyDescriptor(e)),
            };
            if may_omit {
                mapper
                    .offer(
                        at,
                        retention,
                        output::Packet {
                            source: branch,
                            output_sequence: output.sequence,
                            mode: Mode::Whole,
                            entry: false,
                            dd: None,
                        },
                        |dd| {
                            write = enqueue(dd);
                            write.is_ok()
                        },
                    )
                    .map_err(|e| {
                        write
                            .err()
                            .map_or(AdmissionError::DependencyDescriptor(e), AdmissionError::Rtc)
                    })?;
            }
        } else if evidence.mode == Mode::Whole || self.strict_dd {
            enqueue(None).map_err(AdmissionError::Rtc)?;
        } else {
            return Err(AdmissionError::DependencyDescriptor(
                output::Rejected::Bootstrap,
            ));
        }
        self.timeline = next;
        self.output.replace_queued = false;
        Ok(())
    }

    pub(crate) fn committed_branch(&self) -> Option<crate::shard::router::FabricLocalBranch> {
        self.timeline.committed_source()
    }

    pub(crate) fn stage_branch(&mut self, branch: crate::shard::router::FabricLocalBranch) {
        self.output.replace_queued = self
            .committed_branch()
            .is_some_and(|active| active != branch);
    }

    #[cfg(test)]
    pub(crate) fn set_wire_budget(&mut self, max_wire_bytes: usize) {
        self.output.max_wire_bytes = max_wire_bytes;
    }

    pub(crate) fn cut_pending(&self) -> bool {
        self.output.replace_queued
    }
}

pub struct ProjectedHints<'a> {
    values: ArrayVec<(u8, Value<'a>), 7>,
}

enum Value<'a> {
    Borrowed(&'a [u8]),
    Capture([u8; 16]),
}

impl<'a> ProjectedHints<'a> {
    pub fn iter(&self) -> impl Iterator<Item = (u8, &[u8])> {
        self.values.iter().map(|(id, value)| {
            let bytes = match value {
                Value::Borrowed(bytes) => *bytes,
                Value::Capture(bytes) => bytes.as_slice(),
            };
            (*id, bytes)
        })
    }
}

impl EndpointHints {
    pub fn new(
        profile: Profile,
        form: ExtensionForm,
        mappings: &[Mapping<'_>],
    ) -> Result<Self, InvalidMapping> {
        let mut ids = [false; 256];
        let mut hints = ArrayVec::<Hint, 7>::new();
        let mut dependency = None;
        let mut vad = Vad::On;
        for mapping in mappings {
            let id = mapping.id;
            let selected = ids.get_mut(usize::from(id)).ok_or(InvalidMapping::Id)?;
            if *selected || id == 0 || (form == ExtensionForm::OneByte && id > 14) {
                return Err(InvalidMapping::Id);
            }
            *selected = true;
            let Some(kind) =
                extension_kind(mapping.uri).filter(|kind| kind.is_permitted_on(profile))
            else {
                continue;
            };
            if kind == ExtensionKind::DependencyDescriptor {
                if dependency.is_some() {
                    return Err(InvalidMapping::AmbiguousMeaning);
                }
                dependency = Some(Hint {
                    id,
                    kind,
                    uri: mapping.uri.into(),
                });
                continue;
            }
            if hints.iter().any(|hint| hint.kind == kind) {
                return Err(InvalidMapping::AmbiguousMeaning);
            }
            if kind == ExtensionKind::AudioLevel {
                vad = match mapping.attributes {
                    None | Some("vad=on") => Vad::On,
                    Some("vad=off") => Vad::Off,
                    _ => return Err(InvalidMapping::AudioLevel),
                };
            }
            hints
                .try_push(Hint {
                    id,
                    kind,
                    uri: mapping.uri.into(),
                })
                .map_err(|_| InvalidMapping::AmbiguousMeaning)?;
        }
        Ok(Self {
            profile,
            form,
            hints,
            dependency,
            vad,
        })
    }

    fn dependency_hint(&self, bytes: usize) -> Option<&Hint> {
        let max = if self.form == ExtensionForm::OneByte {
            16
        } else {
            255
        };
        self.dependency
            .as_ref()
            .filter(|_| (3..=max).contains(&bytes))
    }

    /// Materialize a checked RTC write for the current negotiated leg. The
    /// caller must check branch authority and commit its timeline only on
    /// successful admission. This performs no subscriber or path selection.
    #[allow(
        clippy::disallowed_types,
        reason = "copy into the destination's RTC-owned immutable storage; Bytes never cross a shard handoff"
    )]
    fn queue_rtp<P: AsRef<[u8]>, E: AsRef<[u8]>>(
        &self,
        rtc: &mut pulsebeam_rtc::RtcConnection,
        at: std::time::Instant,
        WriteLayout {
            leg,
            scope,
            dd,
            answer_fid,
        }: WriteLayout<'_>,
        source: SourceMetadata,
        output: timeline::Projected,
        packet: &FabricPacket<P, E>,
    ) -> Result<(), pulsebeam_rtc::WriteError> {
        use pulsebeam_rtc::{
            BoundedRtp, RtpClockAnchor, RtpExtension, RtpExtensionForm, RtpWrite, WriteError,
        };

        let form = match self.form {
            ExtensionForm::OneByte => RtpExtensionForm::OneByte,
            ExtensionForm::TwoByte => RtpExtensionForm::TwoByte,
            ExtensionForm::Mixed => RtpExtensionForm::Mixed,
        };
        let mid = leg.mid.as_str();
        if packet.header().profile != self.profile
            || self.profile == Profile::Data
            || rtc.rtp_egress_extension_form(mid, leg.primary) != Some(form)
        {
            return Err(WriteError::InvalidRtp);
        }
        let projected = self.project(source, packet);
        let mut extensions = Vec::with_capacity(
            projected
                .values
                .len()
                .saturating_add(usize::from(dd.is_some())),
        );
        if let Some(value) = dd {
            let hint = self
                .dependency_hint(value.len())
                .ok_or(WriteError::InvalidRtp)?;
            if rtc.rtp_egress_extension_id(mid, leg.primary, &hint.uri) != Some(hint.id) {
                return Err(WriteError::InvalidRtp);
            }
            extensions.push(RtpExtension {
                uri: hint.uri.as_ref().into(),
                value: bytes::Bytes::copy_from_slice(value),
            });
        }
        for (id, value) in projected.iter() {
            let hint = self
                .hints
                .iter()
                .find(|hint| hint.id == id)
                .ok_or(WriteError::InvalidRtp)?;
            if rtc.rtp_egress_extension_id(mid, leg.primary, &hint.uri) != Some(id) {
                return Err(WriteError::InvalidRtp);
            }
            extensions.push(RtpExtension {
                uri: hint.uri.as_ref().into(),
                value: bytes::Bytes::copy_from_slice(value),
            });
        }
        let clock = RtpClockAnchor {
            timestamp: output.anchor_timestamp,
            canonical_offset: output.canonical_offset,
        };
        let write = RtpWrite {
            sequence: output.sequence,
            timestamp: output.timestamp,
            marker: packet.header().marker,
            payload: bytes::Bytes::copy_from_slice(packet.payload()),
            extensions,
        };
        let policy = BoundedRtp {
            max_wire_bytes: leg.max_wire_bytes,
            max_primary_wire_bytes: None,
            clock: Some(clock),
            scope: Some(scope),
            replace_queued: leg.replace_queued,
        };
        match answer_fid {
            Some(pair) => rtc.write_fabric_rtp_bounded(at, mid, leg.primary, write, policy, pair),
            None => rtc.write_rtp_bounded(at, mid, leg.primary, write, policy),
        }
        .map_err(|rejected| rejected.reason)
    }

    /// Compute only per-leg optional values. Output forms and appbits are
    /// selected by the transport writer, not copied from the canonical image.
    pub fn project<'a, P: AsRef<[u8]>, E: AsRef<[u8]>>(
        &'a self,
        source: SourceMetadata,
        packet: &'a FabricPacket<P, E>,
    ) -> ProjectedHints<'a> {
        let mut values = ArrayVec::new();
        for ext in packet.extensions() {
            let Some(hint) = self.hints.iter().find(|hint| hint.kind == ext.kind) else {
                continue;
            };
            let value = match ext.kind {
                ExtensionKind::AbsoluteCaptureTime => capture(source.capture, ext.value),
                ExtensionKind::AudioLevel if source.vad == Vad::Off && self.vad == Vad::On => None,
                ExtensionKind::VideoLayersAllocation if !source.same_topology => None,
                _ => Some(Value::Borrowed(ext.value)),
            };
            if let Some(value) = value {
                let len = match &value {
                    Value::Borrowed(bytes) => bytes.len(),
                    Value::Capture(bytes) => bytes.len(),
                };
                if self.form != ExtensionForm::OneByte || len <= 16 {
                    let _ = values.try_push((hint.id, value));
                }
            }
        }
        ProjectedHints { values }
    }
}

fn capture(clock: CaptureClock, input: &[u8]) -> Option<Value<'_>> {
    let delta = match clock {
        CaptureClock::Unmapped => return None,
        CaptureClock::SameSender => return Some(Value::Borrowed(input)),
        CaptureClock::SenderDelta(delta) => delta,
    };
    let words: &[u8; 16] = input.try_into().ok()?;
    let (capture, offset) = words.split_at(8);
    let shifted = i64::from_be_bytes(offset.try_into().ok()?)
        .checked_add(delta)?
        .to_be_bytes();
    let mut output = [0; 16];
    let (time, offset) = output.split_at_mut(8);
    time.copy_from_slice(capture);
    offset.copy_from_slice(&shifted);
    Some(Value::Capture(output))
}

#[cfg(test)]
mod tests;
