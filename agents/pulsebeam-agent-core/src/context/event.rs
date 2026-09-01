use crate::http::*;
use crate::id::*;
use alloc::string::String;

pub enum AgentEvent {
    Rtc(RtcEvent),
    Http(HttpEvent),
    Timer(TimerEvent),
}

pub enum RtcEvent {
    OfferCreated {
        generation: Generation,
        offer: String,
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
    Fired { id: OperationId },
}
