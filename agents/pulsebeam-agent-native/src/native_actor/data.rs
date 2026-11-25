use super::*;
use agent_core::{DataUpdate, ResourceCommand, ResourceResult, TopicMode, TrackSpec};

type ReadResponse = oneshot::Sender<Result<DataUpdate, Error>>;

#[derive(Clone)]
struct DataAddress {
    spec: TrackSpec,
    source: Option<agent_core::TrackSelector>,
    commands: mpsc::Sender<Command>,
    core: Control,
    closed: Arc<Signal>,
    finished: Arc<Signal>,
}

impl DataAddress {
    fn with_delivery(&self, delivery: TopicMode) -> Self {
        Self {
            spec: TrackSpec::Data {
                track_id: self.spec.track_id().into(),
                delivery,
            },
            ..self.clone()
        }
    }

    fn subscribe(&self) -> Result<DataReceiver, Error> {
        let source = self.source.clone().ok_or_else(|| {
            Error::InvalidInput("local data does not identify a remote source".into())
        })?;
        DataReceiver::subscribe(
            &self.core,
            &self.commands,
            &self.closed,
            &self.finished,
            ResourceCommand::ReceiveData {
                spec: self.spec.clone(),
                source,
            },
        )
    }
}

#[derive(Clone)]
pub struct LocalDataTrack {
    address: DataAddress,
}

impl LocalDataTrack {
    pub(crate) fn publish_with_options(
        &self,
        delivery: TopicMode,
        capture_clock: Option<agent_core::ffi::CaptureClock>,
        record_dd: bool,
    ) -> Result<DataPublication, Error> {
        Self {
            address: self.address.with_delivery(delivery),
        }
        .publish_configured(capture_clock, record_dd)
    }

    pub fn publish(&self) -> Result<DataPublication, Error> {
        self.publish_configured(None, false)
    }

    fn publish_configured(
        &self,
        capture_clock: Option<agent_core::ffi::CaptureClock>,
        record_dd: bool,
    ) -> Result<DataPublication, Error> {
        if self.address.closed.is_closed() || self.address.finished.is_closed() {
            return Err(Error::Closed);
        }
        let ResourceResult::Owned { owner } =
            self.address
                .core
                .resource(ResourceCommand::DeclareDataPublication {
                    spec: self.address.spec.clone(),
                    capture_clock,
                    record_dd,
                })?
        else {
            return Err(Error::Rtc("Core did not reserve a data publication".into()));
        };
        let observer = match self.address.core.data_publication_observer(owner) {
            Ok(observer) => observer,
            Err(error) => {
                let _ = self
                    .address
                    .core
                    .resource(ResourceCommand::Release { owner });
                return Err(error);
            }
        };
        Ok(DataPublication {
            inner: Arc::new(DataPublicationLease {
                owner,
                address: self.address.clone(),
                observer,
                closed: Signal::default(),
            }),
        })
    }
}

struct DataPublicationLease {
    owner: u64,
    address: DataAddress,
    observer: Arc<agent_core::observer::DataPublicationStateObserver>,
    closed: Signal,
}

impl Drop for DataPublicationLease {
    fn drop(&mut self) {
        self.closed.close();
        let _ = self
            .address
            .core
            .resource(ResourceCommand::Release { owner: self.owner });
        self.address.core.changed.notify_one();
    }
}

#[derive(Clone)]
pub struct DataPublication {
    inner: Arc<DataPublicationLease>,
}

impl DataPublication {
    pub fn state(&self) -> agent_core::ffi::DataPublicationState {
        self.inner.address.core.state();
        self.inner.observer.state()
    }

    pub fn updates(
        &self,
    ) -> Result<agent_core::observer::DataPublicationUpdates, agent_core::observer::ObserverLimit>
    {
        self.state();
        self.inner.observer.updates()
    }

    pub async fn send(&self, data: Vec<u8>) -> Result<(), Error> {
        self.send_at(data, None).await
    }

    pub(crate) async fn send_at(
        &self,
        data: Vec<u8>,
        options: Option<agent_core::ffi::DataSendOptions>,
    ) -> Result<(), Error> {
        if self.inner.closed.is_closed()
            || self.inner.address.closed.is_closed()
            || self.inner.address.finished.is_closed()
        {
            return Err(Error::Closed);
        }
        let expected_join = self.inner.address.core.state().desired_join_revision;
        let (response, result) = oneshot::channel();
        self.inner
            .address
            .commands
            .send(Command::SendOwnedData {
                owner: self.inner.owner,
                expected_join,
                data,
                options,
                response,
            })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }

