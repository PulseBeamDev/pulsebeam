use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::rc::Rc;
use std::time::Duration;

use futures_channel::oneshot;

use crate::channel::{self, mpsc, mpsc::TrySendError as MpscTrySendError, watch};

use agent_core::ffi::{ErrorCode, ErrorInfo};
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

mod catalog;
mod resource_state;
pub(crate) use resource_state::{ResourceKind, ResourceNotification, ResourceState};

#[derive(Debug)]
pub(crate) enum DataSendTarget {
    #[cfg(test)]
    Track(agent_core::TrackSpec),
    Publication {
        owner: u64,
        expected_join: u64,
    },
}

type DataSendCompletion = oneshot::Sender<Result<(), DataSendError>>;

#[derive(Default)]
pub(crate) struct Turn {
    pub(crate) effects: Vec<Effect>,
    pub(crate) notifications: Vec<Notification>,
    pub(crate) snapshot: Option<Snapshot>,
    pub(crate) publications: Vec<(u64, agent_core::ffi::PublicationState, bool)>,
    pub(crate) resources: Vec<(u64, ResourceState, bool)>,
    pub(crate) watches: Vec<catalog::WatchProjection>,
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
    publication_owners: RefCell<BTreeMap<u64, ()>>,
    resource_observers: RefCell<BTreeMap<u64, ResourceKind>>,
    watch_owners: catalog::WatchOwners,
}

impl Control {
    fn resource(
        &self,
        command: agent_core::ResourceCommand,
    ) -> Result<agent_core::ResourceResult, AgentError> {
        let release = match &command {
            agent_core::ResourceCommand::Release { owner } => Some(*owner),
            _ => None,
        };
        let result = self.agent.borrow_mut().resource(command);
        let mut owners = self.publication_owners.borrow_mut();
        if let Ok(agent_core::ResourceResult::Publication { owner, .. }) = &result {
            owners.insert(*owner, ());
        }
        if let Some(owner) = release {
            owners.remove(&owner);
            self.resource_observers.borrow_mut().remove(&owner);
        }
        result
    }
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
                publication_owners: RefCell::new(BTreeMap::new()),
                resource_observers: RefCell::new(BTreeMap::new()),
                watch_owners: RefCell::new(BTreeMap::new()),
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

    #[cfg(test)]
    pub(crate) fn turn(&mut self, input: Input) -> Turn {
        self.turn_at(input, Duration::ZERO)
    }

