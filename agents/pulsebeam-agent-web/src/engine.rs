use std::cell::Cell;
use std::collections::VecDeque;

use crate::channel::{self, mpsc, mpsc::TrySendError as MpscTrySendError, watch};

use agent_core::{
    Agent, AgentCommand, AgentConfig, AgentError, DesiredState, Effect, Failure, FailureClass,
    HostEvent, Notification, Snapshot, TopicSend,
};

#[derive(Clone)]
pub(crate) enum Input {
    Command(AgentCommand),
    Event(HostEvent),
}

#[derive(Default)]
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

pub(crate) trait Host {
    fn publish_turn(&self, turn: &Turn);
    fn execute_effect(&self, effect: Effect);
    fn host_failed(&self, failure: &Failure);
    fn shutdown(&self);
}

#[derive(Debug)]
pub(crate) enum PublicCommand {
    ReplaceDesired(DesiredState),
    SetConnected(bool),
}

#[derive(Debug)]
pub(crate) enum TopicCommand {
    SendTopic(TopicSend),
}

#[derive(Debug)]
pub(crate) enum LifecycleCommand {
    Close,
    Abort,
}

#[derive(Debug)]
pub(crate) enum Message {
    Public(PublicCommand),
    Topic(TopicCommand),
    HostEvent(HostEvent),
    Lifecycle(LifecycleCommand),
    HostFailure(Failure),
}

pub(crate) struct ActorHandle {
    sender: mpsc::Sender<Message>,
    snapshots: watch::Receiver<Snapshot>,
    closed: Cell<bool>,
}

impl Clone for ActorHandle {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            snapshots: self.snapshots.clone(),
            closed: Cell::new(self.closed.get()),
        }
    }
}

impl ActorHandle {
    pub(crate) fn snapshot(&self) -> Snapshot {
        self.snapshots.current()
    }

    pub(crate) fn snapshots(&self) -> watch::Receiver<Snapshot> {
        self.snapshots.clone()
    }

    pub(crate) fn send_public(&self, message: PublicCommand) -> Result<(), String> {
        self.send(Message::Public(message))
    }

    pub(crate) fn send_topic(&self, message: TopicCommand) -> Result<(), String> {
        self.send(Message::Topic(message))
    }

    pub(crate) fn send_host_event(&self, event: HostEvent) -> Result<(), String> {
        self.send(Message::HostEvent(event))
    }

    pub(crate) fn request_close(&self) -> Result<(), String> {
        self.send(Message::Lifecycle(LifecycleCommand::Close))
    }

    pub(crate) fn abort(&self) -> Result<(), String> {
        if self.closed.get() {
            return Ok(());
        }
        self.closed.set(true);
        self.send(Message::Lifecycle(LifecycleCommand::Abort))
    }

    fn send(&self, message: Message) -> Result<(), String> {
        self.sender.try_send(message).map_err(|error| match error {
            MpscTrySendError::Full(_) => "actor mailbox is full".to_owned(),
            MpscTrySendError::Closed(_) => "actor is not running".to_owned(),
        })
    }
}

pub(crate) struct Actor<H: Host> {
    driver: Driver,
    host: H,
    desired: DesiredState,
    next_revision: u64,
    snapshots: watch::Sender<Snapshot>,
    running: bool,
    closed: bool,
    failure: Option<Failure>,
    last_snapshot: Option<Snapshot>,
}

impl<H: Host> Actor<H> {
    pub(crate) fn new(config: AgentConfig, host: H) -> Result<Self, AgentError> {
        let driver = Driver::new(config)?;
        let initial_snapshot = driver.snapshot().clone();
        let (snapshot_tx, _) = channel::watch::channel(initial_snapshot);
        let snapshots = snapshot_tx;

        Ok(Self {
            driver,
            host,
            desired: DesiredState::default(),
            next_revision: 1,
            snapshots,
            running: true,
            closed: false,
            failure: None,
            last_snapshot: None,
        })
    }

    fn set_connected(&mut self, connected: bool) {
        let mut desired = self.desired.clone();
        if desired.connected == connected {
            return;
        }
        desired.connected = connected;
        self.replace_desired(desired);
    }

    fn replace_desired(&mut self, mut desired: DesiredState) {
        desired.revision = self.next_revision;
        self.next_revision = self.next_revision.saturating_add(1);
        self.desired = desired.clone();
        self.turn(Input::Command(AgentCommand::ReplaceDesired(desired)));
    }

    fn send_topic(&mut self, send: TopicSend) {
        self.turn(Input::Command(AgentCommand::SendTopic(send)));
    }

    fn event(&mut self, event: HostEvent) {
        self.turn(Input::Event(event));
    }

    fn host_failure(&mut self, failure: Failure) {
        let changed = self.failure.as_ref() != Some(&failure);
        self.failure = Some(failure);
        self.host.host_failed(self.failure.as_ref().unwrap());
        if changed {
            self.publish_snapshot(true);
        }
    }

