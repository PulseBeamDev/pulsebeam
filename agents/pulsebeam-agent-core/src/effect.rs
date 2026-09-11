use alloc::{string::String, vec::Vec};
use core::time::Duration;

use crate::{
    ChannelId, Generation, HttpRequest, MediaDirection, MediaKind, MediaSlot, MediaTopology,
    OperationId, SlotBinding, TimerId,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Rtc(RtcEffect),
    Http(HttpEffect),
    Timer(TimerEffect),
    DataChannel(DataChannelEffect),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RtcEffect {
    CreateOffer {
        generation: Generation,
        topology: MediaTopology,
        data_channels: Vec<DataChannelSpec>,
    },
    ApplyAnswer {
        generation: Generation,
        answer: String,
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
    Schedule { timer: TimerId, after: Duration },
    Cancel { timer: TimerId },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataChannelEffect {
    Send {
        operation: OperationId,
        generation: Generation,
        channel: ChannelId,
        binary: bool,
        payload: Vec<u8>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataChannelSpec {
    pub label: String,
    pub ordered: bool,
    pub reliability: DataChannelReliability,
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