    fn turn_at(&mut self, input: Input, now: Duration) -> Turn {
        let source = match &input {
            Input::Command(_) => TurnErrorSource::Command,
            Input::Event(_) => TurnErrorSource::HostEvent,
        };
        let error = match input {
            Input::Command(command) => self.control.agent.borrow_mut().command(command),
            Input::Event(event) => self.control.agent.borrow_mut().handle_at(event, now),
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

    fn send_data(
        &mut self,
        target: DataSendTarget,
        payload: Vec<u8>,
        options: Option<agent_core::ffi::DataSendOptions>,
        now: Duration,
    ) -> (Result<agent_core::OperationId, AgentError>, Turn) {
        let result = (|| {
            let mut agent = self.control.agent.borrow_mut();
            let spec = match target {
                #[cfg(test)]
                DataSendTarget::Track(spec) => spec,
                DataSendTarget::Publication {
                    owner,
                    expected_join,
                } => {
                    if agent_core::ffi::AgentState::from(agent.snapshot()).desired_join_revision
                        != expected_join
                    {
                        return Err(agent_core::TrackResourceError::NotConnected.into());
                    }
                    let publication = agent.data_publication_state(owner)?;
                    agent_core::TrackSpec::Data {
                        track_id: publication.track_id,
                        delivery: publication.delivery.into(),
                    }
                }
            };
            agent.send_data_with_options_at(spec, payload, options, now)
        })();
        let turn = self.finish_turn(None);
        (result, turn)
    }

    pub(crate) fn resource(
        &mut self,
        command: agent_core::ResourceCommand,
    ) -> (Result<agent_core::ResourceResult, AgentError>, Turn) {
        let releasing = match &command {
            agent_core::ResourceCommand::Release { owner } => Some(*owner),
            _ => None,
        };
        let final_state =
            releasing.and_then(|owner| self.control.agent.borrow().publication_state(owner).ok());
        let final_resource = releasing.and_then(|owner| {
            self.control
                .resource_observers
                .borrow()
                .get(&owner)
                .and_then(|kind| kind.state(&self.control.agent.borrow(), owner))
        });
        let result = self.control.resource(command);
        let mut turn = self.finish_turn(None);
        if let (Some(owner), Some(mut state)) = (releasing, final_resource) {
            state.mark_closed();
            turn.resources.push((owner, state, true));
        }
        if let (Some(owner), Some(mut state)) = (releasing, final_state) {
            state.closed = true;
            turn.publications.push((owner, state, true));
        }
        (result, turn)
    }

    fn finish_turn(&mut self, error: Option<TurnError>) -> Turn {
        let mut agent = self.control.agent.borrow_mut();
        let mut effects = Vec::new();
        while let Some(effect) = agent.next_effect() {
            effects.push(effect);
        }

        let watches = self.control.collect_catalog_states(&agent);
        let mut notifications = Vec::new();
        while let Some(notification) = agent.next_notification() {
            notifications.push(notification);
        }

        let publications = self
            .control
            .publication_owners
            .borrow()
            .keys()
            .filter_map(|owner| {
                agent
                    .publication_state(*owner)
                    .ok()
                    .map(|state| (*owner, state, false))
            })
            .collect();

        let resources = self
            .control
            .resource_observers
            .borrow()
            .iter()
            .filter_map(|(owner, kind)| {
                kind.state(&agent, *owner)
                    .map(|state| (*owner, state, false))
            })
            .collect();
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
            publications,
            resources,
            watches,
            error,
            logs: agent.drain_logs(),
        }
    }
}

pub(crate) trait Host {
    fn monotonic_time(&self) -> Duration;
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
    fn prepare_publication_state(
        &self,
        _owner: u64,
        _state: &agent_core::ffi::PublicationState,
        _closing: bool,
    ) -> Option<agent_core::observer::PublicationNotification> {
        None
    }
    fn prepare_resource_state(
        &self,
        _owner: u64,
        _state: &ResourceState,
        _closing: bool,
    ) -> Option<ResourceNotification> {
        None
    }
    fn prepare_catalog_state(
        &self,
        _owner: u64,
        _state: &agent_core::ffi::WatchState,
        _closing: bool,
    ) -> Option<ResourceNotification> {
        None
    }
    fn publication_input_state(&self, _state: &agent_core::PublicationInputState) {}
    fn notify_prepared_pipe_states(&self) {}
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
        target: DataSendTarget,
        payload: Vec<u8>,
        options: Option<agent_core::ffi::DataSendOptions>,
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

    pub(crate) fn join(&self, room: String) -> Result<(), ErrorInfo> {
        self.intent(AgentCommand::Join(room))
    }

    pub(crate) fn leave_room(&self) -> Result<(), ErrorInfo> {
        self.intent(AgentCommand::LeaveRoom)
    }

    pub(crate) fn renew(&self, token: String) -> Result<(), ErrorInfo> {
        self.intent(AgentCommand::RenewAuthorization(token))
    }

    fn intent(&self, command: AgentCommand) -> Result<(), ErrorInfo> {
        if !self.control.accepting.get() || self.sender.is_closed() {
            return Err(ErrorInfo::new(
                ErrorCode::Closed,
                "Browser runtime is closed",
            ));
        }
        self.control
            .agent
            .borrow_mut()
            .command(command)
            .map_err(|error| ErrorInfo::from(&error))?;
        self.schedule_core_change()
    }

