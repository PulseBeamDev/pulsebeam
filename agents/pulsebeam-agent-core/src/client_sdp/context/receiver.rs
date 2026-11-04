use super::*;
use pulsebeam_proto::state::{self as wire, validation};

impl Context {
    /// Actual receiving codec, payload and extension choices, without RTP identities.
    pub fn receive_offer(&self, offer: &str, mid: &str) -> Result<wire::RtpReceiveOffer, Error> {
        let document = Document::parse(offer)?;
        self.offered_reception(&document, mid)
    }

    pub(in crate::client_sdp) fn validate_receive_target(
        &self,
        slot: &crate::SlotBinding,
    ) -> Result<(), Error> {
        let media = self
            .accepted
            .local
            .media
            .get(slot.media_index as usize)
            .ok_or(Error::Identifier)?;
        if slot.direction != crate::MediaDirection::ReceiveOnly
            || slot.slot.direction() != slot.direction
            || slot.slot.kind() != slot.kind
            || media.section.mid != slot.mid
            || wire_kind(media.section.kind)?
                != match slot.kind {
                    crate::MediaKind::Audio => wire::TrackKind::Audio,
                    crate::MediaKind::Video => wire::TrackKind::Video,
                }
            || media.section.stopped
            || media.section.direction != super::super::Direction::RecvOnly
            || self
                .accepted
                .retained
                .receptions
                .iter()
                .any(|binding| binding.mid == slot.mid)
                && !self.accepted.unclaimed_reception(&slot.mid)
        {
            return Err(Error::Envelope(slot.mid.clone()));
        }
        Ok(())
    }

    /// Observe a completed platform answer, not the synthesized candidate.
    /// SDP exposes the receiver identities before packets can be admitted.
    pub fn observe_receive_answer(&self, answer: &str, mid: &str) -> Result<Binding, Error> {
        let expected = self
            .pending
            .as_ref()
            .and_then(|(_, accepted)| {
                accepted
                    .bindings
                    .receptions
                    .iter()
                    .find(|binding| binding.mid == mid)
            })
            .ok_or(Error::CandidateState)?;
        let document = Document::parse(answer)?;
        let media = document
            .media
            .iter()
            .find(|media| media.section.mid == mid)
            .ok_or(Error::Observation)?;
        let section = &media.section;
        let mut observed = Binding {
            key: expected.key.clone(),
            group: expected.group.clone(),
            mid: section.mid.clone(),
            kind: section.kind,
            codec: section
                .codecs
                .first()
                .filter(|_| section.codecs.len() == 1)
                .cloned()
                .ok_or(Error::Observation)?,
            extensions: section
                .extensions
                .iter()
                .cloned()
                .map(|mut extension| {
                    extension.direction = extension.direction.inverse();
                    extension
                })
                .collect(),
            encodings: section.encodings.clone(),
            rtcp: section.rtcp.clone(),
        };
        // A receive offer does not advertise an encoding-mode execution surface.
        // Do not echo a server annotation that this SDP backend cannot observe.
        if expected
            .encodings
            .iter()
            .any(|encoding| encoding.runtime.scalability_mode.is_some())
            || section.stopped
            || section.direction != super::super::Direction::SendOnly
            || !same_reception(expected, &observed)
        {
            return Err(Error::Observation);
        }
        observed
            .extensions
            .retain(|extension| extension.direction.receives());
        Ok(observed)
    }

    fn offered_reception(
        &self,
        offered: &Document,
        mid: &str,
    ) -> Result<wire::RtpReceiveOffer, Error> {
        let media = offered
            .media
            .iter()
            .find(|media| media.section.mid == mid)
            .ok_or(Error::Identifier)?;
        if media.section.stopped
            || media.section.direction != super::super::Direction::RecvOnly
            || !media.transport.same_peer(&self.local_transport)
        {
            return Err(Error::Envelope(mid.into()));
        }
        let kind = wire_kind(media.section.kind)?;
        let codecs = media
            .section
            .codecs
            .iter()
            .filter_map(|codec| self.select_codec(codec).ok())
            .map(|codec| wire_codec(media.section.kind, &codec))
            .collect::<Result<Vec<_>, _>>()?;
        let header_extensions = media
            .section
            .extensions
            .iter()
            .filter(|extension| {
                extension.direction.receives()
                    && self.capabilities.extensions.contains(&extension.uri)
            })
            .map(|extension| wire::RtpHeaderExtensionParameters {
                id: u32::from(extension.id),
                uri: extension.uri.clone(),
            })
            .collect();
        let rtcp_modes = self
            .capabilities
            .rtcp_modes
            .iter()
            .filter_map(|mode| match mode {
                RtcpMode::Compound => Some(wire::RtcpMode::Compound.into()),
                RtcpMode::ReducedSize if media.section.rtcp.reduced_size => {
                    Some(wire::RtcpMode::ReducedSize.into())
                }
                _ => None,
            })
            .collect();
        let offer = wire::RtpReceiveOffer {
            mid: mid.into(),
            codecs,
            header_extensions,
            rtcp_modes,
        };
        validation::validate_receive_offer(&offer, kind)
            .map_err(|_| Error::Envelope(mid.into()))?;
        Ok(offer)
    }