    fn turn(&mut self, input: Input) {
        if self.closed {
            return;
        }

        let mut turn = self.driver.turn(input);
        let mut snapshot = if let Some(snapshot) = turn.snapshot.clone() {
            snapshot
        } else {
            self.driver.snapshot().clone()
        };
        if let Some(failure) = self.failure.as_ref() {
            snapshot.terminal_failure = Some(failure.clone());
        }

        let should_publish_snapshot = self
            .last_snapshot
            .as_ref()
            .is_none_or(|previous| *previous != snapshot);
        if should_publish_snapshot {
            let published = self.snapshots.send(snapshot.clone());
            if published {
                self.last_snapshot = Some(snapshot);
            }
            if !published {
                self.shutdown();
            }
        }

        for effect in turn.effects.drain(..) {
            self.host.execute_effect(effect);
        }

        if !turn.notifications.is_empty() || turn.error.is_some() {
            self.host.publish_turn(&turn);
        }
    }

    fn publish_snapshot(&mut self, force: bool) {
        if self.closed {
            return;
        }

        let mut snapshot = self.driver.snapshot().clone();
        if let Some(failure) = self.failure.as_ref() {
            snapshot.terminal_failure = Some(failure.clone());
        }

        let should_publish = force
            || self
                .last_snapshot
                .as_ref()
                .is_none_or(|previous| *previous != snapshot);
        if should_publish {
            let published = self.snapshots.send(snapshot.clone());
            if published {
                self.last_snapshot = Some(snapshot);
            }
            if !published {
                self.shutdown();
            }
        }
    }

    fn close(&mut self) {
        if self.closed || !self.running {
            return;
        }

        self.running = false;
        self.set_connected(false);
    }

    fn shutdown(&mut self) {
        if self.closed {
            return;
        }

        self.closed = true;
        self.running = false;
        self.host.shutdown();
        self.snapshots.close();
        if self.last_snapshot.is_some() {
            self.last_snapshot = None;
        }
    }

