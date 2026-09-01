use crate::{
    LocalSlotIntent, Topology,
    http::HttpRequest,
    id::{DataChannelId, Generation, RequestId, TimerId},
};
use alloc::{string::String, vec::Vec};
use core::time::Duration;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum AgentEffect {
    Rtc(RtcEffect),
    Http(HttpEffect),
    Timer(TimerEffect),
    DataChannel(DataChannelEffect),
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum RtcEffect {
    CreateTransport {
        generation: Generation,
        topology: Topology,
        signaling_channel: DataChannelId,
    },
    ApplyAnswer {
        generation: Generation,
        answer: String,
    },
    CloseTransport {
        generation: Generation,
    },
    ReconcileLocalSlots {
        generation: Generation,
        slots: Vec<LocalSlotIntent>,
    },
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum DataChannelEffect {
    Open {
        generation: Generation,
        id: DataChannelId,
        config: DataChannelConfig,
    },
    Close {
        generation: Generation,
        id: DataChannelId,
    },
    Send {
        generation: Generation,
        id: DataChannelId,
        payload: Vec<u8>,
    },
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum DataChannelReliability {
    Reliable,
    MaxRetransmits(u16),
    MaxPacketLifetime(u16),
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct DataChannelConfig {
    pub label: String,
    pub protocol: String,
    pub ordered: bool,
    pub negotiated: Option<u16>,
    pub reliability: DataChannelReliability,
}

impl DataChannelConfig {
    pub fn reliable(label: String) -> Self {
        Self {
            label,
            protocol: "pulsebeam/v1".into(),
            ordered: true,
            negotiated: None,
            reliability: DataChannelReliability::Reliable,
        }
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum HttpEffect {
    Request { id: RequestId, request: HttpRequest },
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum TimerEffect {
    Schedule { id: TimerId, after: Duration },

    Cancel { id: TimerId },
}