    pub(crate) fn snapshots(&self) -> watch::Receiver<Snapshot> {
        self.snapshots.clone()
    }

    pub(crate) fn publication_state(
        &self,
        owner: u64,
    ) -> Option<agent_core::ffi::PublicationState> {
        self.control.agent.borrow().publication_state(owner).ok()
    }

    pub(crate) fn observe_resource(&self, owner: u64, kind: ResourceKind) -> Option<ResourceState> {
        let state = kind.state(&self.control.agent.borrow(), owner)?;
        self.control
            .resource_observers
            .borrow_mut()
            .insert(owner, kind);
        Some(state)
    }

    pub(crate) fn observed_resource_state(&self, owner: u64) -> Option<ResourceState> {
        let kind = *self.control.resource_observers.borrow().get(&owner)?;
        kind.state(&self.control.agent.borrow(), owner)
    }

    pub(crate) fn resource_states(&self) -> Vec<(u64, ResourceState)> {
        let agent = self.control.agent.borrow();
        self.control
            .resource_observers
            .borrow()
            .iter()
            .filter_map(|(owner, kind)| kind.state(&agent, *owner).map(|state| (*owner, state)))
            .collect()
    }

    /// Synchronously reserve a local resource while preserving actor acceptance.
    /// CoreChanged is coalesced, and every actor turn projects the current Core.
    pub(crate) fn resource_now(
        &self,
        command: agent_core::ResourceCommand,
    ) -> Result<agent_core::ResourceResult, agent_core::ffi::ErrorInfo> {
        if !self.control.accepting.get() || self.sender.is_closed() {
            return Err(agent_core::ffi::ErrorInfo::new(
                agent_core::ffi::ErrorCode::Closed,
                "Actor is not running",
            ));
        }
        let result = self
            .control
            .resource(command)
            .map_err(|error| agent_core::ffi::ErrorInfo::from(&error))?;
        self.schedule_core_change()?;
        Ok(result)
    }

    #[cfg(all(target_arch = "wasm32", feature = "uniffi"))]
    pub(crate) fn read_owned_data(
        &self,
        owner: u64,
    ) -> Result<Option<agent_core::DataEvent>, agent_core::ffi::ErrorInfo> {
        match self
            .control
            .resource(agent_core::ResourceCommand::ReadData { owner })
            .map_err(|error| agent_core::ffi::ErrorInfo::from(&error))?
        {
            agent_core::ResourceResult::Event { event } => Ok(event),
            _ => Err(agent_core::ffi::ErrorInfo::new(
                agent_core::ffi::ErrorCode::Internal,
                "Unexpected data read result",
            )),
        }
    }

