use core::time::Duration;

use crate::{http::HttpRequest, id::*};
use alloc::{string::String, vec::Vec};

pub(crate) struct Effects {
    inner: Vec<AgentEffect>,
}

impl Effects {
    pub(crate) fn new() -> Self {
        Self {
            inner: Vec::with_capacity(64),
        }
    }

    pub(crate) fn emit(&mut self, effect: AgentEffect) {
        self.inner.push(effect);
    }

    pub(crate) fn extend(&mut self, effects: impl IntoIterator<Item = AgentEffect>) {
        self.inner.extend(effects);
    }
}

impl IntoIterator for Effects {
    type Item = AgentEffect;
    type IntoIter = alloc::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.inner.into_iter()
    }
}

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
