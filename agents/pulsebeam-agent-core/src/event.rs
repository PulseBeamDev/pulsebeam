use alloc::{string::String, vec::Vec};

use crate::{ChannelId, Generation, HttpResponse, OfferResources, OperationId, TimerId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostEvent {
    Rtc(RtcEvent),
    Http(HttpEvent),
    Timer(TimerEvent),
    DataChannel(DataChannelEvent),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RtcEvent {
    OfferCreated {
        generation: Generation,
        offer: String,
        resources: OfferResources,
    },
    AnswerApplied {
        generation: Generation,
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
