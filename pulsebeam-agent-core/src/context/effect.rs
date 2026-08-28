use crate::http::*;
use crate::id::*;
use alloc::string::String;
use core::time::Duration;

pub enum AgentEffect {
    Rtc(RtcEffect),
    Http(HttpEffect),
    Timer(TimerEffect),
    DataChannel(DataChannelEffect),
}

pub enum RtcEffect {
    CreateOffer {
        generation: Generation,
    },
    ApplyAnswer {
        generation: Generation,
        answer: String,
    },
    Close {
        generation: Generation,
    },
}

pub enum DataChannelEffect {
    Open {
        generation: Generation,
        config: DataChannelConfig,
    },
    Close {
        generation: Generation,
        label: DataChannelLabel,
    },
}

pub enum DataChannelReliability {
    Reliable,
    MaxRetransmits(u16),
    MaxPacketLifetime(u16),
}

pub struct DataChannelConfig {
    pub label: String,
    pub protocol: String,
    pub ordered: bool,
    pub negotiated: Option<DataChannelId>,
    pub reliability: DataChannelReliability,
}

impl DataChannelConfig {
    pub(crate) fn reliable(label: String) -> Self {
        Self {
            label,
            protocol: "pulsebeam/v1".into(),
            ordered: true,
            negotiated: None,
            reliability: DataChannelReliability::Reliable,
        }
    }
}

pub enum HttpEffect {
    Request { id: RequestId, request: HttpRequest },
}

pub enum TimerEffect {
    Schedule { id: OperationId, after: Duration },

    Cancel { id: OperationId },
}
