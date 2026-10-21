use core::time::Duration;

use alloc::{collections::VecDeque, string::String, vec::Vec};

use super::{
    MAX_TOPIC_PAYLOAD_BYTES, TOPIC_SEND_QUEUE_CAPACITY, TopicError, TopicPublisher, TopicSend,
    Topics, validate_topic,
};
use crate::{
    ConnectionState, Effect, Generation, Notification, OperationId, Snapshot, TimerEffect, TimerId,
    id::IdGenerator,
};

const SEND_ADMISSION_WINDOW: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DataSendFailure {
    #[error("data send was disconnected")]
    Disconnected,
    #[error("data send failed with the connection")]
    TerminalFailure,
    #[error("data send admission timed out")]
    TimedOut,
    #[error("data send generation or authoritative binding was replaced")]
    TransportReplaced,
    #[error("data send channel failed")]
    ChannelFailed,
    #[error("data send was superseded by a newer value")]
    Superseded,
    #[error("data send was rejected by the host")]
    HostRejected,
    #[error("data send queue is full")]
    QueueFull,
    #[error("data send sequence space is exhausted")]
    SequenceExhausted,
}

pub(super) struct Admission {
    publisher: TopicPublisher,
    payload: Option<Vec<u8>>,
    generation: Option<Generation>,
    binding: Option<String>,
    timer: Option<TimerId>,
}

impl Topics {
    pub(crate) fn validate_data_send(&self, send: &TopicSend) -> Result<(), TopicError> {
        validate_topic(&send.publisher.topic)
            .map_err(|_| TopicError::InvalidTopic(send.publisher.topic.clone()))?;
        if send.payload.len() > MAX_TOPIC_PAYLOAD_BYTES {
            return Err(TopicError::PayloadTooLarge {
                actual: send.payload.len(),
                maximum: MAX_TOPIC_PAYLOAD_BYTES,
            });
        }
        let admissions = self
            .admissions
            .values()
            .filter(|a| a.publisher == send.publisher)
            .count();
        let native = self.publishers.get(&send.publisher).map_or(0, |state| {
            state
                .queue
                .iter()
                .filter(|message| message.admission.is_none())
                .count()
                .saturating_add(usize::from(
                    state
                        .pending
                        .as_ref()
                        .is_some_and(|pending| pending.admission.is_none()),
                ))
        });
        if admissions.saturating_add(native) >= TOPIC_SEND_QUEUE_CAPACITY {
            return Err(TopicError::SendQueueFull(send.publisher.topic.clone()));
        }
        Ok(())
    }

    pub(super) fn waiting_data_sends(&self, publisher: &TopicPublisher) -> usize {
        self.admissions
            .values()
            .filter(|admission| &admission.publisher == publisher && admission.payload.is_some())
            .count()
    }

    pub(crate) fn begin_data_send(
        &mut self,
        send: TopicSend,
        ids: &mut IdGenerator,
        effects: &mut VecDeque<Effect>,
    ) -> OperationId {
        let ticket = ids.operation();
        let timer = ids.timer();
        self.admissions.insert(
            ticket,
            Admission {
                publisher: send.publisher,
                payload: Some(send.payload),
                generation: None,
                binding: None,
                timer: Some(timer),
            },
        );
        effects.push_back(Effect::Timer(TimerEffect::Schedule {
            timer,
            after: SEND_ADMISSION_WINDOW,
        }));
        ticket
    }

