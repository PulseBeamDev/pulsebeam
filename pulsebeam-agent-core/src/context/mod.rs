mod effect;
mod event;

use alloc::{collections::vec_deque::VecDeque, vec::Vec};
pub use effect::*;
pub use event::*;

use crate::{http::HttpRequest, id::*};

pub(crate) struct AgentContext {
    pub ids: IdGenerator,
    pub effects: Effects,
}

impl AgentContext {
    pub(super) fn new() -> Self {
        Self {
            ids: IdGenerator::new(),
            effects: Effects::new(),
        }
    }

    pub(super) fn next_effect(&mut self) -> Option<AgentEffect> {
        self.effects.next_effect()
    }

    pub(crate) fn generation(&mut self) -> Generation {
        self.ids.generation()
    }

    pub(crate) fn request_id(&mut self) -> RequestId {
        self.ids.request()
    }

    pub(crate) fn timer_id(&mut self) -> TimerId {
        self.ids.timer()
    }

    pub(crate) fn emit(&mut self, effect: AgentEffect) {
        self.effects.emit(effect);
    }

    pub(crate) fn http_request(&mut self, req: HttpRequest) -> RequestId {
        let id = self.ids.request();
        self.emit(AgentEffect::Http(HttpEffect::Request { id, request: req }));
        id
    }

    pub(crate) fn rtc_open(&mut self) -> Generation {
        let id = self.ids.generation();
        self.emit(AgentEffect::Rtc(RtcEffect::CreateOffer { generation: id }));
        id
    }

    pub(crate) fn rtc_close(&mut self, generation: Generation) {
        self.emit(AgentEffect::Rtc(RtcEffect::Close { generation }));
    }

    pub(crate) fn dc_open(&mut self, generation: Generation, cfg: DataChannelConfig) {
        self.emit(AgentEffect::DataChannel(DataChannelEffect::Open {
            generation,
            config: cfg,
        }));
    }
}

pub(crate) struct Effects {
    inner: VecDeque<AgentEffect>,
}

impl Effects {
    pub(crate) fn new() -> Self {
        Self {
            inner: VecDeque::with_capacity(64),
        }
    }

    pub(crate) fn emit(&mut self, effect: AgentEffect) {
        self.inner.push_back(effect);
    }

    pub(crate) fn next_effect(&mut self) -> Option<AgentEffect> {
        self.inner.pop_front()
    }
}
