use super::{Loss, ParticipantCmd, PendingDriverOp, PlanHandles, PlanIps};
use anyhow::{Context, Result, ensure};
use pulsebeam_agent_native::agent_core::{TopicMode, TopicNotification};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

const PHASE: Duration = Duration::from_secs(30);
const MAX_SETUP_SENDS: usize = 30;
const PROBE_PERIOD: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Receipt {
    Message(u64),
    Gap(u64, u64),
}

struct Transcript {
    topic: String,
    publisher: String,
    track: String,
    sent: Vec<Vec<u8>>,
}

impl Transcript {
    fn receipts(&self, events: &[TopicNotification]) -> Result<Vec<Receipt>> {
        events
            .iter()
            .map(|event| match event {
                TopicNotification::Message(message) => {
                    ensure!(
                        message.topic == self.topic
                            && message.mode == TopicMode::Ordered
                            && message.participant_id == self.publisher
                            && message.data_track_id == self.track,
                        "ordered message has wrong topic, mode or source: {message:?}"
                    );
                    let index = usize::try_from(message.sequence)?;
                    ensure!(
                        self.sent.get(index) == Some(&message.payload),
                        "ordered message does not match submitted input at {}: {message:?}",
                        message.sequence
                    );
                    Ok(Receipt::Message(message.sequence))
                }
                TopicNotification::RecoveryGap {
                    subscriber,
                    data_track_id,
                    participant_id,
                    expected_sequence,
                    available_sequence,
                } => {
                    ensure!(
                        subscriber.topic == self.topic
                            && subscriber.mode == TopicMode::Ordered
                            && *data_track_id == self.track
                            && *participant_id == self.publisher,
                        "ordered gap has wrong topic, mode or source: {event:?}"
                    );
                    Ok(Receipt::Gap(*expected_sequence, *available_sequence))
                }
                _ => anyhow::bail!("unexpected subscriber notification: {event:?}"),
            })
            .collect()
    }

    fn expected(&self, first: u64, end: usize, gap: Option<(u64, u64)>) -> Vec<Receipt> {
        let mut expected = Vec::new();
        for sequence in first..u64::try_from(end).unwrap() {
            if let Some((missing, available)) = gap {
                if sequence == missing {
                    expected.push(Receipt::Gap(missing, available));
                }
                if (missing..available).contains(&sequence) {
                    continue;
                }
            }
            expected.push(Receipt::Message(sequence));
        }
        expected
    }

    fn check(&self, events: &[TopicNotification], expected: &[Receipt]) -> Result<bool> {
        let actual = self.receipts(events)?;
        ensure!(
            expected.starts_with(&actual),
            "ordered trace differs: actual={actual:?}, expected={expected:?}"
        );
        Ok(actual.len() == expected.len())
    }

    fn setup_floor(&self, events: &[TopicNotification], earliest: u64) -> Result<Option<u64>> {
        let actual = self.receipts(events)?;
        let Some(Receipt::Message(first)) = actual.first() else {
            ensure!(
                actual.is_empty(),
                "gap before first setup receipt: {actual:?}"
            );
            return Ok(None);
        };
        ensure!(
            *first >= earliest,
            "subscriber replayed pre-join history: {actual:?}"
        );
        self.check(events, &self.expected(*first, self.sent.len(), None))?;
        Ok(Some(*first))
    }
}

struct Scenario<'a> {
    handles: &'a mut PlanHandles,
    ips: &'a PlanIps,
    publisher: &'static str,
    transcript: Transcript,
}