    pub(crate) fn progress_data_sends(
        &mut self,
        connected_intent: bool,
        ids: &mut IdGenerator,
        effects: &mut VecDeque<Effect>,
        snapshot: &mut Snapshot,
        notifications: &mut VecDeque<Notification>,
    ) {
        if self.admissions.is_empty() {
            return;
        }
        let tickets = self.admissions.keys().copied().collect::<Vec<_>>();
        for ticket in tickets {
            let Some(admission) = self.admissions.get(&ticket) else {
                continue;
            };
            let publisher = admission.publisher.clone();
            let generation = self.active.as_ref().map(|active| active.generation);
            let channel = self.channel_for_publisher(&publisher);
            let binding = self
                .publishers
                .get(&publisher)
                .and_then(|state| state.data_track_id.clone());
            let failure = if !connected_intent {
                Some(DataSendFailure::Disconnected)
            } else if snapshot.terminal_failure.is_some() {
                Some(DataSendFailure::TerminalFailure)
            } else if admission.generation.is_some() && admission.generation != generation
                || admission.binding.is_some() && admission.binding != binding
            {
                Some(DataSendFailure::TransportReplaced)
            } else if admission.generation.is_some() && channel.is_none() {
                Some(DataSendFailure::ChannelFailed)
            } else if admission.payload.is_none()
                && !self.publishers.get(&publisher).is_some_and(|state| {
                    state
                        .pending
                        .as_ref()
                        .is_some_and(|pending| pending.admission == Some(ticket))
                        || state
                            .queue
                            .iter()
                            .any(|message| message.admission == Some(ticket))
                })
            {
                Some(DataSendFailure::Superseded)
            } else {
                None
            };
            if let Some(error) = failure {
                self.finish_data_send(ticket, Some(error), effects, notifications);
                continue;
            }
            let Some(admission) = self.admissions.get_mut(&ticket) else {
                continue;
            };
            if channel.is_some() || binding.is_some() {
                admission.generation = generation;
            }
            if binding.is_some() {
                admission.binding.clone_from(&binding);
            }
            if snapshot.connection != ConnectionState::Connected
                || channel.is_none()
                || binding.is_none()
            {
                continue;
            }
            let Some(payload) = admission.payload.take() else {
                continue;
            };
            if let Err(error) = self.send_owned(
                TopicSend { publisher, payload },
                Some(ticket),
                ids,
                effects,
                snapshot,
                notifications,
            ) {
                let error = match error {
                    TopicError::SendQueueFull(_) => DataSendFailure::QueueFull,
                    TopicError::SequenceExhausted(_) => DataSendFailure::SequenceExhausted,
                    _ => DataSendFailure::ChannelFailed,
                };
                self.finish_data_send(ticket, Some(error), effects, notifications);
            }
        }
        // A latest enqueue may supersede an earlier ticket processed in this turn.
        let superseded = self
            .admissions
            .iter()
            .filter_map(|(ticket, admission)| {
                (admission.payload.is_none()
                    && self
                        .publishers
                        .get(&admission.publisher)
                        .is_some_and(|state| {
                            !state
                                .pending
                                .as_ref()
                                .is_some_and(|pending| pending.admission == Some(*ticket))
                                && !state
                                    .queue
                                    .iter()
                                    .any(|message| message.admission == Some(*ticket))
                        }))
                .then_some(*ticket)
            })
            .collect::<Vec<_>>();
        for ticket in superseded {
            self.finish_data_send(
                ticket,
                Some(DataSendFailure::Superseded),
                effects,
                notifications,
            );
        }
        self.refresh_snapshot(snapshot);
    }

    pub(crate) fn expire_data_send(
        &mut self,
        timer: TimerId,
        effects: &mut VecDeque<Effect>,
        snapshot: &mut Snapshot,
        notifications: &mut VecDeque<Notification>,
    ) {
        if let Some(ticket) = self
            .admissions
            .iter()
            .find_map(|(ticket, admission)| (admission.timer == Some(timer)).then_some(*ticket))
        {
            self.finish_data_send(
                ticket,
                Some(DataSendFailure::TimedOut),
                effects,
                notifications,
            );
            self.refresh_snapshot(snapshot);
        }
    }

    pub(super) fn dispatch_data_send(
        &mut self,
        ticket: OperationId,
        effects: &mut VecDeque<Effect>,
    ) {
        if let Some(timer) = self
            .admissions
            .get_mut(&ticket)
            .and_then(|admission| admission.timer.take())
        {
            effects.push_back(Effect::Timer(TimerEffect::Cancel { timer }));
        }
    }

    pub(super) fn fail_publisher_sends(
        &mut self,
        publisher: &TopicPublisher,
        error: DataSendFailure,
        effects: &mut VecDeque<Effect>,
        notifications: &mut VecDeque<Notification>,
    ) {
        let tickets = self
            .admissions
            .iter()
            .filter_map(|(ticket, admission)| {
                (&admission.publisher == publisher).then_some(*ticket)
            })
            .collect::<Vec<_>>();
        for ticket in tickets {
            self.finish_data_send(ticket, Some(error), effects, notifications);
        }
    }

    pub(super) fn finish_data_send(
        &mut self,
        ticket: OperationId,
        error: Option<DataSendFailure>,
        effects: &mut VecDeque<Effect>,
        notifications: &mut VecDeque<Notification>,
    ) {
        let Some(admission) = self.admissions.remove(&ticket) else {
            return;
        };
        if let Some(state) = self.publishers.get_mut(&admission.publisher) {
            state
                .queue
                .retain(|message| message.admission != Some(ticket));
            if let Some(pending) = state
                .pending
                .as_mut()
                .filter(|pending| pending.admission == Some(ticket))
            {
                pending.admission = None;
            }
        }
        if let Some(timer) = admission.timer {
            effects.push_back(Effect::Timer(TimerEffect::Cancel { timer }));
        }
        notifications.push_back(Notification::DataSendFinished {
            send: ticket,
            error,
        });
    }
}
