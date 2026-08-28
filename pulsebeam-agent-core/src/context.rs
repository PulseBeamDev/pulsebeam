use crate::{
    effect::{AgentEffect, DataChannelConfig, DataChannelEffect, Effects, HttpEffect, RtcEffect},
    host::Instant,
    http::HttpRequest,
    id::*,
};

pub(crate) struct AgentContext<'a> {
    pub ids: &'a mut IdGenerator,
    pub now: Instant,
    pub effects: &'a mut Effects,
}

impl AgentContext<'_> {
    pub(crate) fn now(&self) -> Instant {
        self.now
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

    pub(crate) fn rtc_close(&mut self, generation: Generation) {
        self.emit(AgentEffect::Rtc(RtcEffect::Close { generation }));
    }

    pub(crate) fn dc_open(&mut self, cfg: DataChannelConfig) -> Generation {
        let id = self.ids.generation();
        self.emit(AgentEffect::DataChannel(DataChannelEffect::Open {
            generation: id,
            config: cfg,
        }));
        id
    }
}
