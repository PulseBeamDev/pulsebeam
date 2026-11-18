use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::rc::Rc;

use futures_channel::oneshot;

use crate::channel::{self, mpsc, mpsc::TrySendError as MpscTrySendError, watch};

use agent_core::{
    Agent, AgentCommand, AgentConfig, AgentError, DesiredState, Effect, Failure, Generation,
    HostEvent, Notification, Snapshot, TimerEffect, TimerEvent, TimerId, TopicSend,
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
    logs: Vec<agent_core::DeferredLog>,
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

struct Control {
    agent: RefCell<Agent>,
    accepting: Cell<bool>,
    wake_pending: Cell<bool>,
}

pub(crate) struct Driver {
    control: Rc<Control>,
    published_version: Option<u64>,
}

impl Driver {
    pub(crate) fn new(config: AgentConfig) -> Result<Self, AgentError> {
        let mut agent = Agent::new(config)?;
        agent.defer_logs();
        Ok(Self {
            control: Rc::new(Control {
                agent: RefCell::new(agent),
                accepting: Cell::new(true),
                wake_pending: Cell::new(false),
            }),
            published_version: None,
        })
    }

    pub(crate) fn set_log_level(&mut self, level: agent_core::LogLevel) {
        self.control.agent.borrow_mut().set_log_level(level);
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        self.control.agent.borrow().snapshot().clone()
    }

    pub(crate) fn turn(&mut self, input: Input) -> Turn {
        let source = match &input {
            Input::Command(_) => TurnErrorSource::Command,
            Input::Event(_) => TurnErrorSource::HostEvent,
        };
        let error = match input {
            Input::Command(command) => self.control.agent.borrow_mut().command(command),
            Input::Event(event) => self.control.agent.borrow_mut().handle(event),
        }
        .err()
        .map(|error| TurnError { source, error });

        self.finish_turn(error)
    }

    fn host_failed(&mut self, failure: Failure) -> Turn {
        let error = self
            .control
            .agent
            .borrow_mut()
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
        let result = self.control.agent.borrow_mut().send_data(spec, payload);
        let turn = self.finish_turn(None);
        (result, turn)
    }

    pub(crate) fn resource(
        &mut self,
        command: agent_core::ResourceCommand,
    ) -> (Result<agent_core::ResourceResult, AgentError>, Turn) {
        let result = self.control.agent.borrow_mut().resource(command);
        let turn = self.finish_turn(None);
        (result, turn)
    }

    fn finish_turn(&mut self, error: Option<TurnError>) -> Turn {
        let mut agent = self.control.agent.borrow_mut();
        let mut effects = Vec::new();
        while let Some(effect) = agent.next_effect() {
            effects.push(effect);
        }

        let mut notifications = Vec::new();
        while let Some(notification) = agent.next_notification() {
            notifications.push(notification);
        }

        let version = agent.snapshot().version;
        let snapshot = if self.published_version == Some(version) {
            None
        } else {
            self.published_version = Some(version);
            Some(agent.snapshot().clone())
        };

        Turn {
            effects,
            notifications,
            snapshot,
            error,
            logs: agent.drain_logs(),
        }
    }
}

pub(crate) trait Host {
    fn publish_turn(&self, turn: &Turn);
    fn execute_effect(&self, effect: Effect);
    fn host_failed(&self, failure: &Failure);
    fn shutdown(&self);
    fn publication_claimed(
        &self,
        _owner: u64,
        _slot: &str,
        _kind: agent_core::MediaKind,
        _after_generation: Option<Generation>,
    ) {
    }
    fn resource_released(&self, _owner: u64) {}
    fn publication_input_state(&self, _state: &agent_core::PublicationInputState) {}
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
    CoreChanged,
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
    control: Rc<Control>,
    sender: mpsc::Sender<Message>,
    snapshots: watch::Receiver<Snapshot>,
    closed: Cell<bool>,
}

impl Clone for ActorHandle {
    fn clone(&self) -> Self {
        Self {
            control: Rc::clone(&self.control),
            sender: self.sender.clone(),
            snapshots: self.snapshots.clone(),
            closed: Cell::new(self.closed.get()),
        }
    }
}

impl ActorHandle {
    pub(crate) fn snapshot(&self) -> Snapshot {
        self.control.agent.borrow().snapshot().clone()
    }

    pub(crate) fn join(&self, room: String) -> Result<(), String> {
        self.intent(AgentCommand::Join(room))
    }

    pub(crate) fn renew(&self, token: String) -> Result<(), String> {
        self.intent(AgentCommand::RenewAuthorization(token))
    }

    fn intent(&self, command: AgentCommand) -> Result<(), String> {
        if !self.control.accepting.get() || self.sender.is_closed() {
            return Err("actor is not running".into());
        }
        self.control
            .agent
            .borrow_mut()
            .command(command)
            .map_err(|error| error.to_string())?;
        if !self.control.wake_pending.replace(true) {
            match self.sender.try_send(Message::CoreChanged) {
                Ok(()) => {}
                // Every queued message also flushes pending core work.
                Err(MpscTrySendError::Full(_)) => self.control.wake_pending.set(false),
                Err(MpscTrySendError::Closed(_)) => {
                    self.control.wake_pending.set(false);
                    return Err("actor is not running".into());
                }
            }
        }
        Ok(())
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
        self.control.accepting.set(false);
        self.send(Message::Lifecycle(LifecycleCommand::Close))
    }

    pub(crate) fn abort(&self) -> Result<(), String> {
        self.control.accepting.set(false);
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
    snapshots: watch::Sender<Snapshot>,
    running: bool,
    closed: bool,
    last_snapshot: Option<Snapshot>,
    data_sends: BTreeMap<agent_core::OperationId, DataSendCompletion>,
    core_wake: Option<TimerId>,
}

impl<H: Host> Actor<H> {
    pub(crate) fn new(config: AgentConfig, host: H) -> Result<Self, AgentError> {
        let driver = Driver::new(config)?;
        let initial_snapshot = driver.snapshot();
        let (snapshot_tx, _) = channel::watch::channel(initial_snapshot);
        let snapshots = snapshot_tx;

        Ok(Self {
            driver,
            host,
            snapshots,
            running: true,
            closed: false,
            last_snapshot: None,
            data_sends: BTreeMap::new(),
            core_wake: None,
        })
    }

    fn handle(&mut self, sender: mpsc::Sender<Message>) -> ActorHandle {
        let (snapshots, receiver) = channel::watch::channel(self.driver.snapshot());
        self.snapshots = snapshots;
        ActorHandle {
            control: Rc::clone(&self.driver.control),
            sender,
            snapshots: receiver,
            closed: Cell::new(false),
        }
    }

    fn set_connected(&mut self, connected: bool) {
        self.turn(Input::Command(if connected {
            AgentCommand::Connect
        } else {
            AgentCommand::Disconnect
        }));
    }

    fn replace_desired(&mut self, mut desired: DesiredState) {
        desired.revision = self.driver.snapshot().desired_revision.saturating_add(1);
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
            self.flush_core();
            return Err("topic publication generation was replaced".to_owned());
        }
        let binding = snapshot
            .topics
            .publishers
            .iter()
            .find(|publisher| publisher.registration == send.publisher)
            .and_then(|publisher| publisher.data_track_id.as_deref());
        if binding != Some(expected_data_track_id) {
            self.flush_core();
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

    fn flush_core(&mut self) {
        let turn = self.driver.finish_turn(None);
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
            self.driver.snapshot()
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
            match effect {
                Effect::Timer(TimerEffect::Schedule { timer, after }) if after.is_zero() => {
                    self.core_wake = Some(timer);
                }
                Effect::Timer(TimerEffect::Cancel { timer }) => {
                    if self.core_wake == Some(timer) {
                        self.core_wake = None;
                    } else {
                        self.host
                            .execute_effect(Effect::Timer(TimerEffect::Cancel { timer }));
                    }
                }
                effect => self.host.execute_effect(effect),
            }
        }

        if !turn.notifications.is_empty() || turn.error.is_some() {
            self.host.publish_turn(&turn);
        }
        for record in turn.logs {
            record.emit();
        }
    }

    fn wake_core(&mut self) -> bool {
        if let Some(timer) = self.core_wake.take() {
            self.event(HostEvent::Timer(TimerEvent::Fired { timer }));
            true
        } else {
            false
        }
    }

    fn close(&mut self) {
        if self.closed || !self.running {
            return;
        }

        self.driver.control.accepting.set(false);
        self.running = false;
        self.set_connected(false);
    }

    fn shutdown(&mut self) {
        if self.closed {
            return;
        }

        self.driver.control.accepting.set(false);
        self.closed = true;
        self.running = false;
        self.core_wake = None;
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
            Message::CoreChanged => {
                self.driver.control.wake_pending.set(false);
                self.flush_core();
            }
            Message::Public(command) => match command {
                PublicCommand::ReplaceDesired(desired) => self.replace_desired(desired),
                PublicCommand::RenewAuthorization(token) => {
                    self.turn(Input::Command(AgentCommand::RenewAuthorization(token)));
                }
                PublicCommand::SetConnected(connected) => self.set_connected(connected),
                PublicCommand::Reconnect => self.turn(Input::Command(AgentCommand::Reconnect)),
                PublicCommand::SetLogLevel(level) => {
                    self.driver.set_log_level(level);
                    self.flush_core();
                }
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
                let release = match &command {
                    agent_core::ResourceCommand::Release { owner } => Some(*owner),
                    _ => None,
                };
                let publication_kind = match &command {
                    agent_core::ResourceCommand::ClaimPublication { spec } => spec.media_kind(),
                    _ => None,
                };
                let (result, turn) = self.driver.resource(command);
                if let (
                    Some(kind),
                    Ok(agent_core::ResourceResult::Publication {
                        owner,
                        slot,
                        after_generation,
                    }),
                ) = (publication_kind, &result)
                {
                    self.host
                        .publication_claimed(*owner, slot, kind, *after_generation);
                }
                if let Ok(
                    agent_core::ResourceResult::PublicationInput { state, .. }
                    | agent_core::ResourceResult::PublicationInputState(state),
                ) = &result
                {
                    self.host.publication_input_state(state);
                }
                if let Some(owner) = release {
                    self.host.resource_released(owner);
                }
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

#[cfg(any(test, target_arch = "wasm32"))]
async fn run_actor<H: Host>(mut actor: Actor<H>, message_rx: mpsc::Receiver<Message>) -> Actor<H> {
    loop {
        if actor.wake_core() && actor.core_wake.is_some() {
            // A rearmed wake must not monopolize the actor or the executor.
            let mut yielded = false;
            std::future::poll_fn(|cx| {
                if yielded {
                    std::task::Poll::Ready(())
                } else {
                    yielded = true;
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            })
            .await;
            if let Some(message) = message_rx.try_recv() {
                actor.process(message);
            }
            continue;
        }
        let Some(message) = message_rx.recv().await else {
            break;
        };
        actor.process(message);
    }
    actor.shutdown();
    actor
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn spawn_actor<H: Host + 'static>(
    config: AgentConfig,
    host: H,
) -> Result<ActorHandle, AgentError> {
    let mut actor = Actor::new(config.clone(), host)?;
    let (message_tx, message_rx) = channel::mpsc::channel(32);
    let handle = actor.handle(message_tx);

    wasm_bindgen_futures::spawn_local(async move {
        run_actor(actor, message_rx).await;
    });

    Ok(handle)
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
        on_turn: RefCell<Option<Box<dyn Fn()>>>,
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
            if let Some(callback) = self.on_turn.borrow().as_ref() {
                callback();
            }
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
            retry: agent_core::RetryPolicy::default(),
            log_level: agent_core::LogLevel::default(),
        }
    }

    fn retry_timer() -> (agent_core::TimerId, agent_core::Generation) {
        let mut driver = Driver::new(config()).unwrap();
        let turn = driver.turn(Input::Command(AgentCommand::Join("room".into())));
        let Effect::Rtc(RtcEffect::CreateOffer { generation, .. }) = turn.effects[0] else {
            panic!("expected offer");
        };
        let turn = driver.turn(Input::Event(HostEvent::Rtc(RtcEvent::Failed {
            generation,
            message: "offline".into(),
        })));
        let timer = turn
            .effects
            .into_iter()
            .find_map(|effect| match effect {
                Effect::Timer(agent_core::TimerEffect::Schedule { timer, .. }) => Some(timer),
                _ => None,
            })
            .unwrap();
        (timer, generation)
    }

    #[test]
    fn intent_commits_without_actor_progress_and_wakes_through_a_full_mailbox() {
        let mut actor = Actor::new(config(), FakeHost::default()).unwrap();
        let (sender, receiver) = super::channel::mpsc::channel(1);
        let handle = actor.handle(sender);
        let before = handle.snapshot();
        assert!(handle.join("bad\0room".into()).is_err());
        assert_eq!(handle.snapshot(), before);
        assert!(handle.renew(String::new()).is_err());
        assert_eq!(handle.snapshot(), before);
        handle.renew("new-token".into()).unwrap();
        let renewed = handle.snapshot();
        assert_eq!(renewed.credential_revision, 1);
        assert_eq!(renewed.desired_room, None);
        handle.renew("new-token".into()).unwrap();
        assert_eq!(handle.snapshot(), renewed);

        for index in 0..1000 {
            handle.join(format!("room-{index}")).unwrap();
        }
        let committed = handle.snapshot();
        assert_eq!(committed.desired_room.as_deref(), Some("room-999"));
        assert_eq!(committed.join_revision, 1000);
        handle.join("room-999".into()).unwrap();
        assert_eq!(handle.snapshot(), committed);
        assert!(actor.host.effect_events().is_empty());
        assert!(actor.host.turns.borrow().is_empty());
        let wake = receiver.try_recv().unwrap();
        assert!(receiver.try_recv().is_none(), "join wake is coalesced");
        let reentrant = handle.clone();
        let mut callback = actor.host.on_turn.borrow_mut();
        *callback = Some(Box::new(move || {
            reentrant.join("from-output".into()).unwrap();
            assert_eq!(
                reentrant.snapshot().desired_room.as_deref(),
                Some("from-output")
            );
        }));
        drop(callback);
        actor.process(wake);
        assert!(!actor.host.effect_events().is_empty());
        assert_eq!(
            handle.snapshot().desired_room.as_deref(),
            Some("from-output")
        );
        actor.host.on_turn.borrow_mut().take();
        actor.process(receiver.try_recv().unwrap());
        assert_eq!(actor.last_snapshot.as_ref(), Some(&handle.snapshot()));

        handle
            .send_public(PublicCommand::SetLogLevel(agent_core::LogLevel::Off))
            .unwrap();
        handle.join("mailbox-full".into()).unwrap();
        let committed = handle.snapshot();
        assert_eq!(committed.desired_room.as_deref(), Some("mailbox-full"));
        actor.process(receiver.try_recv().unwrap());
        assert_eq!(actor.last_snapshot.as_ref(), Some(&committed));
        assert!(receiver.try_recv().is_none());
        handle.join("awake-again".into()).unwrap();
        actor.process(receiver.try_recv().unwrap());
        assert_eq!(actor.last_snapshot.as_ref(), Some(&handle.snapshot()));

        let sibling = handle.clone();
        handle.request_close().unwrap();
        let closing = handle.snapshot();
        assert!(sibling.join("too-late".into()).is_err());
        assert!(sibling.renew("too-late".into()).is_err());
        assert_eq!(handle.snapshot(), closing);
        actor.process(receiver.try_recv().unwrap());
        assert_eq!(
            handle.snapshot().desired_revision,
            closing.desired_revision + 1
        );
        assert!(
            actor
                .host
                .effect_events()
                .iter()
                .any(|effect| matches!(effect, Effect::Rtc(RtcEffect::Close { .. })))
        );
        actor.shutdown();
        assert!(handle.join("after-shutdown".into()).is_err());

        let mut actor = Actor::new(config(), FakeHost::default()).unwrap();
        let (sender, receiver) = super::channel::mpsc::channel(1);
        let handle = actor.handle(sender);
        drop(receiver);
        let before = handle.snapshot();
        assert!(handle.join("no-runtime".into()).is_err());
        assert_eq!(handle.snapshot(), before);
    }

    #[test]
    fn zero_timer_is_actor_owned_cancelled_and_fired_once() {
        use agent_core::TimerEffect;
        use std::time::Duration;
        let (timer, generation) = retry_timer();
        let mut actor = Actor::new(config(), FakeHost::default()).unwrap();
        let publish = |actor: &mut Actor<FakeHost>, effects| {
            actor.publish_turn(super::Turn {
                effects,
                ..Default::default()
            });
        };
        publish(
            &mut actor,
            vec![
                Effect::Timer(TimerEffect::Schedule {
                    timer,
                    after: Duration::ZERO,
                }),
                Effect::Rtc(RtcEffect::Close { generation }),
            ],
        );
        assert_eq!(
            actor.host.effect_events(),
            vec![Effect::Rtc(RtcEffect::Close { generation })]
        );
        assert_eq!(actor.core_wake, Some(timer));
        publish(
            &mut actor,
            vec![Effect::Timer(TimerEffect::Cancel { timer })],
        );
        assert!(!actor.wake_core());
        assert_eq!(
            actor.host.effect_events(),
            vec![Effect::Rtc(RtcEffect::Close { generation })]
        );
        publish(
            &mut actor,
            vec![Effect::Timer(TimerEffect::Schedule {
                timer,
                after: Duration::ZERO,
            })],
        );
        assert!(actor.wake_core()); // stale for this Core instance
        assert!(!actor.wake_core());
        publish(
            &mut actor,
            vec![Effect::Timer(TimerEffect::Schedule {
                timer,
                after: Duration::from_secs(1),
            })],
        );
        assert!(
            matches!(actor.host.effect_events().last(), Some(Effect::Timer(TimerEffect::Schedule { after, .. })) if *after == Duration::from_secs(1))
        );
        publish(
            &mut actor,
            vec![Effect::Timer(TimerEffect::Schedule {
                timer,
                after: Duration::ZERO,
            })],
        );
        actor.shutdown();
        assert!(!actor.wake_core());
    }

    #[test]
    fn pending_wake_precedes_queued_input_and_preserves_input_order() {
        use std::future::Future;
        use std::task::{Context, Poll, Waker};
        use std::time::Duration;

        let mut actor = Actor::new(config(), FakeHost::default()).unwrap();
        actor.turn(Input::Command(AgentCommand::Join("room".into())));
        let generation = actor.driver.snapshot().generation.unwrap();
        actor.process(Message::HostEvent(HostEvent::Rtc(RtcEvent::Failed {
            generation,
            message: "offline".into(),
        })));
        let timer = actor
            .host
            .effect_events()
            .iter()
            .find_map(|effect| match effect {
                Effect::Timer(agent_core::TimerEffect::Schedule { timer, .. }) => Some(*timer),
                _ => None,
            })
            .unwrap();
        actor.publish_turn(super::Turn {
            effects: vec![Effect::Timer(agent_core::TimerEffect::Schedule {
                timer,
                after: Duration::ZERO,
            })],
            ..Default::default()
        });
        let (sender, receiver) = super::channel::mpsc::channel(4);
        sender
            .try_send(Message::Public(PublicCommand::SetConnected(false)))
            .unwrap();
        sender
            .try_send(Message::Public(PublicCommand::SetConnected(true)))
            .unwrap();
        drop(sender);
        let mut run = Box::pin(super::run_actor(actor, receiver));
        let Poll::Ready(mut actor) = run.as_mut().poll(&mut Context::from_waker(Waker::noop()))
        else {
            panic!("closed mailbox should finish without wall time");
        };
        let effects = actor.host.effect_events();
        let offers: Vec<_> = effects
            .iter()
            .enumerate()
            .filter_map(|(index, effect)| match effect {
                Effect::Rtc(RtcEffect::CreateOffer { generation, .. }) => {
                    Some((index, *generation))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            offers.len(),
            2,
            "the pending wake fires despite queued input"
        );
        let (retry, retry_generation) = offers[1];
        let close = effects
            .iter()
            .rposition(|effect| matches!(effect, Effect::Rtc(RtcEffect::Close { .. })))
            .unwrap();
        assert!(retry < close, "the retry runs before the queued disconnect");
        assert_eq!(
            effects[close],
            Effect::Rtc(RtcEffect::Close {
                generation: retry_generation
            })
        );
        assert_eq!(actor.driver.snapshot().desired_revision, 3);
        let before = actor.driver.snapshot();
        actor.driver.turn(Input::Command(AgentCommand::Connect));
        assert_eq!(actor.driver.snapshot(), before);
    }

    #[test]
    fn driver_coalesces_unchanged_snapshots_and_drains_effects() {
        let mut driver = Driver::new(config()).unwrap();
        let first = driver.turn(Input::Command(AgentCommand::Join("room".into())));
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
        let opening = driver.turn(Input::Command(AgentCommand::Join("room".into())));
        let generation = match opening.effects.as_slice() {
            [Effect::Rtc(RtcEffect::CreateOffer { generation, .. })] => *generation,
            effects => panic!("expected one create-offer effect, got {effects:?}"),
        };

        let rejected = driver.turn(Input::Event(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation,
            offer: String::new(),
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

        actor.turn(Input::Command(AgentCommand::Join("room".into())));
        let expected_generation = actor.driver.snapshot().generation.unwrap().get();
        for generation in [expected_generation, expected_generation.saturating_add(1)] {
            let before = actor.driver.snapshot();
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
            assert_eq!(actor.driver.snapshot(), before);
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
        actor.turn(Input::Command(AgentCommand::Join("room".into())));
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