    pub fn close(&self) -> Result<(), Error> {
        if self.inner.closed.is_closed() {
            return Ok(());
        }
        self.inner.closed.close();
        self.inner.address.core.resource(ResourceCommand::Release {
            owner: self.inner.owner,
        })?;
        self.inner.address.core.changed.notify_one();
        Ok(())
    }
}

#[derive(Clone)]
pub struct RoomRef {
    name: String,
    commands: mpsc::Sender<Command>,
    core: Control,
    closed: Arc<Signal>,
    finished: Arc<Signal>,
}

impl RoomRef {
    pub fn watch(&self, config: agent_core::ffi::WatchConfig) -> Result<super::Watch, Error> {
        if self.closed.is_closed() || self.finished.is_closed() {
            return Err(Error::Closed);
        }
        let watch = super::Watch::new(self.name.clone(), config, self.core.clone())?;
        if self.closed.is_closed() || self.finished.is_closed() {
            return Err(Error::Closed);
        }
        Ok(watch)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn participant(&self, id: String) -> Result<Participant, Error> {
        if self.closed.is_closed() || self.finished.is_closed() {
            return Err(Error::Closed);
        }
        agent_core::validate_participant_id(&id)
            .map_err(|error| Error::Core(agent_core::AgentError::InvalidResource(error.into())))?;
        Ok(Participant {
            room: self.name.clone(),
            id,
            commands: self.commands.clone(),
            core: self.core.clone(),
            closed: self.closed.clone(),
            finished: self.finished.clone(),
        })
    }
}

#[derive(Clone)]
pub struct Participant {
    room: String,
    id: String,
    pub(super) commands: mpsc::Sender<Command>,
    pub(super) core: Control,
    pub(super) closed: Arc<Signal>,
    pub(super) finished: Arc<Signal>,
}

impl Participant {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub(super) fn source(&self, track_id: String) -> agent_core::TrackSelector {
        agent_core::TrackSelector {
            room: self.room.clone(),
            participant_id: self.id.clone(),
            track_id,
        }
    }

    pub fn data(&self, track_id: String, delivery: TopicMode) -> Result<RemoteDataTrack, Error> {
        let source = self.source(track_id.clone());
        Ok(RemoteDataTrack {
            address: data_address(
                &self.commands,
                &self.core,
                &self.closed,
                &self.finished,
                track_id,
                delivery,
                Some(source),
            )?,
        })
    }
}

#[derive(Clone)]
pub struct RemoteDataTrack {
    address: DataAddress,
}

impl RemoteDataTrack {
    pub(crate) fn subscribe_with_delivery(
        &self,
        delivery: TopicMode,
    ) -> Result<DataReceiver, Error> {
        self.address.with_delivery(delivery).subscribe()
    }

    pub fn subscribe(&self) -> Result<DataReceiver, Error> {
        self.address.subscribe()
    }
}

fn data_address(
    commands: &mpsc::Sender<Command>,
    core: &Control,
    closed: &Arc<Signal>,
    finished: &Arc<Signal>,
    track_id: String,
    delivery: TopicMode,
    source: Option<agent_core::TrackSelector>,
) -> Result<DataAddress, Error> {
    if closed.is_closed() || finished.is_closed() {
        return Err(Error::Closed);
    }
    let spec = TrackSpec::Data { track_id, delivery };
    spec.validate()
        .map_err(|error| Error::Core(agent_core::AgentError::InvalidResource(error.into())))?;
    if let Some(source) = &source {
        source
            .validate()
            .map_err(|error| Error::Core(agent_core::AgentError::InvalidResource(error.into())))?;
    }
    Ok(DataAddress {
        spec,
        source,
        commands: commands.clone(),
        core: core.clone(),
        closed: closed.clone(),
        finished: finished.clone(),
    })
}

impl Agent {
    pub fn receive_selector_data(
        &self,
        owner: agent_core::TrackOwner,
        source: agent_core::SelectorMatch,
    ) -> Result<DataReceiver, Error> {
        DataReceiver::subscribe(
            &self.core,
            &self.commands,
            &self.lease.close,
            &self.lease.finished,
            ResourceCommand::ReceiveSelectorData { owner, source },
        )
    }

    pub fn data(&self, track_id: String, delivery: TopicMode) -> Result<LocalDataTrack, Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        Ok(LocalDataTrack {
            address: data_address(
                &self.commands,
                &self.core,
                &self.lease.close,
                &self.lease.finished,
                track_id,
                delivery,
                None,
            )?,
        })
    }

