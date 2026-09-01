use alloc::{string::String, vec::Vec};
use core::time::Duration;

use crate::{ChannelId, Generation, HttpRequest, MediaTopology, OperationId, SlotBinding, TimerId};

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
