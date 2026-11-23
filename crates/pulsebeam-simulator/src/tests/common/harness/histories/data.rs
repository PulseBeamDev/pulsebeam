use super::*;
use pulsebeam_agent_native::agent_core::{
    AgentError, DataEvent, ResourceCommand, ResourceResult, TopicPublisher, TrackResourceError,
    TrackSelector, TrackSpec, ffi::PublicationExecution,
};
use pulsebeam_agent_native::{Agent, DataPublication, Error};

const BURST: u8 = 4;
const WARMUP_SENDS: u8 = 50;

pub(super) struct DataLane {
    delivery: Delivery,
    topic: String,
    publisher: Agent,
    publication: DataPublication,
    receiver: Agent,
    participant_id: String,
    source_room: String,
    owner: u64,
    excluded: u64,
    supplied: BTreeMap<Vec<u8>, u8>,
    received: BTreeSet<Vec<u8>>,
    sequences: BTreeSet<u64>,
    last: Option<u64>,
    pub(super) replaced: bool,
}

async fn application_agent(
    state: &mut Transition,
    handles: &PlanHandles,
    who: &'static str,
) -> Result<Agent> {
    let receipt = Arc::new(Mutex::new(None));
    handles[who]
        .shared
        .pending_ops
        .lock()
        .unwrap()
        .push(PendingDriverOp::ApplicationAgent(receipt.clone()));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(agent) = receipt.lock().unwrap().take() {
                return Ok(agent);
            }
            state.wait(handles, Duration::from_millis(10)).await?;
        }
    })
    .await
    .context("application handle handoff timed out")?
}

impl DataLane {
    fn spec(&self) -> TrackSpec {
        TrackSpec::Data {
            track_id: self.topic.clone(),
            delivery: self.delivery.mode(),
        }
    }

    fn payload(&self, tag: u8, offset: u8) -> Vec<u8> {
        format!("history:{}:{tag}:{offset}", self.delivery.name()).into_bytes()
    }

    async fn receive_owner(&self, participant_id: Option<String>) -> Result<u64> {
        let ResourceResult::Owned { owner } = self
            .receiver
            .resource(ResourceCommand::ReceiveData {
                spec: self.spec(),
                source: TrackSelector {
                    room: self.source_room.clone(),
                    participant_id: participant_id.unwrap_or_else(|| self.participant_id.clone()),
                    track_id: self.topic.clone(),
                },
            })
            .await?
        else {
            anyhow::bail!("data receive returned no owner")
        };
        Ok(owner)
    }