    pub fn room(&self, name: String) -> Result<RoomRef, Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        agent_core::validate_room_name(&name)
            .map_err(|error| Error::Core(agent_core::AgentError::InvalidResource(error.into())))?;
        Ok(RoomRef {
            name,
            commands: self.commands.clone(),
            core: self.core.clone(),
            closed: self.lease.close.clone(),
            finished: self.lease.finished.clone(),
        })
    }
}

struct DataLease {
    owner: u64,
    selector_owned: bool,
    core: Control,
    observer: Arc<agent_core::observer::DataSubscriptionStateObserver>,
    closed: Arc<Signal>,
    finished: Arc<Signal>,
    commands: mpsc::Sender<Command>,
    changed: Arc<Notify>,
    reader: Mutex<()>,
    failure: std::sync::OnceLock<Error>,
}

impl DataLease {
    fn match_retired(&self) -> bool {
        self.selector_owned && self.observer.state().closed
    }

    fn read_result(&self, result: Result<DataUpdate, Error>) -> Result<DataUpdate, Error> {
        self.failure
            .get()
            .map_or(result, |error| Err(error.clone()))
    }
}

impl Drop for DataLease {
    fn drop(&mut self) {
        self.closed.close();
        let _ = self
            .core
            .resource(ResourceCommand::Release { owner: self.owner });
        self.changed.notify_one();
    }
}

#[derive(Clone)]
pub struct DataReceiver {
    inner: Arc<DataLease>,
}

impl DataReceiver {
    fn subscribe(
        core: &Control,
        commands: &mpsc::Sender<Command>,
        closed: &Signal,
        finished: &Arc<Signal>,
        command: ResourceCommand,
    ) -> Result<Self, Error> {
        if closed.is_closed() || finished.is_closed() {
            return Err(Error::Closed);
        }
        let selector_owned = matches!(command, ResourceCommand::ReceiveSelectorData { .. });
        let ResourceResult::Owned { owner } = core.resource(command)? else {
            return Err(Error::Rtc(
                "Core did not reserve a data subscription".into(),
            ));
        };
        let receiver = Self::new(
            owner,
            core.clone(),
            commands.clone(),
            finished.clone(),
            core.changed.clone(),
            selector_owned,
        )?;
        if closed.is_closed() || finished.is_closed() {
            return Err(Error::Closed);
        }
        match commands.try_send(Command::RegisterDataReceiver {
            receiver: receiver.clone(),
        }) {
            Ok(()) => {
                core.changed.notify_one();
                Ok(receiver)
            }
            Err(mpsc::error::TrySendError::Full(_)) => Err(Error::Backpressure),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(Error::Closed),
        }
    }

    fn new(
        owner: u64,
        core: Control,
        commands: mpsc::Sender<Command>,
        finished: Arc<Signal>,
        changed: Arc<Notify>,
        selector_owned: bool,
    ) -> Result<Self, Error> {
        let observer = match core.data_subscription_observer(owner) {
            Ok(observer) => observer,
            Err(error) => {
                let _ = core.resource(ResourceCommand::Release { owner });
                return Err(error);
            }
        };
        Ok(Self {
            inner: Arc::new(DataLease {
                owner,
                selector_owned,
                core,
                observer,
                commands,
                finished,
                changed,
                closed: Arc::new(Signal::default()),
                reader: Mutex::new(()),
                failure: std::sync::OnceLock::new(),
            }),
        })
    }

    pub fn state(&self) -> agent_core::ffi::DataSubscriptionState {
        self.inner.core.state();
        self.inner.observer.state()
    }

    pub fn updates(
        &self,
    ) -> Result<agent_core::observer::DataSubscriptionUpdates, agent_core::observer::ObserverLimit>
    {
        self.state();
        self.inner.observer.updates()
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.is_closed()
            || self.inner.finished.is_closed()
            || self.inner.match_retired()
    }

    pub async fn next(&self) -> Result<DataUpdate, Error> {
        let _reader = self.inner.reader.try_lock().map_err(|_| {
            Error::Core(agent_core::AgentError::OperationFailed(
                agent_core::ffi::ErrorInfo::new(
                    agent_core::ffi::ErrorCode::Conflict,
                    "A data read is already pending",
                ),
            ))
        })?;
        if self.is_closed() || self.inner.failure.get().is_some() {
            return self.inner.read_result(Ok(DataUpdate::Closed));
        }
        let (response, result) = oneshot::channel();
        if self
            .inner
            .commands
            .send(Command::ReadData {
                owner: self.inner.owner,
                response,
            })
            .await
            .is_err()
        {
            return self.inner.read_result(Ok(DataUpdate::Closed));
        }
        self.inner
            .read_result(result.await.unwrap_or(Ok(DataUpdate::Closed)))
    }

