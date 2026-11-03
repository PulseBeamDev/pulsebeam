use alloc::{string::String, vec::Vec};

use crate::{ChannelId, Generation, HttpResponse, OfferResources, OperationId, TimerId};

#[derive(Clone, Debug, PartialEq)]
pub enum HostEvent {
    Rtc(RtcEvent),
    Http(HttpEvent),
    Timer(TimerEvent),
    DataChannel(DataChannelEvent),
}

#[derive(Clone, Debug, PartialEq)]
pub enum RtcEvent {
    OfferCreated {
        generation: Generation,
        offer: String,
        resources: OfferResources,
    },
    BootstrapMediaPrepared {
        generation: Generation,
        preparation: Result<crate::client_sdp::Bindings, crate::client_sdp::Error>,
    },
    MediaPrepared {
        generation: Generation,
        operation: OperationId,
        slots: Vec<crate::SlotBinding>,
        outcome: crate::client_sdp::ApplyOutcome,
        preparation: Result<crate::client_sdp::Bindings, crate::client_sdp::Error>,
    },
    MediaObserved {
        generation: Generation,
        bindings: crate::client_sdp::Bindings,
    },
    AnswerApplied {
        generation: Generation,
    },
    BootstrapRejected {
        generation: Generation,
        message: String,
    },
    Connected {
        generation: Generation,
    },
    Disconnected {
        generation: Generation,
    },
    Failed {
        generation: Generation,
        message: String,
    },
    Closed {
        generation: Generation,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpEvent {
    Response {
        operation: OperationId,
        response: HttpResponse,
    },
    Failed {
        operation: OperationId,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerEvent {
    Fired { timer: TimerId },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataChannelEvent {
    Prepared {
        generation: Generation,
        channel: ChannelId,
        preparation: Result<crate::PreparedDataChannel, String>,
    },
    Opened {
        generation: Generation,
        channel: ChannelId,
    },
    Closed {
        generation: Generation,
        channel: ChannelId,
    },
    Message {
        generation: Generation,
        channel: ChannelId,
        payload: Vec<u8>,
    },
    Sent {
        operation: OperationId,
        generation: Generation,
        channel: ChannelId,
    },
    SendFailed {
        operation: OperationId,
        generation: Generation,
        channel: ChannelId,
        message: String,
    },
}
