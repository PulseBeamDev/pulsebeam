use std::cell::Cell;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;

use futures_channel::oneshot;

use crate::channel::{self, mpsc, mpsc::TrySendError as MpscTrySendError, watch};

use agent_core::{
    Agent, AgentCommand, AgentConfig, AgentError, DesiredState, Effect, Failure, Generation,
    HostEvent, Notification, Snapshot, TopicSend,
};

#[derive(Clone)]
pub(crate) enum Input {
    Command(AgentCommand),
    Event(HostEvent),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum DataSendError {
    #[error(transparent)]
    Admission(#[from] AgentError),
    #[error(transparent)]
    Delivery(#[from] agent_core::DataSendFailure),
    #[error("data send runtime is closed")]
    Closed,
}

type DataSendCompletion = oneshot::Sender<Result<(), DataSendError>>;

#[derive(Default)]
pub(crate) struct Turn {
    pub(crate) effects: Vec<Effect>,
    pub(crate) notifications: Vec<Notification>,
    pub(crate) snapshot: Option<Snapshot>,
    pub(crate) error: Option<TurnError>,
}

pub(crate) struct TurnError {
    pub(crate) source: TurnErrorSource,
    error: AgentError,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TurnErrorSource {
    Command,
    HostEvent,
}

impl fmt::Display for TurnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
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

    pub(crate) fn set_log_level(&mut self, level: agent_core::LogLevel) {
        self.agent.set_log_level(level);
    }

    pub(crate) fn snapshot(&self) -> &Snapshot {
        self.agent.snapshot()
    }

    pub(crate) fn turn(&mut self, input: Input) -> Turn {
        let source = match &input {
            Input::Command(_) => TurnErrorSource::Command,
            Input::Event(_) => TurnErrorSource::HostEvent,
        };
        let error = match input {
            Input::Command(command) => self.agent.command(command),
            Input::Event(event) => self.agent.handle(event),
        }
        .err()
        .map(|error| TurnError { source, error });

        self.finish_turn(error)
    }

    fn host_failed(&mut self, failure: Failure) -> Turn {
        let error = self
            .agent
            .host_failed(failure)
            .err()
            .map(|error| TurnError {
                source: TurnErrorSource::HostEvent,
                error,
            });
        self.finish_turn(error)
    }

    pub(crate) fn send_data(
        &mut self,
        spec: agent_core::TrackSpec,
        payload: Vec<u8>,
    ) -> (Result<agent_core::OperationId, AgentError>, Turn) {
        let result = self.agent.send_data(spec, payload);
        let turn = self.finish_turn(None);
        (result, turn)
    }

    pub(crate) fn resource(
        &mut self,
        command: agent_core::ResourceCommand,
    ) -> (Result<agent_core::ResourceResult, AgentError>, Turn) {
        let result = self.agent.resource(command);
        let turn = self.finish_turn(None);
        (result, turn)
    }

    fn finish_turn(&mut self, error: Option<TurnError>) -> Turn {
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

pub(crate) enum PublicCommand {
    ReplaceDesired(DesiredState),
    RenewAuthorization(String),
    SetConnected(bool),
    Reconnect,
    SetLogLevel(agent_core::LogLevel),
}

impl fmt::Debug for PublicCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReplaceDesired(desired) => formatter
                .debug_tuple("ReplaceDesired")
                .field(desired)
                .finish(),
            Self::RenewAuthorization(_) => formatter.write_str("RenewAuthorization([REDACTED])"),
            Self::SetConnected(connected) => formatter
                .debug_tuple("SetConnected")
                .field(connected)
                .finish(),
            Self::Reconnect => formatter.write_str("Reconnect"),
            Self::SetLogLevel(level) => formatter.debug_tuple("SetLogLevel").field(level).finish(),
        }
    }
}

#[derive(Debug)]
pub(crate) enum LifecycleCommand {
    Close,
    Abort,
}

#[derive(Debug)]
pub(crate) enum Message {
    Public(PublicCommand),
    Topic {
        send: TopicSend,
        expected_generation: u64,
        expected_data_track_id: String,
        completion: oneshot::Sender<Result<(), String>>,
    },
    DataSend {
        spec: agent_core::TrackSpec,
        payload: Vec<u8>,
        completion: DataSendCompletion,
    },
    Resource {
        command: agent_core::ResourceCommand,
        completion: oneshot::Sender<Result<agent_core::ResourceResult, AgentError>>,
    },
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

    pub(crate) fn send_topic(
        &self,
        send: TopicSend,
        expected_generation: u64,
        expected_data_track_id: String,
    ) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        let (completion, receiver) = oneshot::channel();
        self.send(Message::Topic {
            send,
            expected_generation,
            expected_data_track_id,
            completion,
        })?;
        Ok(receiver)
    }

    pub(crate) fn send_data(
        &self,
        spec: agent_core::TrackSpec,
        payload: Vec<u8>,
    ) -> Result<oneshot::Receiver<Result<(), DataSendError>>, String> {
        let (completion, receiver) = oneshot::channel();
        self.send(Message::DataSend {
            spec,
            payload,
            completion,
        })?;
        Ok(receiver)
    }

    pub(crate) async fn send_resource(
        &self,
        command: agent_core::ResourceCommand,
    ) -> Result<oneshot::Receiver<Result<agent_core::ResourceResult, AgentError>>, String> {
        if self.closed.get() {
            return Err("actor is not running".to_owned());
        }
        let (completion, receiver) = oneshot::channel();
        self.sender
            .send(Message::Resource {
                command,
                completion,
            })
            .await
            .map_err(|_| "actor is not running".to_owned())?;
        Ok(receiver)
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
    last_snapshot: Option<Snapshot>,
    data_sends: BTreeMap<agent_core::OperationId, DataSendCompletion>,
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
            last_snapshot: None,
            data_sends: BTreeMap::new(),
        })
    }

    fn set_connected(&mut self, connected: bool) {
        let mut desired = self.desired.clone();
        if desired.connected == connected {
            if connected {
                self.turn(Input::Command(AgentCommand::Connect));
            }
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

    fn send_topic(
        &mut self,
        send: TopicSend,
        expected_generation: u64,
        expected_data_track_id: &str,
    ) -> Result<(), String> {
        let snapshot = self.driver.snapshot();
        if snapshot.generation.map(Generation::get) != Some(expected_generation) {
            return Err("topic publication generation was replaced".to_owned());
        }
        let binding = snapshot
            .topics
            .publishers
            .iter()
            .find(|publisher| publisher.registration == send.publisher)
            .and_then(|publisher| publisher.data_track_id.as_deref());
        if binding != Some(expected_data_track_id) {
            return Err("topic publication binding was replaced".to_owned());
        }
        let turn = self
            .driver
            .turn(Input::Command(AgentCommand::SendTopic(send)));
        let result = turn
            .error
            .as_ref()
            .map_or(Ok(()), |error| Err(error.to_string()));
        self.publish_turn(turn);
        result
    }

    fn event(&mut self, event: HostEvent) {
        self.turn(Input::Event(event));
    }

    fn host_failure(&mut self, failure: Failure) {
        self.host.host_failed(&failure);
        let turn = self.driver.host_failed(failure);
        self.publish_turn(turn);
    }

    fn turn(&mut self, input: Input) {
        if self.closed {
            return;
        }

        let turn = self.driver.turn(input);
        self.publish_turn(turn);
    }

    fn publish_turn(&mut self, mut turn: Turn) {
        let snapshot = if let Some(snapshot) = turn.snapshot.clone() {
            snapshot
        } else {
            self.driver.snapshot().clone()
        };
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

        for notification in &turn.notifications {
            if let Notification::DataSendFinished { send, error } = notification
                && let Some(completion) = self.data_sends.remove(send)
            {
                let _ = completion
                    .send(error.map_or(Ok(()), |error| Err(DataSendError::Delivery(error))));
            }
        }

        for effect in turn.effects.drain(..) {
            self.host.execute_effect(effect);
        }

        if !turn.notifications.is_empty() || turn.error.is_some() {
            self.host.publish_turn(&turn);
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
        for (_, completion) in std::mem::take(&mut self.data_sends) {
            let _ = completion.send(Err(DataSendError::Closed));
        }
        self.host.shutdown();
        self.snapshots.close();
        if self.last_snapshot.is_some() {
            self.last_snapshot = None;
        }
    }

    fn process(&mut self, message: Message) {
        if self.closed {
            if let Message::DataSend { completion, .. } = message {
                let _ = completion.send(Err(DataSendError::Closed));
            }
            return;
        }

        match message {
            Message::Public(command) => match command {
                PublicCommand::ReplaceDesired(desired) => self.replace_desired(desired),
                PublicCommand::RenewAuthorization(token) => {
                    self.turn(Input::Command(AgentCommand::RenewAuthorization(token)));
                }
                PublicCommand::SetConnected(connected) => self.set_connected(connected),
                PublicCommand::Reconnect => self.turn(Input::Command(AgentCommand::Reconnect)),
                PublicCommand::SetLogLevel(level) => self.driver.set_log_level(level),
            },
            Message::Topic {
                send,
                expected_generation,
                expected_data_track_id,
                completion,
            } => {
                let _ = completion.send(self.send_topic(
                    send,
                    expected_generation,
                    &expected_data_track_id,
                ));
            }
            Message::DataSend {
                spec,
                payload,
                completion,
            } => {
                let (result, turn) = self.driver.send_data(spec, payload);
                match result {
                    Ok(ticket) => {
                        self.data_sends.insert(ticket, completion);
                    }
                    Err(error) => {
                        let _ = completion.send(Err(DataSendError::Admission(error)));
                    }
                }
                self.publish_turn(turn);
            }
            Message::Resource {
                command,
                completion,
            } => {
                let (result, turn) = self.driver.resource(command);
                self.publish_turn(turn);
                let _ = completion.send(result);
            }
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
    use futures_channel::oneshot;
    use std::cell::RefCell;

    use super::{
        Actor, Driver, Host, Input, LifecycleCommand, Message, PublicCommand, SerialQueue,
        TurnErrorSource,
    };
    use agent_core::{
        AgentCommand, ChannelId, ConnectionState, DesiredState, Effect, Failure, FailureClass,
        HostEvent, OfferResources, RtcEffect, RtcEvent, TopicMode, TopicPublisher, TopicSend,
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
            token: "token".into(),
            topology: agent_core::MediaTopology {
                local_video: 1,
                local_audio: 1,
                remote_video: 1,
                remote_audio: 1,
            },
            retry: agent_core::RetryPolicy::default(),
            log_level: agent_core::LogLevel::default(),
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
    fn driver_marks_host_rejections_separately_from_command_rejections() {
        let mut driver = Driver::new(config()).unwrap();
        let opening = driver.turn(Input::Command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 1,
            connected: true,
            ..DesiredState::default()
        })));
        let generation = match opening.effects.as_slice() {
            [Effect::Rtc(RtcEffect::CreateOffer { generation, .. })] => *generation,
            effects => panic!("expected one create-offer effect, got {effects:?}"),
        };

        let rejected = driver.turn(Input::Event(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation,
            offer: "offer".into(),
            resources: OfferResources {
                slots: Vec::new(),
                signaling_channel: ChannelId::new(1).unwrap(),
                data_channels: Vec::new(),
            },
        })));

        assert_eq!(
            rejected.error.as_ref().map(|error| error.source),
            Some(TurnErrorSource::HostEvent)
        );
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
        let expected_generation = actor.driver.snapshot().generation.unwrap().get();
        for generation in [expected_generation, expected_generation.saturating_add(1)] {
            let before = actor.driver.snapshot().clone();
            let (completion, mut receiver) = futures_channel::oneshot::channel();
            actor.process(Message::Topic {
                send: TopicSend {
                    publisher: TopicPublisher {
                        topic: "chat".into(),
                        mode: TopicMode::Latest,
                    },
                    payload: vec![1, 2, 3],
                },
                expected_generation: generation,
                expected_data_track_id: "unbound-track".to_owned(),
                completion,
            });
            assert!(receiver.try_recv().unwrap().unwrap().is_err());
            assert_eq!(actor.driver.snapshot(), &before);
        }

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
        assert!(after.version > before_version);
        assert_eq!(
            actor.driver.snapshot().connection,
            ConnectionState::TerminalFailure
        );

        actor.process(Message::HostFailure(Failure {
            class: FailureClass::Protocol,
            message: "boom".into(),
        }));
        let final_snapshot = actor.published_snapshot().unwrap();
        assert_eq!(final_snapshot.version, after.version);
        assert_eq!(final_snapshot.terminal_failure.unwrap().message, "boom");
    }

    #[test]
    fn host_failure_rejects_owned_sends_and_explicit_connect_retries_core_state() {
        let mut actor = Actor::new(config(), FakeHost::default()).unwrap();
        actor.process(Message::Public(PublicCommand::SetConnected(true)));
        let (completion, mut pending) = oneshot::channel();
        actor.process(Message::DataSend {
            spec: agent_core::TrackSpec::Data {
                label: "pending".into(),
                delivery: agent_core::TopicMode::Lossy,
            },
            payload: vec![0, 255],
            completion,
        });
        assert!(pending.try_recv().unwrap().is_none());
        actor.process(Message::HostFailure(Failure {
            class: FailureClass::Protocol,
            message: "boom".into(),
        }));
        assert!(matches!(
            pending.try_recv().unwrap(),
            Some(Err(super::DataSendError::Delivery(
                agent_core::DataSendFailure::TerminalFailure
            )))
        ));
        assert_eq!(
            actor.driver.snapshot().connection,
            ConnectionState::TerminalFailure
        );
        actor.process(Message::Public(PublicCommand::SetConnected(true)));
        assert!(actor.driver.snapshot().terminal_failure.is_none());
        assert!(matches!(
            actor.driver.snapshot().connection,
            ConnectionState::CreatingOffer
        ));
        assert!(actor.data_sends.is_empty());
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