    pub async fn close(&self) -> Result<(), Error> {
        if self.is_closed() {
            return Ok(());
        }
        self.inner.closed.close();
        self.inner.core.resource(ResourceCommand::Release {
            owner: self.inner.owner,
        })?;
        self.inner.changed.notify_one();
        let (response, result) = oneshot::channel();
        if self
            .inner
            .commands
            .send(Command::Resource {
                command: ResourceCommand::Release {
                    owner: self.inner.owner,
                },
                response,
            })
            .await
            .is_ok()
        {
            result.await.unwrap_or(Ok(ResourceResult::Updated))?;
        }
        Ok(())
    }
}

pub(super) struct DataIo {
    lease: Weak<DataLease>,
    pending: Option<ReadResponse>,
}

impl<R, F, O> Actor<R, F, O>
where
    R: rtc::SessionRuntime,
    F: FnMut() -> Result<(R, O), Error>,
{
    #[cfg(test)]
    pub(super) fn receive_data(
        &mut self,
        spec: TrackSpec,
        source: agent_core::TrackSelector,
    ) -> Result<DataReceiver, Error> {
        let ResourceResult::Owned { owner } = self
            .core
            .resource(ResourceCommand::ReceiveData { spec, source })?
        else {
            return Err(Error::Rtc(
                "Core did not allocate a data receive owner".into(),
            ));
        };
        let receiver = DataReceiver::new(
            owner,
            self.core.clone(),
            self.command_tx.clone(),
            self.finished.clone(),
            self.leases_changed.clone(),
            false,
        )?;
        self.register_data_receiver(receiver.clone());
        Ok(receiver)
    }

    pub(super) fn register_data_receiver(&mut self, receiver: DataReceiver) {
        let owner = receiver.inner.owner;
        if receiver.is_closed() {
            let _ = self.core.resource(ResourceCommand::Release { owner });
            return;
        }
        self.data_receivers.insert(
            owner,
            DataIo {
                lease: Arc::downgrade(&receiver.inner),
                pending: None,
            },
        );
    }

    pub(super) fn read_data(&mut self, owner: u64, response: ReadResponse) {
        if let Some(io) = self.data_receivers.get_mut(&owner) {
            io.pending = Some(response);
        } else {
            let _ = response.send(Ok(DataUpdate::Closed));
        }
    }

    pub(super) fn progress_data_reads(&mut self) {
        for (&owner, io) in &mut self.data_receivers {
            let Some(response) = io.pending.take() else {
                continue;
            };
            if response.is_closed() {
                continue;
            }
            let result = self.core.resource(ResourceCommand::ReadData { owner });
            // Closing a cursor releases the resource before the actor drains its lease queue.
            let Some(lease) = io.lease.upgrade().filter(|lease| !lease.closed.is_closed()) else {
                let _ = response.send(Ok(DataUpdate::Closed));
                continue;
            };
            match result {
                Ok(ResourceResult::Event { event: Some(event) }) => {
                    let _ = response.send(Ok(DataUpdate::Event { event }));
                }
                Ok(ResourceResult::Event { event: None }) => io.pending = Some(response),
                Err(agent_core::AgentError::InvalidResource(
                    agent_core::TrackResourceError::Released,
                )) if lease.selector_owned => {
                    lease.closed.close();
                    self.leases_changed.notify_one();
                    let _ = response.send(Ok(DataUpdate::Closed));
                }
                result => {
                    let error = result.err().map(Error::Core).unwrap_or_else(|| {
                        Error::Rtc("Core did not return a data receive event".into())
                    });
                    let _ = lease.failure.set(error.clone());
                    lease.closed.close();
                    self.leases_changed.notify_one();
                    let _ = response.send(Err(error));
                }
            }
        }
    }

    pub(super) fn retire_data_receivers(&mut self) {
        self.data_receivers.retain(|&owner, io| {
            if io
                .lease
                .upgrade()
                .is_some_and(|lease| !lease.closed.is_closed() && !lease.match_retired())
            {
                return true;
            }
            let _ = self.core.resource(ResourceCommand::Release { owner });
            if let Some(response) = io.pending.take() {
                let _ = response.send(Ok(DataUpdate::Closed));
            }
            false
        });
    }
}
