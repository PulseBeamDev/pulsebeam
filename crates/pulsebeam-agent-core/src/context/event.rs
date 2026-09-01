use crate::{
    NegotiatedTopology,
    http::HttpResponse,
    id::{DataChannelId, Generation, RequestId, TimerId},
};
use alloc::string::String;

pub enum AgentEvent {
    Rtc(RtcEvent),
    Http(HttpEvent),
    Timer(TimerEvent),
    DataChannel(DataChannelEvent),
}

pub enum RtcEvent {
    OfferCreated {
        generation: Generation,
        offer: String,
        topology: NegotiatedTopology,
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
}

pub enum HttpEvent {
    Response {
        id: RequestId,
        response: HttpResponse,
    },

    Failed {
        id: RequestId,
    },
}

pub enum TimerEvent {
    Fired { id: TimerId },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DataChannelEvent {
    Opened {
        generation: Generation,
        id: DataChannelId,
    },
    Message {
        generation: Generation,
        id: DataChannelId,
        payload: alloc::vec::Vec<u8>,
    },
    Closed {
        generation: Generation,
        id: DataChannelId,
    },
    WriteFailed {
        generation: Generation,
        id: DataChannelId,
    },
}
