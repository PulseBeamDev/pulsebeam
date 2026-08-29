mod effect;
mod event;

use alloc::collections::{BTreeSet, VecDeque};
pub use effect::*;
pub use event::*;

use crate::{http::HttpRequest, id::*};
use core::time::Duration;

#[allow(
    dead_code,
    reason = "the session and topic state machines consume this contract in later plans"
)]
pub(crate) struct AgentContext {
    pub ids: IdGenerator,
    pub effects: Effects,
    correlations: Correlations,
}

#[allow(
    dead_code,
    reason = "the session and topic state machines consume this contract in later plans"
)]
impl AgentContext {
    pub(super) fn new() -> Self {
        Self {
            ids: IdGenerator::new(),
            effects: Effects::new(),
            correlations: Correlations::default(),
        }
    }

    pub(super) fn next_effect(&mut self) -> Option<AgentEffect> {
        self.effects.next_effect()
    }

    pub(crate) fn generation(&mut self) -> Option<Generation> {
        let generation = self.ids.generation()?;
        self.correlations.begin_transport(generation);
        Some(generation)
    }

    pub(crate) fn request_id(&mut self) -> Option<RequestId> {
        self.ids.request()
    }

    pub(crate) fn timer_id(&mut self) -> Option<TimerId> {
        let timer = self.ids.timer()?;
        self.correlations.timers.insert(timer);
        Some(timer)
    }

    pub(crate) fn schedule_timer(&mut self, after: Duration) -> Option<TimerId> {
        let id = self.timer_id()?;
        self.emit(AgentEffect::Timer(TimerEffect::Schedule { id, after }));
        Some(id)
    }

    pub(crate) fn cancel_timer(&mut self, id: TimerId) {
        self.correlations.timers.remove(&id);
        self.emit(AgentEffect::Timer(TimerEffect::Cancel { id }));
    }

    pub(crate) fn emit(&mut self, effect: AgentEffect) {
        self.effects.emit(effect);
    }

    pub(crate) fn http_request(&mut self, req: HttpRequest) -> Option<RequestId> {
        let id = self.ids.request()?;
        self.correlations.requests.insert(id);
        self.emit(AgentEffect::Http(HttpEffect::Request { id, request: req }));
        Some(id)
    }

    pub(crate) fn complete_request(&mut self, id: RequestId) {
        self.correlations.requests.remove(&id);
    }

    pub(crate) fn rtc_open(&mut self) -> Option<Generation> {
        self.generation()
    }

    pub(crate) fn rtc_close(&mut self, generation: Generation) {
        self.emit(AgentEffect::Rtc(RtcEffect::CloseTransport { generation }));
    }

    pub(crate) fn data_channel_id(&mut self) -> Option<DataChannelId> {
        let id = self.ids.data_channel()?;
        self.correlations.channels.insert(id);
        Some(id)
    }

    pub(crate) fn forget_data_channel(&mut self, id: DataChannelId) {
        self.correlations.channels.remove(&id);
    }

    pub(crate) fn dc_open(
        &mut self,
        generation: Generation,
        id: DataChannelId,
        cfg: DataChannelConfig,
    ) {
        self.emit(AgentEffect::DataChannel(DataChannelEffect::Open {
            generation,
            id,
            config: cfg,
        }));
    }

    pub(crate) fn accepts(&self, event: &AgentEvent) -> bool {
        self.correlations.accepts(event)
    }
}

#[derive(Default)]
struct Correlations {
    transport: Option<Generation>,
    requests: BTreeSet<RequestId>,
    timers: BTreeSet<TimerId>,
    channels: BTreeSet<DataChannelId>,
}

#[allow(
    dead_code,
    reason = "the session state machine starts transport correlation in the next plan"
)]
impl Correlations {
    fn begin_transport(&mut self, generation: Generation) {
        self.transport = Some(generation);
        self.requests.clear();
        self.timers.clear();
        self.channels.clear();
    }

    fn accepts(&self, event: &AgentEvent) -> bool {
        match event {
            AgentEvent::Rtc(event) => self.transport.is_some_and(|generation| match event {
                RtcEvent::OfferCreated {
                    generation: event, ..
                }
                | RtcEvent::AnswerApplied { generation: event }
                | RtcEvent::Connected { generation: event }
                | RtcEvent::Disconnected { generation: event } => generation == *event,
            }),
            AgentEvent::Http(event) => match event {
                HttpEvent::Response { id, .. } | HttpEvent::Failed { id } => {
                    self.requests.contains(id)
                }
            },
            AgentEvent::Timer(TimerEvent::Fired { id }) => self.timers.contains(id),
            AgentEvent::DataChannel(event) => {
                let (generation, id) = match event {
                    DataChannelEvent::Opened { generation, id }
                    | DataChannelEvent::Message { generation, id, .. }
                    | DataChannelEvent::Closed { generation, id }
                    | DataChannelEvent::WriteFailed { generation, id } => (generation, id),
                };
                self.transport == Some(*generation) && self.channels.contains(id)
            }
        }
    }
}

pub(crate) struct Effects {
    inner: VecDeque<AgentEffect>,
}

#[allow(
    dead_code,
    reason = "later state machines enqueue host effects through this queue"
)]
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests use direct assertions")]

    use super::*;

    #[test]
    fn effects_are_observed_in_emission_order() {
        let mut effects = Effects::new();
        let first = AgentEffect::Rtc(RtcEffect::CloseTransport {
            generation: Generation::new(1),
        });
        let second = AgentEffect::Timer(TimerEffect::Cancel {
            id: TimerId::new(2),
        });

        effects.emit(first.clone());
        effects.emit(second.clone());

        assert_eq!(effects.next_effect(), Some(first));
        assert_eq!(effects.next_effect(), Some(second));
        assert_eq!(effects.next_effect(), None);
    }
}
