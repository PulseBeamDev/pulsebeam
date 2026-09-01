use std::collections::VecDeque;

use agent_core::{
    Agent, AgentCommand, AgentConfig, AgentError, Effect, HostEvent, Notification, Snapshot,
};

pub(crate) enum Input {
    Command(AgentCommand),
    Event(HostEvent),
}

pub(crate) struct Turn {
    pub(crate) effects: Vec<Effect>,
    pub(crate) notifications: Vec<Notification>,
    pub(crate) snapshot: Option<Snapshot>,
    pub(crate) error: Option<AgentError>,
}

pub(crate) struct Driver {
    agent: Agent,
    published_version: Option<u64>,
}

impl Driver {
    pub(crate) fn new(config: AgentConfig) -> Result<Self, AgentError> {
        Ok(Self {
            agent: Agent::new(config)?,
            published_version: None,
        })
    }

    pub(crate) fn snapshot(&self) -> &Snapshot {
        self.agent.snapshot()
    }

    pub(crate) fn turn(&mut self, input: Input) -> Turn {
        let error = match input {
            Input::Command(command) => self.agent.command(command),
            Input::Event(event) => self.agent.handle(event),
        }
        .err();

        let mut effects = Vec::new();
        while let Some(effect) = self.agent.next_effect() {
            effects.push(effect);
        }
        let mut notifications = Vec::new();
        while let Some(notification) = self.agent.next_notification() {
            notifications.push(notification);
        }
        let version = self.agent.snapshot().version;
        let snapshot = if self.published_version == Some(version) {
            None
        } else {
            self.published_version = Some(version);
            Some(self.agent.snapshot().clone())
        };

        Turn {
            effects,
            notifications,
            snapshot,
            error,
        }
    }
}

pub(crate) struct SerialQueue<T> {
    items: VecDeque<T>,
    draining: bool,
}

impl<T> Default for SerialQueue<T> {
    fn default() -> Self {
        Self {
            items: VecDeque::new(),
            draining: false,
        }
    }
}

impl<T> SerialQueue<T> {
    pub(crate) fn push(&mut self, item: T) -> bool {
        self.items.push_back(item);
        if self.draining {
            false
        } else {
            self.draining = true;
            true
        }
    }

    pub(crate) fn pop(&mut self) -> Option<T> {
        self.items.pop_front()
    }

    pub(crate) fn finish(&mut self) {
        debug_assert!(
            self.items.is_empty(),
            "serial queue finished with pending input"
        );
        debug_assert!(self.draining, "serial queue must be draining before finish");
        self.draining = false;
    }

    pub(crate) fn clear(&mut self) {
        self.items.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use agent_core::{
        AgentCommand, AgentConfig, DesiredState, Effect, HostEvent, MediaTopology, RetryPolicy,
        RtcEffect, RtcEvent,
    };

    use super::{Driver, Input, SerialQueue};

    fn config() -> AgentConfig {
        AgentConfig {
            endpoint: "https://sfu.test".into(),
            room_id: "room".into(),
            request_headers: Vec::new(),
            topology: MediaTopology {
                local_video: vec!["camera".into()],
                local_audio: vec!["microphone".into()],
                remote_video: 1,
                remote_audio: 1,
            },
            manual_subscriptions: true,
            retry: RetryPolicy {
                initial_delay: Duration::from_millis(10),
                maximum_delay: Duration::from_secs(1),
                maximum_attempts: 3,
            },
        }
    }

    #[test]
    fn serial_queue_defers_reentrant_input_and_preserves_order() {
        let mut queue = SerialQueue::default();
        assert!(queue.push(1));
        assert_eq!(queue.pop(), Some(1));
        assert!(!queue.push(2));
        assert!(!queue.push(3));
        assert_eq!(queue.pop(), Some(2));
        assert_eq!(queue.pop(), Some(3));
        assert_eq!(queue.pop(), None);
        queue.finish();
        assert!(queue.push(4));
    }

    #[test]
    fn driver_coalesces_unchanged_snapshots_and_drains_effects() {
        let mut driver = Driver::new(config()).unwrap();
        let first = driver.turn(Input::Command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 1,
            connected: true,
            ..DesiredState::default()
        })));
        assert!(first.error.is_none());
        assert!(first.snapshot.is_some());
        let generation = match first.effects.as_slice() {
            [Effect::Rtc(RtcEffect::CreateOffer { generation, .. })] => *generation,
            effects => panic!("expected one create-offer effect, got {effects:?}"),
        };
        assert!(!first.notifications.is_empty());
        assert_eq!(driver.snapshot().desired_revision, 1);

        let stale_close = driver.turn(Input::Event(HostEvent::Rtc(RtcEvent::Closed {
            generation,
        })));
        assert!(stale_close.error.is_none());
        assert!(stale_close.snapshot.is_none());

        let unchanged = driver.turn(Input::Command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 1,
            connected: true,
            ..DesiredState::default()
        })));
        assert!(unchanged.error.is_none());
        assert!(unchanged.effects.is_empty());
        assert!(unchanged.snapshot.is_none());
    }

    #[test]
    fn clearing_a_queue_cancels_deferred_work() {
        let mut queue = SerialQueue::default();
        assert!(queue.push(1));
        assert!(!queue.push(2));
        queue.clear();
        assert_eq!(queue.pop(), None);
        queue.finish();
        assert!(queue.push(3));
    }
}