    pub(super) async fn declare(
        delivery: Delivery,
        state: &mut Transition,
        handles: &PlanHandles,
    ) -> Result<Self> {
        let topic = format!("history-{}", delivery.name());
        let publisher = application_agent(state, handles, "anchor").await?;
        let receiver = application_agent(state, handles, "continuing").await?;
        let participant_id = publisher
            .snapshot()
            .participant_id
            .context("data publisher external admission identity")?;
        let excluded_external_id = receiver
            .snapshot()
            .participant_id
            .context("excluded data receiver external identity")?;
        let publication = publisher.data(topic.clone(), delivery.mode())?.publish()?;
        let mut lane = Self {
            delivery,
            topic,
            publisher,
            publication,
            receiver,
            participant_id,
            source_room: handles["anchor"].room_name.to_owned(),
            owner: 0,
            excluded: 0,
            supplied: BTreeMap::new(),
            received: BTreeSet::new(),
            sequences: BTreeSet::new(),
            last: None,
            replaced: false,
        };
        lane.owner = lane.receive_owner(None).await?;
        lane.excluded = lane.receive_owner(Some(excluded_external_id)).await?;
        // Channel readiness is not seed completion or receipt. Establish service
        // with bounded, explicitly supplied warmup values before fault claims.
        tokio::time::timeout(Duration::from_secs(5), async {
            for attempt in 0..WARMUP_SENDS {
                if lane.publication.state().execution == PublicationExecution::Installed {
                    lane.publish(255, attempt).await?;
                }
                state.wait(handles, Duration::from_millis(100)).await?;
                lane.drain(handles).await?;
                if lane
                    .received
                    .iter()
                    .any(|bytes| lane.supplied.get(bytes) == Some(&255))
                {
                    return Ok::<_, anyhow::Error>(());
                }
            }
            anyhow::bail!("bounded topic warmup exhausted")
        })
        .await
        .context("topic did not establish actual payload service")??;
        // Establish current-generation video alongside DATA before holding the other path.
        let publisher_generation = lane.publisher.snapshot().generation;
        let receiver_generation = lane.receiver.snapshot().generation;
        let mut baseline = None;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                state.clients_safe(handles)?;
                if publisher_generation.is_some()
                    && receiver_generation.is_some()
                    && observation(&handles["anchor"])
                        .is_some_and(|o| o.snapshot.generation == publisher_generation)
                    && observation(&handles["continuing"])
                        .is_some_and(|o| o.snapshot.generation == receiver_generation)
                    && let Ok((source, _)) = source_context(handles, "continuing", "anchor")
                {
                    let count = handles["continuing"]
                        .video_rx()
                        .decoded_from_incarnation(&source)
                        .map_or(0, |decoded| decoded.frames);
                    if let Some((origin, before)) = &baseline
                        && *origin == source
                        && count > *before
                    {
                        return Ok::<_, anyhow::Error>(());
                    }
                    if baseline
                        .as_ref()
                        .is_none_or(|(origin, _)| *origin != source)
                    {
                        baseline = Some((source, count));
                    }
                }
                state.wait(handles, Duration::from_millis(10)).await?;
            }
        })
        .await
        .context("logical continuing video did not establish alongside DATA")??;
        Ok(lane)
    }

    async fn publish(&mut self, tag: u8, offset: u8) -> Result<()> {
        let payload = self.payload(tag, offset);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match self.publication.send(payload.clone()).await {
                    Ok(()) => break,
                    Err(Error::Core(AgentError::InvalidTopic(
                        pulsebeam_agent_native::agent_core::TopicError::SendQueueFull(_),
                    ))) => tokio::time::sleep(Duration::from_millis(10)).await,
                    Err(error) => return Err(error.into()),
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("DATA admission queue did not drain")??;
        self.supplied.insert(payload, tag);
        Ok(())
    }

    async fn drain(&mut self, handles: &PlanHandles) -> Result<()> {
        let track = observation(&handles["continuing"]).and_then(|value| {
            value
                .data_catalog
                .get(&(
                    self.participant_id.clone(),
                    TopicPublisher {
                        topic: self.topic.clone(),
                        mode: self.delivery.mode(),
                    },
                ))
                .cloned()
        });
        for _ in 0..64 {
            let ResourceResult::Event { event } = self
                .receiver
                .resource(ResourceCommand::ReadData { owner: self.owner })
                .await?
            else {
                anyhow::bail!("data read returned wrong result")
            };
            let Some(event) = event else {
                ensure!(
                    matches!(
                        self.receiver
                            .resource(ResourceCommand::ReadData {
                                owner: self.excluded,
                            })
                            .await?,
                        ResourceResult::Event { event: None }
                    ),
                    "participant-filtered owner received excluded payload"
                );
                return Ok(());
            };
            match event {
                DataEvent::Message {
                    participant_id,
                    publication_id,
                    sequence,
                    data,
                    ..
                } => {
                    ensure!(
                        participant_id == self.participant_id
                            && Some(&publication_id) == track.as_ref(),
                        "data payload identity ({participant_id}, {publication_id}) differs from external publisher {} / track {track:?}",
                        self.participant_id
                    );
                    ensure!(
                        self.supplied.contains_key(&data),
                        "data payload was never supplied"
                    );
                    ensure!(
                        self.sequences.insert(sequence),
                        "duplicate data sequence in bounded history"
                    );
                    match self.delivery {
                        Delivery::Latest => ensure!(
                            self.last.is_none_or(|last| sequence > last),
                            "latest data regressed"
                        ),
                        Delivery::Lossy => {}
                    }
                    if self.delivery != Delivery::Lossy {
                        self.last = Some(sequence);
                    }
                    self.received.insert(data);
                }
                DataEvent::Gap { .. } => {
                    anyhow::bail!("unexpected DATA gap in a latest or lossy lane")
                }
                DataEvent::Discontinuity { .. } => {
                    anyhow::bail!("unexpected DATA source change in a fixed-source history")
                }
            }
        }
        anyhow::bail!("data events exceeded bounded supplied history")
    }

    pub(super) async fn send(
        &mut self,
        tag: u8,
        state: &mut Transition,
        handles: &PlanHandles,
    ) -> Result<()> {
        for offset in 0..BURST {
            self.publish(tag, offset).await?;
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                state.clients_safe(handles)?;
                self.drain(handles).await?;
                let delivered = |offset| self.received.contains(&self.payload(tag, offset));
                let complete = match self.delivery {
                    Delivery::Latest => delivered(BURST - 1),
                    Delivery::Lossy => (0..BURST).any(delivered),
                };
                if complete {
                    return Ok::<_, anyhow::Error>(());
                }
                state.wait(handles, Duration::from_millis(10)).await?;
            }
        })
        .await
        .context("established clean topic burst did not deliver its mode contract")??;
        Ok(())
    }

    pub(super) async fn replace(&mut self) -> Result<()> {
        let replacement = self.receive_owner(None).await?;
        ensure!(
            replacement != self.owner,
            "data replacement reused active owner"
        );
        self.receiver
            .resource(ResourceCommand::Release { owner: self.owner })
            .await?;
        ensure!(
            matches!(
                self.receiver
                    .resource(ResourceCommand::ReadData { owner: self.owner })
                    .await,
                Err(Error::Core(AgentError::InvalidResource(
                    TrackResourceError::Released
                )))
            ),
            "released data owner remained readable"
        );
        self.owner = replacement;
        self.replaced = true;
        self.last = None;
        self.sequences.clear();
        self.received.clear();
        Ok(())
    }
}
