use alloc::{string::String, vec::Vec};
use core::time::Duration;

use crate::{
    ChannelId, Generation, HttpRequest, MediaDirection, MediaKind, MediaSlot, OperationId,
    SlotBinding, TimerId,
};

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Rtc(RtcEffect),
    Http(HttpEffect),
    Timer(TimerEffect),
    DataChannel(DataChannelEffect),
    PublicationInput(crate::PublicationInputControl),
    MediaObservation(crate::MediaObservationControl),
}

#[derive(Clone, Debug, PartialEq)]
pub struct BootstrapCapabilities {
    pub rtp: pulsebeam_proto::state::RtpCapabilities,
    pub data_delivery: Vec<pulsebeam_proto::state::DataDelivery>,
}

impl BootstrapCapabilities {
    pub fn applied_context(
        &self,
        offer: &str,
        answer: &str,
        generation: Generation,
    ) -> Result<crate::client_sdp::Context, crate::client_sdp::Error> {
        let [a, b, c, d, _, _, _, _] = generation.get().to_le_bytes();
        crate::client_sdp::Context::new(
            offer,
            answer,
            crate::client_sdp::Capabilities::from_wire(&self.rtp)?,
            u32::from_le_bytes([a, b, c, d]),
            crate::client_sdp::Limits::default(),
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RtcEffect {
    CreateOffer {
        generation: Generation,
        data_channels: Vec<DataChannelSpec>,
    },
    ApplyAnswer {
        generation: Generation,
        answer: String,
        capabilities: BootstrapCapabilities,
    },
    CreateDataChannel {
        generation: Generation,
        channel: ChannelId,
        specification: DataChannelSpec,
    },
    CloseDataChannel {
        generation: Generation,
        channel: ChannelId,
    },
    PrepareMedia {
        generation: Generation,
        operation: OperationId,
        requests: Vec<crate::client_sdp::Preparation>,
    },
    AllocateMedia {
        generation: Generation,
        operation: OperationId,
        allocations: Vec<crate::client_sdp::Allocation>,
    },
    RetireMedia {
        generation: Generation,
        operation: OperationId,
        mids: Vec<String>,
    },
    CancelMediaPreparation {
        generation: Generation,
        operation: OperationId,
    },
    Close {
        generation: Generation,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpEffect {
    Request {
        operation: OperationId,
        generation: Option<Generation>,
        request: HttpRequest,
    },
    Cancel {
        operation: OperationId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimerEffect {
    /// Zero duration requests the next runnable host boundary, not a wall-clock timer.
    Schedule {
        timer: TimerId,
        after: Duration,
    },
    Cancel {
        timer: TimerId,
    },
}

#[derive(Clone, PartialEq, Eq)]
pub enum DataChannelEffect {
    Send {
        operation: OperationId,
        generation: Generation,
        channel: ChannelId,
        binary: bool,
        payload: Vec<u8>,
    },
}

impl core::fmt::Debug for DataChannelEffect {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Send {
                operation,
                generation,
                channel,
                binary,
                ..
            } => formatter
                .debug_struct("Send")
                .field("operation", operation)
                .field("generation", generation)
                .field("channel", channel)
                .field("binary", binary)
                .field("payload", &"[REDACTED]")
                .finish(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataChannelSpec {
    pub label: String,
    pub ordered: bool,
    pub reliability: DataChannelReliability,
    /// Physical SCTP stream for external negotiation, not a callback ChannelId.
    pub negotiated_id: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedDataChannel {
    pub stream_id: u16,
    pub negotiated: bool,
    pub ordered: bool,
    pub reliability: DataChannelReliability,
    pub association_max_channels: Option<u32>,
}

impl PreparedDataChannel {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.stream_id == u16::MAX
            || self.association_max_channels.is_some_and(|capacity| {
                capacity == 0 || capacity > 65_536 || u32::from(self.stream_id) >= capacity
            })
        {
            return Err("invalid prepared SCTP stream");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataChannelReliability {
    Reliable,
    MaxRetransmits(u16),
    MaxPacketLifetime(u16),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfferResources {
    pub slots: Vec<SlotBinding>,
    pub signaling_channel: ChannelId,
    pub data_channels: Vec<DataChannelBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataChannelBinding {
    pub label: String,
    pub channel: ChannelId,
}

pub fn negotiated_slot_bindings(
    offer: &str,
    slots: impl IntoIterator<Item = (MediaSlot, String)>,
) -> Result<Vec<SlotBinding>, &'static str> {
    #[derive(Clone)]
    struct Section {
        index: u32,
        mid: Option<String>,
        kind: Option<MediaKind>,
        direction: Option<MediaDirection>,
    }

    let mut sections = Vec::<Section>::new();
    let mut session_direction = None;
    for raw in offer.lines() {
        let line = raw.trim_end_matches('\r');
        if let Some(media) = line.strip_prefix("m=") {
            let index = u32::try_from(sections.len()).map_err(|_| "too many SDP media sections")?;
            let kind = match media.split_whitespace().next() {
                Some("video") => Some(MediaKind::Video),
                Some("audio") => Some(MediaKind::Audio),
                _ => None,
            };
            sections.push(Section {
                index,
                mid: None,
                kind,
                direction: session_direction,
            });
        } else if let Some(mid) = line.strip_prefix("a=mid:") {
            let section = sections
                .last_mut()
                .ok_or("SDP MID appears before a media section")?;
            if section.mid.replace(mid.to_owned()).is_some() {
                return Err("SDP media section has more than one MID");
            }
        } else {
            let direction = match line {
                "a=sendonly" => Some(MediaDirection::SendOnly),
                "a=recvonly" => Some(MediaDirection::ReceiveOnly),
                "a=sendrecv" | "a=inactive" => {
                    if sections.last().is_none_or(|section| section.kind.is_none()) {
                        None
                    } else {
                        return Err("SDP RTP media section is not unidirectional");
                    }
                }
                _ => continue,
            };
            if sections.is_empty() {
                session_direction = direction;
            } else if let Some(section) = sections.last_mut() {
                section.direction = direction;
            }
        }
    }

    let rtp_count = sections
        .iter()
        .filter(|section| section.kind.is_some())
        .count();
    let mut bindings = Vec::new();
    let mut bound_indices = alloc::collections::BTreeSet::new();
    for (slot, mid) in slots {
        let mut matches = sections
            .iter()
            .filter(|section| section.mid.as_deref() == Some(mid.as_str()));
        let section = matches.next().ok_or("negotiated MID is missing from SDP")?;
        if matches.next().is_some() {
            return Err("SDP MID is mapped more than once");
        }
        let kind = section
            .kind
            .ok_or("negotiated MID is not an RTP media section")?;
        let direction = section
            .direction
            .ok_or("SDP RTP media section has no direction")?;
        if slot.kind() != kind || slot.direction() != direction {
            return Err("negotiated media kind or direction does not match its slot");
        }
        if !bound_indices.insert(section.index) {
            return Err("SDP media section is mapped more than once");
        }
        bindings.push(SlotBinding {
            slot,
            mid,
            media_index: section.index,
            kind,
            direction,
        });
    }
    if bound_indices.len() != rtp_count {
        return Err("SDP RTP media section is missing a slot binding");
    }
    Ok(bindings)
}
