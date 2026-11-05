use super::*;
use agent_core::{DataUpdate, ResourceCommand, ResourceResult, TopicMode, TrackSpec};

type ReadResponse = oneshot::Sender<Result<DataUpdate, Error>>;

#[derive(Clone)]
struct DataAddress {
    spec: TrackSpec,
    participant_id: Option<String>,
    commands: mpsc::Sender<Command>,
    closed: Arc<Signal>,
    finished: Arc<Signal>,
}

impl DataAddress {
    async fn receive(&self) -> Result<DataReceiver, Error> {
        if self.closed.is_closed() || self.finished.is_closed() {
            return Err(Error::Closed);
        }
        let (response, result) = oneshot::channel();
        self.commands
            .send(Command::ReceiveData {
                spec: self.spec.clone(),
                participant_id: self.participant_id.clone(),
                response,
            })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }
}

#[derive(Clone)]
pub struct LocalDataTrack {
    address: DataAddress,
}

impl LocalDataTrack {
    pub async fn send(&self, data: Vec<u8>) -> Result<(), Error> {
        if self.address.closed.is_closed() || self.address.finished.is_closed() {
            return Err(Error::Closed);
        }
        let (response, result) = oneshot::channel();
        self.address
            .commands
            .send(Command::SendData {
                spec: self.address.spec.clone(),
                data,
                response,
            })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }

    pub async fn receive(&self) -> Result<DataReceiver, Error> {
        self.address.receive().await
    }
}

#[derive(Clone)]
pub struct Participant {
    id: String,
    pub(super) commands: mpsc::Sender<Command>,
    pub(super) closed: Arc<Signal>,
    pub(super) finished: Arc<Signal>,
}

impl Participant {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn data(&self, label: String, delivery: TopicMode) -> Result<RemoteDataTrack, Error> {
        Ok(RemoteDataTrack {
            address: data_address(
                &self.commands,
                &self.closed,
                &self.finished,
                label,
                delivery,
                Some(self.id.clone()),
            )?,
        })
    }
}

#[derive(Clone)]
pub struct RemoteDataTrack {
    address: DataAddress,
}

impl RemoteDataTrack {
    pub async fn receive(&self) -> Result<DataReceiver, Error> {
        self.address.receive().await
    }
}

fn data_address(
    commands: &mpsc::Sender<Command>,
    closed: &Arc<Signal>,
    finished: &Arc<Signal>,
    label: String,
    delivery: TopicMode,
    participant_id: Option<String>,
) -> Result<DataAddress, Error> {
    if closed.is_closed() || finished.is_closed() {
        return Err(Error::Closed);
    }
    let spec = TrackSpec::Data { label, delivery };
    spec.validate()
        .map_err(|error| Error::Core(agent_core::AgentError::InvalidResource(error.into())))?;
    Ok(DataAddress {
        spec,
        participant_id,
        commands: commands.clone(),
        closed: closed.clone(),
        finished: finished.clone(),
    })
}

impl Agent {
    pub fn data(&self, label: String, delivery: TopicMode) -> Result<LocalDataTrack, Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        Ok(LocalDataTrack {
            address: data_address(
                &self.commands,
                &self.lease.close,
                &self.lease.finished,
                label,
                delivery,
                None,
            )?,
        })
    }

    pub fn participant(&self, id: String) -> Result<Participant, Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        agent_core::validate_participant_id(&id)
            .map_err(|error| Error::Core(agent_core::AgentError::InvalidResource(error.into())))?;
        Ok(Participant {
            id,
            commands: self.commands.clone(),
            closed: self.lease.close.clone(),
            finished: self.lease.finished.clone(),
        })
    }
}

struct DataLease {
    owner: u64,
    closed: Arc<Signal>,
    finished: Arc<Signal>,
    commands: mpsc::Sender<Command>,
    changed: Arc<Notify>,
    reader: Mutex<()>,
}

impl Drop for DataLease {
    fn drop(&mut self) {
        self.closed.close();
        self.changed.notify_one();
    }
}

#[derive(Clone)]
pub struct DataReceiver {
    inner: Arc<DataLease>,
}

impl DataReceiver {
    pub fn is_closed(&self) -> bool {
        self.inner.closed.is_closed() || self.inner.finished.is_closed()
    }

    pub async fn next(&self) -> Result<DataUpdate, Error> {
        let _reader = self.inner.reader.lock().await;
        if self.is_closed() {
            return Ok(DataUpdate::Closed);
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
            return Ok(DataUpdate::Closed);
        }
        result.await.unwrap_or(Ok(DataUpdate::Closed))
    }

    pub async fn close(&self) -> Result<(), Error> {
        if self.is_closed() {
            return Ok(());
        }
        self.inner.closed.close();
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
    pub(super) fn receive_data(
        &mut self,
        spec: TrackSpec,
        participant_id: Option<String>,
    ) -> Result<DataReceiver, Error> {
        let ResourceResult::Owned { owner } = self.core.resource(ResourceCommand::ReceiveData {
            spec,
            participant_id,
        })?
        else {
            return Err(Error::Rtc(
                "Core did not allocate a data receive owner".into(),
            ));
        };
        let inner = Arc::new(DataLease {
            owner,
            closed: Arc::new(Signal::default()),
            finished: self.finished.clone(),
            commands: self.command_tx.clone(),
            changed: self.leases_changed.clone(),
            reader: Mutex::new(()),
        });
        self.data_receivers.insert(
            owner,
            DataIo {
                lease: Arc::downgrade(&inner),
                pending: None,
            },
        );
        Ok(DataReceiver { inner })
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
                    if let Some(lease) = io.lease.upgrade() {
                        lease.closed.close();
                    }
                    self.leases_changed.notify_one();
                    let error = result.err().map(Error::Core).unwrap_or_else(|| {
                        Error::Rtc("Core did not return a data receive event".into())
                    });
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
