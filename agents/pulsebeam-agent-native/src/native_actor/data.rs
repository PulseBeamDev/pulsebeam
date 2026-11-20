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
    fn subscribe(&self) -> Result<DataReceiver, Error> {
        if self.closed.is_closed() || self.finished.is_closed() {
            return Err(Error::Closed);
        }
        let source = self.source.clone().ok_or_else(|| {
            Error::InvalidInput("local data does not identify a remote source".into())
        })?;
        let ResourceResult::Owned { owner } = self.core.resource(ResourceCommand::ReceiveData {
            spec: self.spec.clone(),
            source,
        })?
        else {
            return Err(Error::Rtc(
                "Core did not reserve a data subscription".into(),
            ));
        };
        let receiver = DataReceiver::new(
            owner,
            self.core.clone(),
            self.commands.clone(),
            self.finished.clone(),
            self.core.changed.clone(),
        )?;
        if self.closed.is_closed() || self.finished.is_closed() {
            return Err(Error::Closed);
        }
        match self.commands.try_send(Command::RegisterDataReceiver {
            receiver: receiver.clone(),
        }) {
            Ok(()) => {
                self.core.changed.notify_one();
                Ok(receiver)
            }
            Err(mpsc::error::TrySendError::Full(_)) => Err(Error::Backpressure),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(Error::Closed),
        }
    }
}

#[derive(Clone)]
pub struct LocalDataTrack {
    address: DataAddress,
}

impl LocalDataTrack {
    pub fn publish(&self) -> Result<DataPublication, Error> {
        if self.address.closed.is_closed() || self.address.finished.is_closed() {
            return Err(Error::Closed);
        }
        let ResourceResult::Owned { owner } =
            self.address
                .core
                .resource(ResourceCommand::DeclareDataPublication {
                    spec: self.address.spec.clone(),
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
    fn new(
        owner: u64,
        core: Control,
        commands: mpsc::Sender<Command>,
        finished: Arc<Signal>,
        changed: Arc<Notify>,
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
        self.inner.closed.is_closed() || self.inner.finished.is_closed()
    }

    pub async fn next(&self) -> Result<DataUpdate, Error> {
        let _reader = self.inner.reader.lock().await;
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
            match self.core.resource(ResourceCommand::ReadData { owner }) {
                Ok(ResourceResult::Event { event: Some(event) }) => {
                    let _ = response.send(Ok(DataUpdate::Event { event }));
                }
                Ok(ResourceResult::Event { event: None }) => io.pending = Some(response),
                result => {
                    let error = result.err().map(Error::Core).unwrap_or_else(|| {
                        Error::Rtc("Core did not return a data receive event".into())
                    });
                    if let Some(lease) = io.lease.upgrade() {
                        let _ = lease.failure.set(error.clone());
                        lease.closed.close();
                    }
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
                .is_some_and(|lease| !lease.closed.is_closed())
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