impl Scenario<'_> {
    async fn bind_publisher(&mut self, deadline: Instant) -> Result<()> {
        loop {
            ensure!(
                Instant::now() < deadline,
                "ordered publisher did not become available"
            );
            let binding = {
                let observation = self.handles[self.publisher]
                    .shared
                    .observation
                    .lock()
                    .unwrap();
                observation.as_ref().and_then(|observed| {
                    let status = observed.snapshot.topics.publishers.iter().find(|status| {
                        status.registration.topic == self.transcript.topic
                            && status.registration.mode == TopicMode::Ordered
                    })?;
                    Some((
                        observed.snapshot.participant_external_id.clone()?,
                        status.data_track_id.clone()?,
                        status.next_sequence,
                    ))
                })
            };
            if let Some((publisher, track, next)) = binding {
                ensure!(
                    next == 0,
                    "ordered fixture publication already has submitted messages"
                );
                self.transcript.publisher = publisher;
                self.transcript.track = track;
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn send(&mut self, payload: Vec<u8>, deadline: Instant) -> Result<()> {
        self.handles[self.publisher]
            .shared
            .pending_ops
            .lock()
            .unwrap()
            .push(PendingDriverOp::PublishData(
                self.transcript.topic.clone(),
                TopicMode::Ordered,
                payload.clone(),
            ));
        self.transcript.sent.push(payload);
        let target = u64::try_from(self.transcript.sent.len())?;
        loop {
            ensure!(
                Instant::now() < deadline,
                "ordered input {target} did not finish local send"
            );
            let settled = {
                let observation = self.handles[self.publisher]
                    .shared
                    .observation
                    .lock()
                    .unwrap();
                if let Some(status) = observation.as_ref().and_then(|observed| {
                    observed.snapshot.topics.publishers.iter().find(|status| {
                        status.registration.topic == self.transcript.topic
                            && status.registration.mode == TopicMode::Ordered
                    })
                }) {
                    ensure!(
                        status.data_track_id.as_ref() == Some(&self.transcript.track)
                            && status.next_sequence <= target,
                        "ordered publisher identity or input sequence changed"
                    );
                    status.next_sequence == target
                        && !status.send_pending
                        && status.queued_messages == 0
                } else {
                    false
                }
            };
            if settled {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn check(&self, receiver: &str, expected: &[Receipt]) -> Result<bool> {
        let events = self.handles[receiver].shared.topic_events.lock().unwrap();
        self.transcript
            .check(
                events
                    .get(&self.transcript.topic)
                    .map_or(&[], Vec::as_slice),
                expected,
            )
            .with_context(|| format!("ordered receiver {receiver}"))
    }

    async fn wait(&self, expected: &BTreeMap<&str, Vec<Receipt>>, deadline: Instant) -> Result<()> {
        loop {
            ensure!(
                Instant::now() < deadline,
                "ordered receivers did not reach expected traces: {expected:?}"
            );
            let mut complete = true;
            for (receiver, trace) in expected {
                complete &= self.check(receiver, trace)?;
            }
            if complete {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn setup(
        &mut self,
        receivers: &[&'static str],
        phase: &str,
        deadline: Instant,
    ) -> Result<BTreeMap<&'static str, u64>> {
        let earliest = u64::try_from(self.transcript.sent.len())?;
        let mut floors = BTreeMap::new();
        for probe in 0..MAX_SETUP_SENDS {
            self.send(format!("setup:{phase}:{probe}").into_bytes(), deadline)
                .await?;
            let next_probe = (Instant::now() + PROBE_PERIOD).min(deadline);
            loop {
                for receiver in receivers {
                    let events = self.handles[*receiver].shared.topic_events.lock().unwrap();
                    if let Some(first) = self.transcript.setup_floor(
                        events
                            .get(&self.transcript.topic)
                            .map_or(&[], Vec::as_slice),
                        earliest,
                    )? {
                        floors.insert(*receiver, first);
                    }
                }
                if floors.len() == receivers.len() {
                    let expected = self.expected(&floors, None, &[]);
                    self.wait(&expected, deadline).await?;
                    return Ok(floors);
                }
                if Instant::now() >= next_probe {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        anyhow::bail!("ordered {phase} setup exhausted {MAX_SETUP_SENDS} explicit probes")
    }

    fn expected<'a>(
        &self,
        floors: &BTreeMap<&'a str, u64>,
        gap: Option<(u64, u64)>,
        recovering: &[&str],
    ) -> BTreeMap<&'a str, Vec<Receipt>> {
        floors
            .iter()
            .map(|(receiver, first)| {
                (
                    *receiver,
                    self.transcript.expected(
                        *first,
                        self.transcript.sent.len(),
                        if recovering.contains(receiver) {
                            gap
                        } else {
                            None
                        },
                    ),
                )
            })
            .collect()
    }

    fn loss(&self, receivers: &[&str], loss: Loss) {
        for receiver in receivers {
            pulsebeam_runtime::net::shaper::set_loss(self.ips[*receiver], loss);
        }
    }
}

pub(super) async fn execute(
    handles: &mut PlanHandles,
    ips: &PlanIps,
    publisher: &'static str,
    topic: &'static str,
    recovering: [&'static str; 2],
    late: &'static str,
) -> Result<()> {
    let mut scenario = Scenario {
        handles,
        ips,
        publisher,
        transcript: Transcript {
            topic: topic.into(),
            publisher: String::new(),
            track: String::new(),
            sent: Vec::new(),
        },
    };
    let initial_deadline = Instant::now() + PHASE;
    scenario.bind_publisher(initial_deadline).await?;
    let mut floors = scenario
        .setup(&recovering, "initial", initial_deadline)
        .await?;
    scenario
        .send(b"first".to_vec(), Instant::now() + PHASE)
        .await?;
    scenario
        .wait(
            &scenario.expected(&floors, None, &[]),
            Instant::now() + PHASE,
        )
        .await?;

    scenario.loss(&recovering, Loss::Independent(1.0));
    let before_tail = scenario.expected(&floors, None, &[]);
    scenario
        .send(b"lost-tail".to_vec(), Instant::now() + PHASE)
        .await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    for (receiver, expected) in &before_tail {
        ensure!(
            scenario.check(receiver, expected)?,
            "downlink loss changed an established prefix"
        );
    }
    scenario.loss(&recovering, Loss::wifi());
    scenario
        .wait(
            &scenario.expected(&floors, None, &[]),
            Instant::now() + PHASE,
        )
        .await?;
    scenario
        .send(b"later".to_vec(), Instant::now() + PHASE)
        .await?;
    scenario
        .wait(
            &scenario.expected(&floors, None, &[]),
            Instant::now() + PHASE,
        )
        .await?;

    let late_handle = scenario
        .handles
        .get_mut(late)
        .context("late receiver is missing")?;
    late_handle.present = true;
    late_handle.send_command(ParticipantCmd::Reconnect);
    floors.extend(
        scenario
            .setup(&[late], "late", Instant::now() + PHASE)
            .await?,
    );
    scenario
        .send(b"live".to_vec(), Instant::now() + PHASE)
        .await?;
    scenario
        .wait(
            &scenario.expected(&floors, None, &[]),
            Instant::now() + PHASE,
        )
        .await?;

    scenario.loss(&recovering, Loss::Independent(1.0));
    let before_window = scenario.expected(&floors, None, &[]);
    let missing = u64::try_from(scenario.transcript.sent.len())?;
    let retained = pulsebeam_agent_native::agent_core::TOPIC_HISTORY_CAPACITY;
    ensure!(
        MAX_SETUP_SENDS < retained,
        "setup probes must fit retained history"
    );
    let deadline = Instant::now() + PHASE;
    for index in 0..retained + 44 {
        scenario
            .send(format!("window:{index}").into_bytes(), deadline)
            .await?;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    for receiver in recovering {
        ensure!(
            scenario.check(receiver, &before_window[receiver])?,
            "impaired receiver changed before path restoration"
        );
    }
    let available = u64::try_from(scenario.transcript.sent.len() - retained)?;
    scenario.loss(&recovering, Loss::wifi());
    let gap = Some((missing, available));
    scenario
        .wait(
            &scenario.expected(&floors, gap, &recovering),
            Instant::now() + PHASE,
        )
        .await?;
    scenario
        .send(b"continued".to_vec(), Instant::now() + PHASE)
        .await?;
    scenario
        .wait(
            &scenario.expected(&floors, gap, &recovering),
            Instant::now() + PHASE,
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    for (receiver, expected) in scenario.expected(&floors, gap, &recovering) {
        ensure!(
            scenario.check(receiver, &expected)?,
            "terminal ordered trace is incomplete"
        );
    }
    println!(
        "[ordered-recovery] submitted={} retained={retained} exact_gap={gap:?} floors={floors:?}",
        scenario.transcript.sent.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_agent_native::agent_core::{TopicMessage, TopicSubscriber};

    fn transcript() -> Transcript {
        Transcript {
            topic: "state".into(),
            publisher: "publisher".into(),
            track: "track".into(),
            sent: (0..6)
                .map(|sequence| format!("input:{sequence}").into_bytes())
                .collect(),
        }
    }

    fn message(sequence: u64) -> TopicNotification {
        TopicNotification::Message(TopicMessage {
            topic: "state".into(),
            mode: TopicMode::Ordered,
            participant_id: "publisher".into(),
            data_track_id: "track".into(),
            sequence,
            payload: format!("input:{sequence}").into_bytes(),
        })
    }

    fn gap(expected_sequence: u64, available_sequence: u64) -> TopicNotification {
        TopicNotification::RecoveryGap {
            subscriber: TopicSubscriber {
                topic: "state".into(),
                mode: TopicMode::Ordered,
            },
            data_track_id: "track".into(),
            participant_id: "publisher".into(),
            expected_sequence,
            available_sequence,
        }
    }

    #[test]
    fn exact_trace_distinguishes_incomplete_tail_from_interior_loss_and_replay() {
        let input = transcript();
        let expected = input.expected(0, 3, None);
        assert!(!input.check(&[message(0), message(1)], &expected).unwrap());
        assert!(
            input
                .check(&[message(0), message(1), message(2)], &expected)
                .unwrap()
        );
        for invalid in [
            vec![message(0), message(2)],
            vec![message(0), message(0)],
            vec![message(1)],
            vec![message(0), message(1), message(2), message(3)],
        ] {
            assert!(input.check(&invalid, &expected).is_err());
        }
    }

    #[test]
    fn gap_range_and_position_are_exact_and_never_apply_to_healthy_receivers() {
        let input = transcript();
        let expected = input.expected(0, 6, Some((2, 4)));
        let valid = vec![message(0), message(1), gap(2, 4), message(4), message(5)];
        assert!(input.check(&valid, &expected).unwrap());
        assert!(input.check(&valid, &input.expected(0, 6, None)).is_err());
        for invalid in [
            vec![message(0), gap(2, 4)],
            vec![message(0), message(1), gap(2, 5)],
            vec![message(0), message(1), gap(2, 4), gap(2, 4)],
        ] {
            assert!(input.check(&invalid, &expected).is_err());
        }
    }

    #[test]
    fn setup_accepts_only_an_attributed_contiguous_post_join_suffix() {
        let input = transcript();
        assert_eq!(input.setup_floor(&[], 3).unwrap(), None);
        assert_eq!(
            input.setup_floor(&[message(4), message(5)], 3).unwrap(),
            Some(4)
        );
        assert!(input.setup_floor(&[message(2)], 3).is_err());
        assert!(input.setup_floor(&[gap(3, 4)], 3).is_err());
        assert!(input.setup_floor(&[message(3), message(5)], 3).is_err());
    }

    #[test]
    fn substituted_content_and_source_never_pass_a_matching_sequence() {
        let input = transcript();
        for field in 0..5 {
            let TopicNotification::Message(mut value) = message(0) else {
                unreachable!()
            };
            match field {
                0 => value.payload = b"substituted".to_vec(),
                1 => value.data_track_id = "replacement".into(),
                2 => value.participant_id = "bystander".into(),
                3 => value.topic = "other".into(),
                _ => value.mode = TopicMode::Latest,
            }
            assert!(
                input
                    .check(&[TopicNotification::Message(value)], &[Receipt::Message(0)])
                    .is_err()
            );
        }
    }
}