    fn process(&mut self, message: Message) {
        if self.closed {
            return;
        }

        match message {
            Message::Public(command) => match command {
                PublicCommand::ReplaceDesired(desired) => self.replace_desired(desired),
                PublicCommand::SetConnected(connected) => self.set_connected(connected),
            },
            Message::Topic(topic) => match topic {
                TopicCommand::SendTopic(send) => self.send_topic(send),
            },
            Message::HostEvent(event) => self.event(event),
            Message::Lifecycle(lifecycle) => match lifecycle {
                LifecycleCommand::Close => self.close(),
                LifecycleCommand::Abort => self.shutdown(),
            },
            Message::HostFailure(failure) => self.host_failure(failure),
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn spawn_actor<H: Host + 'static>(
    config: AgentConfig,
    host: H,
) -> Result<ActorHandle, AgentError> {
    let mut actor = Actor::new(config.clone(), host)?;
    let (message_tx, mut message_rx) = channel::mpsc::channel(32);
    let initial_snapshot = actor.driver.snapshot().clone();
    let (snapshot_tx, snapshot_rx) = channel::watch::channel(initial_snapshot);

    actor.snapshots = snapshot_tx;

    wasm_bindgen_futures::spawn_local(async move {
        while let Some(message) = message_rx.recv().await {
            actor.process(message);
        }
        actor.shutdown();
    });

    Ok(ActorHandle {
        sender: message_tx,
        snapshots: snapshot_rx,
        closed: Cell::new(false),
    })
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
    use std::cell::RefCell;

    use super::{
        Actor, Driver, FailureClass, Host, Input, LifecycleCommand, Message, PublicCommand,
        SerialQueue, TopicCommand,
    };
    use agent_core::{
        AgentCommand, DesiredState, Effect, Failure, HostEvent, RtcEffect, RtcEvent, TopicMode,
        TopicPublisher, TopicSend,
    };

    #[derive(Default)]
    struct FakeHost {
        turns: RefCell<Vec<String>>,
        effects: RefCell<Vec<agent_core::Effect>>,
        failures: RefCell<Vec<FailureClass>>,
        shutdowns: RefCell<u8>,
    }

    impl FakeHost {
        fn effect_events(&self) -> Vec<agent_core::Effect> {
            self.effects.borrow().clone()
        }
    }

    impl Host for FakeHost {
        fn publish_turn(&self, turn: &super::Turn) {
            self.turns.borrow_mut().push(format!(
                "{}:{}:{:?}",
                turn.snapshot.is_some(),
                turn.notifications.len(),
                turn.error.as_ref().map(|error| format!("{error}"))
            ));
        }

        fn execute_effect(&self, effect: agent_core::Effect) {
            self.effects.borrow_mut().push(effect);
        }

        fn host_failed(&self, failure: &Failure) {
            self.failures.borrow_mut().push(failure.class);
            self.turns
                .borrow_mut()
                .push(format!("host-failure:{}", failure.message));
        }

        fn shutdown(&self) {
            let mut shutdowns = self.shutdowns.borrow_mut();
            *shutdowns = shutdowns.saturating_add(1);
            self.turns.borrow_mut().push("shutdown".to_owned());
        }
    }

    fn config() -> agent_core::AgentConfig {
        agent_core::AgentConfig {
            endpoint: "https://example.com".into(),
            room_id: "room".into(),
            request_headers: vec![agent_core::HttpHeader {
                name: "x-test".into(),
                value: "unit".into(),
            }],
            topology: agent_core::MediaTopology {
                local_video: vec!["cam".into()],
                local_audio: vec!["mic".into()],
                remote_video: 1,
                remote_audio: 1,
            },
            manual_subscriptions: true,
            retry: agent_core::RetryPolicy::default(),
        }
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
    fn driver_tracks_complete_desired_revisions() {
        let host = FakeHost::default();
        let mut actor = Actor::new(config(), host).unwrap();

        let mut desired = DesiredState::default();
        desired.connected = true;
        actor.process(Message::Public(PublicCommand::ReplaceDesired(
            desired.clone(),
        )));
        desired.revision = 1;
        assert_eq!(actor.driver.snapshot().desired_revision, 1);

        actor.process(Message::Public(PublicCommand::ReplaceDesired(desired)));
        assert_eq!(actor.driver.snapshot().desired_revision, 2);
    }

    #[test]
    fn events_and_commands_are_serialized_and_effects_dispatched() {
        let host = FakeHost::default();
        let mut actor = Actor::new(config(), host).unwrap();

        actor.process(Message::Public(PublicCommand::SetConnected(true)));
        actor.process(Message::Topic(TopicCommand::SendTopic(TopicSend {
            publisher: TopicPublisher {
                topic: "chat".into(),
                mode: TopicMode::Latest,
            },
            payload: vec![1, 2, 3],
        })));

        let effects = actor.host.effect_events();
        assert!(!effects.is_empty());
        let generation = effects
            .into_iter()
            .find_map(|effect| match effect {
                agent_core::Effect::Rtc(agent_core::RtcEffect::CreateOffer {
                    generation, ..
                }) => Some(generation),
                _ => None,
            })
            .expect("connection effect emitted");

        actor.process(Message::HostEvent(HostEvent::Rtc(RtcEvent::Disconnected {
            generation,
        })));

        let after = actor.host.effect_events();
        assert!(after.len() > 1);
    }

    #[test]
    fn host_failure_becomes_snapshot_failure_and_is_deduped() {
        let host = FakeHost::default();
        let mut actor = Actor::new(config(), host).unwrap();

        let before_version = actor.driver.snapshot().version;
        assert!(actor.published_snapshot().is_none());

        actor.process(Message::HostFailure(Failure {
            class: FailureClass::Protocol,
            message: "boom".into(),
        }));

        let after = actor.published_snapshot().unwrap();
        assert_eq!(after.terminal_failure.unwrap().message, "boom");
        assert_eq!(after.version, before_version);

        actor.process(Message::HostFailure(Failure {
            class: FailureClass::Protocol,
            message: "boom".into(),
        }));
        let final_failure = actor
            .published_snapshot()
            .unwrap()
            .terminal_failure
            .expect("failure should remain");
        assert_eq!(final_failure.message, "boom");
    }

    #[test]
    fn duplicate_public_failures_are_not_republished() {
        let host = FakeHost::default();
        let mut actor = Actor::new(config(), host).unwrap();

        actor.process(Message::HostFailure(Failure {
            class: FailureClass::Protocol,
            message: "boom".into(),
        }));
        let first_version = actor.published_snapshot().unwrap().version;

        actor.process(Message::HostFailure(Failure {
            class: FailureClass::Protocol,
            message: "boom".into(),
        }));
        let second_version = actor.published_snapshot().unwrap().version;
        assert_eq!(first_version, second_version);
    }

    #[test]
    fn lifecycle_messages_are_idempotent() {
        let host = FakeHost::default();
        let mut actor = Actor::new(config(), host).unwrap();

        actor.process(Message::Lifecycle(LifecycleCommand::Close));
        actor.process(Message::Lifecycle(LifecycleCommand::Close));
        actor.process(Message::Lifecycle(LifecycleCommand::Abort));
        actor.process(Message::Lifecycle(LifecycleCommand::Abort));

        let host = &actor.host;
        assert_eq!(*host.shutdowns.borrow(), 1);
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
    fn clearing_a_queue_clears_deferred_work() {
        let mut queue = SerialQueue::default();
        assert!(queue.push(1));
        assert!(!queue.push(2));
        queue.clear();
        assert_eq!(queue.pop(), None);
        queue.finish();
        assert!(queue.push(3));
    }

    impl<H: Host> Actor<H> {
        fn published_snapshot(&self) -> Option<agent_core::Snapshot> {
            self.last_snapshot.clone()
        }
    }
}
