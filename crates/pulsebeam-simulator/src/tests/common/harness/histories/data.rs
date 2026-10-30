use super::*;
use pulsebeam_agent_native::agent_core::{
    AgentError, DataEvent, ResourceCommand, ResourceResult, TopicPublisher, TopicSubscriber,
    TrackResourceError, TrackSpec,
};
use pulsebeam_agent_native::{Agent, Error};

const BURST: u8 = 4;
const WARMUP_SENDS: u8 = 50;

pub(super) struct DataLane {
    delivery: Delivery,
    topic: String,
    publisher: Agent,
    receiver: Agent,
    participant_external_id: String,
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
            label: self.topic.clone(),
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
                participant_id,
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
        handles["anchor"].shared.pending_ops.lock().unwrap().push(
            PendingDriverOp::DeclarePublishTopic(topic.clone(), delivery.mode()),
        );
        handles["continuing"]
            .shared
            .pending_ops
            .lock()
            .unwrap()
            .push(PendingDriverOp::DeclareSubscribeTopic(
                topic.clone(),
                delivery.mode(),
            ));
        let publisher = application_agent(state, handles, "anchor").await?;
        let receiver = application_agent(state, handles, "continuing").await?;
        let participant_external_id = publisher
            .snapshot()
            .participant_external_id
            .context("data publisher external admission identity")?;
        let excluded_external_id = receiver
            .snapshot()
            .participant_external_id
            .context("excluded data receiver external identity")?;
        let mut lane = Self {
            delivery,
            topic,
            publisher,
            receiver,
            participant_external_id,
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
                let source_ready =
                    lane.publisher
                        .snapshot()
                        .topics
                        .publishers
                        .iter()
                        .any(|status| {
                            status.registration
                                == TopicPublisher {
                                    topic: lane.topic.clone(),
                                    mode: delivery.mode(),
                                }
                                && status.channel.is_some()
                                && status.data_track_id.is_some()
                        });
                let receiver_ready =
                    lane.receiver
                        .snapshot()
                        .topics
                        .subscribers
                        .iter()
                        .any(|status| {
                            status.registration
                                == TopicSubscriber {
                                    topic: lane.topic.clone(),
                                    mode: delivery.mode(),
                                }
                                && status.channel.is_some()
                        });
                if source_ready && receiver_ready {
                    lane.publish(255, attempt).await?;
                }
                state.wait(handles, Duration::from_millis(100)).await?;
                lane.drain().await?;
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
        // Topic registration replaces the native transport. Establish fresh
        // current-generation video before calling the later held path unrelated.
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
        .context(
            "logical continuing video did not establish after topic transport replacement",
        )??;
        Ok(lane)
    }

    async fn publish(&mut self, tag: u8, offset: u8) -> Result<()> {
        let payload = self.payload(tag, offset);
        self.supplied.insert(payload.clone(), tag);
        self.publisher
            .send_topic(pulsebeam_agent_native::agent_core::TopicSend {
                publisher: TopicPublisher {
                    topic: self.topic.clone(),
                    mode: self.delivery.mode(),
                },
                payload,
            })
            .await?;
        Ok(())
    }

    async fn drain(&mut self) -> Result<()> {
        let track = self
            .publisher
            .snapshot()
            .topics
            .publishers
            .iter()
            .find(|status| {
                status.registration
                    == TopicPublisher {
                        topic: self.topic.clone(),
                        mode: self.delivery.mode(),
                    }
            })
            .and_then(|status| status.data_track_id.clone());
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
                    track_id,
                    sequence,
                    data,
                } => {
                    ensure!(
                        participant_id == self.participant_external_id
                            && Some(&track_id) == track.as_ref(),
                        "data payload identity ({participant_id}, {track_id}) differs from external publisher {} / track {track:?}",
                        self.participant_external_id
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
                        Delivery::Ordered => ensure!(
                            self.last
                                .is_none_or(|last| last.checked_add(1) == Some(sequence)),
                            "ordered payload skipped without a gap"
                        ),
                        Delivery::Lossy => {}
                    }
                    if self.delivery != Delivery::Lossy {
                        self.last = Some(sequence);
                    }
                    self.received.insert(data);
                }
                DataEvent::Gap {
                    participant_id,
                    track_id,
                    from,
                    to,
                } => {
                    ensure!(
                        self.delivery == Delivery::Ordered
                            && participant_id == self.participant_external_id
                            && Some(&track_id) == track.as_ref()
                            && to > from
                            && self
                                .last
                                .is_none_or(|last| last.checked_add(1) == Some(from)),
                        "invalid ordered gap"
                    );
                    self.last = Some(to - 1);
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
                self.drain().await?;
                let delivered = |offset| self.received.contains(&self.payload(tag, offset));
                let complete = match self.delivery {
                    Delivery::Latest => delivered(BURST - 1),
                    Delivery::Lossy => (0..BURST).any(delivered),
                    Delivery::Ordered => (0..BURST).all(delivered),
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