    pub(crate) fn pipe_state(
        &self,
        input: agent_core::PublicationInputId,
    ) -> Option<agent_core::ffi::PipeState> {
        self.control.agent.borrow().pipe_state(input)
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

    #[cfg(test)]
    pub(crate) fn send_data(
        &self,
        spec: agent_core::TrackSpec,
        payload: Vec<u8>,
    ) -> Result<oneshot::Receiver<Result<(), DataSendError>>, agent_core::ffi::ErrorInfo> {
        self.queue_data(DataSendTarget::Track(spec), payload, None)
    }

    pub(crate) fn send_owned_data(
        &self,
        owner: u64,
        payload: Vec<u8>,
        options: Option<agent_core::ffi::DataSendOptions>,
    ) -> Result<oneshot::Receiver<Result<(), DataSendError>>, agent_core::ffi::ErrorInfo> {
        let expected_join = {
            let agent = self.control.agent.borrow();
            agent
                .data_publication_state(owner)
                .map_err(|error| agent_core::ffi::ErrorInfo::from(&error))?;
            agent_core::ffi::AgentState::from(agent.snapshot()).desired_join_revision
        };
        self.queue_data(
            DataSendTarget::Publication {
                owner,
                expected_join,
            },
            payload,
            options,
        )
    }

    fn queue_data(
        &self,
        target: DataSendTarget,
        payload: Vec<u8>,
        options: Option<agent_core::ffi::DataSendOptions>,
    ) -> Result<oneshot::Receiver<Result<(), DataSendError>>, agent_core::ffi::ErrorInfo> {
        use agent_core::ffi::{ErrorCode, ErrorInfo};
        if self.closed.get() || !self.control.accepting.get() || self.sender.is_closed() {
            return Err(ErrorInfo::new(ErrorCode::Closed, "Data runtime is closed"));
        }
        let (completion, receiver) = oneshot::channel();
        self.sender
            .try_send(Message::DataSend {
                target,
                payload,
                options,
                completion,
            })
            .map_err(|error| match error {
                MpscTrySendError::Full(_) => ErrorInfo::new(
                    ErrorCode::CapacityExceeded,
                    "Data operation mailbox is full",
                ),
                MpscTrySendError::Closed(_) => {
                    ErrorInfo::new(ErrorCode::Closed, "Data runtime is closed")
                }
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
    core_wakes: VecDeque<TimerId>,
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
            core_wakes: VecDeque::new(),
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
            .and_then(|publisher| publisher.publication_id.as_deref());
        if binding != Some(expected_data_track_id) {
            self.flush_core();
            return Err("topic publication binding was replaced".to_owned());
        }
        let turn = self.driver.turn_at(
            Input::Command(AgentCommand::SendTopic(send)),
            self.host.monotonic_time(),
        );
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

        let turn = self.driver.turn_at(input, self.host.monotonic_time());
        self.publish_turn(turn);
    }

    fn publish_turn(&mut self, mut turn: Turn) {
        // Commit the entire turn before any foreign wake can reenter and change Core.
        let publications = turn
            .publications
            .iter()
            .filter_map(|(owner, state, closing)| {
                self.host.prepare_publication_state(*owner, state, *closing)
            })
            .collect::<Vec<_>>();
        let resources = turn
            .resources
            .iter()
            .filter_map(|(owner, state, closing)| {
                self.host.prepare_resource_state(*owner, state, *closing)
            })
            .collect::<Vec<_>>();
        let watches = turn
            .watches
            .iter()
            .filter_map(|(owner, state, closing)| {
                self.host.prepare_catalog_state(*owner, state, *closing)
            })
            .collect::<Vec<_>>();
        for publication in publications {
            publication.notify();
        }
        for notification in watches {
            notification();
        }
        for notification in resources {
            notification();
        }
        self.host.notify_prepared_pipe_states();
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
                let _ = completion.send(
                    error
                        .as_ref()
                        .map_or(Ok(()), |error| Err(DataSendError::Delivery(error.clone()))),
                );
            }
        }

        for effect in turn.effects.drain(..) {
            match effect {
                Effect::Timer(TimerEffect::Schedule { timer, after }) if after.is_zero() => {
                    if !self.core_wakes.contains(&timer) {
                        self.core_wakes.push_back(timer);
                    }
                }
                Effect::Timer(TimerEffect::Cancel { timer }) => {
                    if let Some(index) = self.core_wakes.iter().position(|id| *id == timer) {
                        self.core_wakes.remove(index);
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
        if let Some(timer) = self.core_wakes.pop_front() {
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
        self.finalize_catalog_watches();
        self.set_connected(false);
    }

    fn shutdown(&mut self) {
        if self.closed {
            return;
        }

        self.driver.control.accepting.set(false);
        self.closed = true;
        self.running = false;
        self.core_wakes.clear();
        self.finalize_catalog_watches();
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
                target,
                payload,
                options,
                completion,
            } => {
                let (result, turn) =
                    self.driver
                        .send_data(target, payload, options, self.host.monotonic_time());
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
                    agent_core::ResourceCommand::ClaimPublication { spec, .. } => spec.media_kind(),
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
                self.publish_turn(turn);
                if let Some(owner) = release {
                    self.host.resource_released(owner);
                }
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
        if actor.wake_core() && !actor.core_wakes.is_empty() {
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
        Actor, DataSendTarget, Driver, Host, Input, LifecycleCommand, Message, PublicCommand,
        SerialQueue, TurnErrorSource,
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
        watches: RefCell<Vec<(u64, agent_core::ffi::WatchState, bool)>>,
        on_catalog: RefCell<Option<std::rc::Rc<dyn Fn()>>>,
    }

    impl FakeHost {
        fn effect_events(&self) -> Vec<agent_core::Effect> {
            self.effects.borrow().clone()
        }
    }

    impl Host for FakeHost {
        fn monotonic_time(&self) -> std::time::Duration {
            std::time::Duration::ZERO
        }

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

        fn prepare_catalog_state(
            &self,
            owner: u64,
            state: &agent_core::ffi::WatchState,
            closing: bool,
        ) -> Option<super::ResourceNotification> {
            self.watches
                .borrow_mut()
                .push((owner, state.clone(), closing));
            self.on_catalog
                .borrow()
                .clone()
                .map(|callback| Box::new(move || callback()) as super::ResourceNotification)
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
        failed_offer_timer(&mut driver, turn)
    }

    fn failed_offer_timer(
        driver: &mut Driver,
        turn: super::Turn,
    ) -> (agent_core::TimerId, agent_core::Generation) {
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
        use agent_core::{TopicMode, TrackSpec, ffi::ErrorCode};
        let data = TrackSpec::Data {
            track_id: "events".into(),
            delivery: TopicMode::Ordered,
        };
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
        assert_eq!(
            handle.send_data(data.clone(), vec![1]).unwrap_err().code,
            ErrorCode::CapacityExceeded
        );
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
        assert_eq!(
            handle.send_data(data.clone(), vec![1]).unwrap_err().code,
            ErrorCode::Closed
        );

        let mut actor = Actor::new(config(), FakeHost::default()).unwrap();
        let (sender, receiver) = super::channel::mpsc::channel(1);
        let handle = actor.handle(sender);
        drop(receiver);
        let before = handle.snapshot();
        assert!(handle.join("no-runtime".into()).is_err());
        assert_eq!(handle.snapshot(), before);
        assert_eq!(
            handle.send_data(data, vec![1]).unwrap_err().code,
            ErrorCode::Closed
        );
    }

    #[test]
    fn local_resource_reservation_remains_wakeable_after_mailbox_saturation() {
        use agent_core::{ResourceCommand, ResourceResult, TrackSpec, ffi::ErrorCode};

        let mut actor = Actor::new(config(), FakeHost::default()).unwrap();
        let (sender, receiver) = super::channel::mpsc::channel(1);
        let handle = actor.handle(sender);
        handle
            .send_public(PublicCommand::SetLogLevel(agent_core::LogLevel::Off))
            .unwrap();
        let ResourceResult::Publication { owner, .. } = handle
            .resource_now(ResourceCommand::claim_publication(TrackSpec::Video {
                track_id: "pipe".into(),
            }))
            .unwrap()
        else {
            panic!("expected publication");
        };
        actor.process(receiver.try_recv().unwrap());
        assert!(receiver.try_recv().is_none());
        let ResourceResult::PublicationInput { operation, .. } = handle
            .resource_now(ResourceCommand::AttachPublicationInput { owner })
            .unwrap()
        else {
            panic!("expected pipe");
        };
        let wake = receiver
            .try_recv()
            .expect("a subsequent reservation must wake the actor");
        actor.process(wake);
        assert!(handle.pipe_state(operation.input().unwrap()).is_some());
        assert_eq!(
            handle
                .resource_now(ResourceCommand::AttachPublicationInput { owner })
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
    }

    #[test]
    fn catalog_failed_state_survives_reads_until_transfer_or_close() {
        use agent_core::ffi::{ErrorCode, WatchConfig, WatchStatus};
        use agent_core::{DataChannelEvent, HttpEffect, HttpEvent, HttpHeader, HttpResponse};
        use pulsebeam_proto::state::{self as wire, codec};

        for close in [false, true] {
            let mut actor = Actor::new(config(), FakeHost::default()).unwrap();
            let (sender, receiver) = super::channel::mpsc::channel(1);
            let handle = actor.handle(sender);
            handle.join("room".into()).unwrap();
            actor.process(receiver.try_recv().unwrap());
            while actor.wake_core() {}
            let generation = handle.snapshot().generation.unwrap();
            let channel = agent_core::ChannelId::new(400).unwrap();
            actor.event(HostEvent::Rtc(RtcEvent::OfferCreated {
                generation,
                offer: "v=0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:data\r\n"
                    .into(),
                resources: OfferResources {
                    reserved_mids: Vec::new(),
                    slots: Vec::new(),
                    signaling_channel: channel,
                    data_channels: Vec::new(),
                },
            }));
            let operation = actor
                .host
                .effects
                .borrow()
                .iter()
                .find_map(|effect| match effect {
                    Effect::Http(HttpEffect::Request { operation, .. }) => Some(*operation),
                    _ => None,
                })
                .expect("admission capability request");
            actor.event(HostEvent::Http(HttpEvent::Response {
                operation,
                response: HttpResponse {
                    status: 200,
                    headers: vec![],
                    body: br#"{"owner":"opaque-test-proof"}"#.to_vec(),
                },
            }));
            let operation = actor
                .host
                .effects
                .borrow()
                .iter()
                .rev()
                .find_map(|effect| match effect {
                    Effect::Http(HttpEffect::Request { operation, .. }) => Some(*operation),
                    _ => None,
                })
                .expect("join request");
            actor.event(HostEvent::Http(HttpEvent::Response {
                operation,
                response: HttpResponse {
                    status: 201,
                    headers: vec![HttpHeader {
                        name: "location".into(),
                        value: "https://example.com/participants/p1".into(),
                    }],
                    body: serde_json::to_vec(&serde_json::json!({
                        "room_id": "room",
                        "participant_id": "p1",
                        "connection_id": "conn_0065WZSB8G1XN31RDHP9883WVRC",
                        "profile": "pulsebeam.media.v1", "rtp_capabilities": {},
                        "data_delivery": ["LATEST", "LOSSY", "ORDERED"], "answer": "answer"
                    }))
                    .unwrap(),
                },
            }));
            actor.event(HostEvent::Rtc(RtcEvent::Connected { generation }));
            actor.event(HostEvent::DataChannel(DataChannelEvent::Opened {
                generation,
                channel,
            }));
            actor.event(HostEvent::Rtc(RtcEvent::AnswerApplied { generation }));
            actor.event(HostEvent::DataChannel(DataChannelEvent::Message {
                generation,
                channel,
                binary: true,
                payload: codec::encode_server(&wire::ServerStateBatch {
                    initial_complete: true,
                    records: vec![wire::ServerStateRecord {
                        key: Some(wire::ResourceKey {
                            key: Some(wire::resource_key::Key::Session(true)),
                        }),
                        confirmed: Some(wire::ConfirmedState {
                            evaluated_revision: 0,
                            rejection: None,
                            value: Some(wire::confirmed_state::Value::Session(
                                wire::SessionState {
                                    authorization: Some(wire::Authorization {
                                        expires_at_unix_seconds: 10001,
                                        revision: 1,
                                    }),
                                    closed: None,
                                    diagnostic: String::new(),
                                    successor_receipt: None,
                                },
                            )),
                        }),
                    }],
                    ..Default::default()
                })
                .unwrap(),
            }));
            let (owner, _) = handle
                .open_catalog_watch("room".into(), WatchConfig::default())
                .unwrap();
            actor.process(receiver.try_recv().unwrap());
            while actor.wake_core() {}
            let watch_id = actor
                .host
                .effects
                .borrow()
                .iter()
                .filter_map(|effect| match effect {
                    Effect::DataChannel(agent_core::DataChannelEffect::Send {
                        payload, ..
                    }) => codec::decode_client(payload).ok(),
                    _ => None,
                })
                .flat_map(|batch| batch.catalog_records)
                .find_map(|record| match record.command {
                    Some(wire::catalog_client_record::Command::Open(open)) => Some(open.watch_id),
                    _ => None,
                })
                .unwrap_or_else(|| {
                    panic!(
                        "catalog open missing: {:?}; effects: {:?}",
                        handle.snapshot(),
                        actor.host.effects.borrow()
                    )
                });
            // Pause between Core mutation and the actor turn to exercise synchronous reads.
            handle
                .control
                .agent
                .borrow_mut()
                .handle(HostEvent::DataChannel(DataChannelEvent::Message {
                    generation,
                    channel,
                    binary: true,
                    payload: codec::encode_server(&wire::ServerStateBatch {
                        catalog_records: vec![wire::CatalogServerRecord {
                            response: Some(wire::catalog_server_record::Response::Error(
                                wire::CatalogWatchError {
                                    watch_id,
                                    failure: Some(wire::Failure {
                                        code: wire::FailureCode::PermissionDenied as i32,
                                        diagnostic: "private authority".into(),
                                    }),
                                },
                            )),
                        }],
                        ..Default::default()
                    })
                    .unwrap(),
                }))
                .unwrap();
            let failed = handle.catalog_states();
            assert_eq!(handle.catalog_states(), failed);
            assert_eq!(failed.len(), 1);
            assert_eq!(failed[0].1.status, WatchStatus::Failed);
            assert_eq!(
                failed[0].1.error.as_ref().unwrap().code,
                ErrorCode::PermissionDenied
            );
            assert!(
                !failed[0]
                    .1
                    .error
                    .as_ref()
                    .unwrap()
                    .message
                    .contains("private authority")
            );
            actor.host.watches.borrow_mut().clear();
            if close {
                assert_eq!(handle.close_catalog_watch(owner), Some(failed[0].1.clone()));
                assert!(handle.close_catalog_watch(owner).is_none());
            }
            actor.flush_core();
            if !close {
                assert_eq!(*actor.host.watches.borrow(), failed);
            }
            assert!(handle.catalog_states().is_empty());
            actor.host.watches.borrow_mut().clear();
            actor.flush_core();
            assert!(
                actor.host.watches.borrow().is_empty(),
                "terminal state transfers once"
            );
            for _ in 0..8 {
                handle
                    .open_catalog_watch("room".into(), WatchConfig::default())
                    .unwrap();
            }
        }
    }

    #[test]
    fn catalog_reservations_are_synchronous_bounded_and_reusable() {
        use agent_core::ffi::{ErrorCode, WatchConfig, WatchStatus};
        let mut actor = Actor::new(config(), FakeHost::default()).unwrap();
        let (sender, receiver) = super::channel::mpsc::channel(1);
        let handle = actor.handle(sender);
        handle
            .send_public(PublicCommand::SetLogLevel(agent_core::LogLevel::Off))
            .unwrap();
        let owners = (0..8)
            .map(|_| {
                let (id, state) = handle
                    .open_catalog_watch("other-room".into(), WatchConfig::default())
                    .unwrap();
                assert_eq!(state.status, WatchStatus::Loading);
                assert_eq!(state.room, "other-room");
                assert_eq!(state.limit, 100);
                id
            })
            .collect::<Vec<_>>();
        assert_eq!(handle.snapshot().desired_room, None);
        assert_eq!(
            handle
                .open_catalog_watch("room".into(), WatchConfig::default())
                .unwrap_err()
                .code,
            ErrorCode::CapacityExceeded
        );
        assert_eq!(handle.catalog_states().len(), 8);
        assert_eq!(handle.catalog_states().len(), 8);
        actor.process(receiver.try_recv().unwrap());
        assert_eq!(actor.host.watches.borrow().len(), 8);
        assert!(receiver.try_recv().is_none());
        assert_eq!(
            handle.close_catalog_watch(owners[0]).unwrap().status,
            WatchStatus::Closed
        );
        assert!(handle.close_catalog_watch(owners[0]).is_none());
        handle
            .open_catalog_watch("new-room".into(), WatchConfig::default())
            .unwrap();
        actor.process(receiver.try_recv().expect("release must wake the actor"));
        assert!(receiver.try_recv().is_none());
        assert_eq!(handle.catalog_states().len(), 8);
        actor.close();
        assert!(handle.catalog_states().is_empty());
        assert_eq!(
            handle
                .open_catalog_watch("room".into(), WatchConfig::default())
                .unwrap_err()
                .code,
            ErrorCode::Closed
        );
    }

    #[test]
    fn catalog_root_finalization_prepares_all_states_before_reentrant_wakes() {
        use agent_core::ffi::{WatchConfig, WatchStatus};
        for abort in [false, true] {
            let mut actor = Actor::new(config(), FakeHost::default()).unwrap();
            let (sender, receiver) = super::channel::mpsc::channel(1);
            let handle = actor.handle(sender);
            for _ in 0..2 {
                handle
                    .open_catalog_watch("room".into(), WatchConfig::default())
                    .unwrap();
            }
            actor.process(receiver.try_recv().unwrap());
            actor.host.watches.borrow_mut().clear();
            let reentrant = handle.clone();
            *actor.host.on_catalog.borrow_mut() = Some(std::rc::Rc::new(move || {
                assert!(reentrant.catalog_states().is_empty());
                assert!(reentrant.close_catalog_watch(1).is_none());
            }));
            // Root finalization cannot depend on admission or another mailbox turn.
            handle.control.accepting.set(false);
            if abort {
                actor.shutdown();
            } else {
                actor.close();
            }
            let states = actor.host.watches.borrow();
            assert_eq!(states.len(), 2);
            assert!(
                states
                    .iter()
                    .all(|(_, state, closing)| *closing && state.status == WatchStatus::Closed)
            );
            drop(states);
            actor.shutdown();
            assert_eq!(actor.host.watches.borrow().len(), 2);
        }
    }

    #[test]
    fn zero_timer_is_actor_owned_cancelled_and_fired_once() {
        use agent_core::TimerEffect;
        use std::time::Duration;
        let mut driver = Driver::new(config()).unwrap();
        let turn = driver.turn(Input::Command(AgentCommand::Join("room".into())));
        let (timer, generation) = failed_offer_timer(&mut driver, turn);
        let retry = driver.turn(Input::Event(HostEvent::Timer(
            agent_core::TimerEvent::Fired { timer },
        )));
        let (other, _) = failed_offer_timer(&mut driver, retry);
        assert_ne!(timer, other);
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
        publish(
            &mut actor,
            vec![Effect::Timer(TimerEffect::Schedule {
                timer: other,
                after: Duration::ZERO,
            })],
        );
        assert_eq!(
            actor.core_wakes.iter().copied().collect::<Vec<_>>(),
            [timer, other]
        );
        publish(
            &mut actor,
            vec![Effect::Timer(TimerEffect::Cancel { timer })],
        );
        assert_eq!(actor.core_wakes.front(), Some(&other));
        assert!(actor.wake_core());
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
                reserved_mids: Vec::new(),
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
            target: DataSendTarget::Track(agent_core::TrackSpec::Data {
                track_id: "pending".into(),
                delivery: agent_core::TopicMode::Lossy,
            }),
            payload: vec![0, 255],
            options: None,
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
                agent_core::DataSendFailure::TerminalFailure { error }
            ))) if error == agent_core::ffi::AgentState::from(&actor.driver.snapshot()).error.unwrap()
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