    /// Prepare one authoritative plan. Bootstrap receives may be claimed once;
    /// subsequent physical identities remain immutable until terminal retirement.
    pub fn prepare_receive_plan(
        &mut self,
        offer: &str,
        owner: MediaOwner,
        parameters: &wire::RtpParameters,
    ) -> Result<Candidate, Error> {
        // Zero is reserved by libwebrtc's receiver frame-transformer API.
        if parameters
            .encodings
            .iter()
            .any(|encoding| encoding.ssrc == Some(0) || encoding.rtx_ssrc == Some(0))
        {
            return Err(Error::Envelope(parameters.mid.clone()));
        }
        let offered = Document::parse(offer)?;
        let observation = self.offered_reception(&offered, &parameters.mid)?;
        let media = offered
            .media
            .iter()
            .find(|media| media.section.mid == parameters.mid)
            .ok_or(Error::Identifier)?;
        validation::validate_receive_plan(&observation, parameters, wire_kind(media.section.kind)?)
            .map_err(|_| Error::Envelope(parameters.mid.clone()))?;
        let selected = parameters.codec.as_ref().ok_or(Error::Identifier)?;
        let payload = selected
            .payload_type
            .and_then(|payload| u8::try_from(payload).ok())
            .ok_or(Error::Identifier)?;
        let mut codec = media
            .section
            .codecs
            .iter()
            .find(|codec| codec.payload_type == payload)
            .and_then(|codec| self.select_codec(codec).ok())
            .ok_or(Error::Identifier)?;
        codec.parameters.feedback = selected.feedback.iter().cloned().collect();
        if selected.rtx_payload_type.is_none() {
            codec.repair = None;
        }
        let rtcp = parameters.rtcp.as_ref().ok_or(Error::Identifier)?;
        let binding = Binding {
            key: owner.key.clone(),
            group: owner.group.clone(),
            mid: parameters.mid.clone(),
            kind: media.section.kind,
            codec,
            extensions: media
                .section
                .extensions
                .iter()
                .filter(|extension| {
                    parameters.header_extensions.iter().any(|selected| {
                        selected.id == u32::from(extension.id) && selected.uri == extension.uri
                    })
                })
                .cloned()
                .collect(),
            encodings: parameters
                .encodings
                .iter()
                .map(|encoding| Encoding {
                    rid: None,
                    primary: encoding.ssrc,
                    repair: encoding.rtx_ssrc,
                    paused: false,
                    runtime: RuntimeEncoding {
                        scalability_mode: encoding.scalability_mode.clone(),
                        ..Default::default()
                    },
                })
                .collect(),
            rtcp: RtcpParameters {
                cname: Some(rtcp.cname.clone()),
                reduced_size: rtcp.reduced_size,
            },
        };
        if let Some(previous) = self
            .accepted
            .retained
            .receptions
            .iter()
            .find(|previous| previous.mid == binding.mid)
            && !self.accepted.unclaimed_reception(&binding.mid)
            && !same_reception(previous, &binding)
        {
            return Err(Error::Envelope(binding.mid));
        }
        let request = Preparation {
            mid: parameters.mid.clone(),
            payload_type: payload,
            publication: None,
            reception: Some(owner),
        };
        self.prepare_inner(offer, &[request], &[], None, Some(&binding))
    }
}

fn same_reception(previous: &Binding, next: &Binding) -> bool {
    previous.key == next.key
        && previous.group == next.group
        && previous.mid == next.mid
        && previous.kind == next.kind
        && previous.codec == next.codec
        && previous.rtcp == next.rtcp
        && previous.encodings == next.encodings
        && previous.extensions.len() == next.extensions.len()
        && previous.extensions.iter().all(|old| {
            next.extensions.iter().any(|new| {
                old.id == new.id
                    && old.uri == new.uri
                    && old.direction.receives() == new.direction.receives()
            })
        })
}

fn wire_kind(kind: Kind) -> Result<wire::TrackKind, Error> {
    match kind {
        Kind::Audio => Ok(wire::TrackKind::Audio),
        Kind::Video => Ok(wire::TrackKind::Video),
        Kind::Application => Err(Error::Identifier),
    }
}

fn wire_codec(kind: Kind, codec: &Codec) -> Result<wire::RtpCodecParameters, Error> {
    let prefix = match kind {
        Kind::Audio => "audio",
        Kind::Video => "video",
        Kind::Application => return Err(Error::Identifier),
    };
    Ok(wire::RtpCodecParameters {
        payload_type: Some(u32::from(codec.payload_type)),
        rtx_payload_type: codec
            .repair
            .as_ref()
            .map(|repair| u32::from(repair.payload_type)),
        feedback: codec.parameters.feedback.iter().cloned().collect(),
        format: Some(wire::CodecFormat {
            mime_type: format!("{prefix}/{}", codec.parameters.name.to_ascii_lowercase()),
            clock_rate: codec.parameters.clock_rate,
            channels: codec.parameters.channels.map(u32::from),
            parameters: codec
                .parameters
                .fmtp
                .iter()
                .map(|(name, value)| wire::CodecParameter {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
        }),
    })
}
