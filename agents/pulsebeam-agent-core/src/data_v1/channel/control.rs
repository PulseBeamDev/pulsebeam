use core::{num::NonZeroU64, time::Duration};

use pulsebeam_proto::{
    data_control::{
        self, ControlOutcome, ExpectationOutcome, PublisherObservation, SubscriberBinding,
        SubscriberControl, SubscriberObservation, publisher_observation, subscriber_control,
        subscriber_observation,
    },
    data_endpoint::Endpoint,
    state::{NativeDataControl, OwnedDataControl, native_data_control},
};

use crate::{ChannelId, Generation};

use super::{
    AssembledUnit, Error, PublisherMode, QueuedOriginal, ReceiveChannel, SendChannel, SendKind,
    record_capacity,
};
use crate::data_v1::{PublisherDelivery, ReceiptAction};

#[derive(Debug, PartialEq)]
pub enum PublisherControl {
    Admitted { unit_seq: u64 },
    Delivery(PublisherDelivery),
    Reply(NativeDataControl),
    Ignored,
}

impl SendChannel {
    pub fn handle_publisher_control(
        &mut self,
        generation: Generation,
        signaling: ChannelId,
        frame: NativeDataControl,
        now: Duration,
    ) -> Result<PublisherControl, Error> {
        let PublisherMode::Reliable { publisher, control } = &mut self.publisher else {
            return Err(Error::Binding);
        };
        if self.closed
            || self.channel.binding.channel.association != generation
            || control.signaling != signaling
        {
            return Err(Error::Binding);
        }
        let Some(native_data_control::Endpoint::Publisher(frame)) = frame.endpoint else {
            return Err(Error::Binding);
        };
        if frame.owned != control.owned.get() {
            return Err(Error::Binding);
        }
        let Endpoint::Publisher(endpoint) = self.channel.binding.endpoint else {
            return Err(Error::Binding);
        };
        let observation = data_control::decode::<PublisherObservation>(endpoint, &frame.record)
            .map_err(Error::Control)?;
        let result = match observation.record {
            Some(publisher_observation::Record::Admission(ack)) => {
                let unit_seq = ack
                    .unit_seq
                    .ok_or(Error::Control(data_control::Error::Structure))?;
                if publisher.acknowledge(self.channel.binding, ack, now)? {
                    PublisherControl::Admitted { unit_seq }
                } else {
                    PublisherControl::Ignored
                }
            }
            Some(publisher_observation::Record::Delivery(outcome)) => publisher
                .observe_delivery(self.channel.binding, outcome, now)?
                .map_or(PublisherControl::Ignored, PublisherControl::Delivery),
            Some(publisher_observation::Record::RequestOriginal(demand)) => {
                let now = self.now.max(now);
                if self
                    .original
                    .as_ref()
                    .is_some_and(|original| original.expires <= now)
                {
                    self.original = None;
                }
                // Sources retry on loss. Coalesce against the single queued or in-flight original.
                if self.original.is_some()
                    || self
                        .pending
                        .is_some_and(|(_, kind)| kind == SendKind::Original)
                {
                    return Ok(PublisherControl::Ignored);
                }
                let mut payload = alloc::vec![0; record_capacity(self.channel.record_dd)];
                if let Some((size, expires)) = publisher.write_original_until(
                    self.channel.binding,
                    demand,
                    &mut payload,
                    now,
                )? {
                    payload.truncate(size);
                    self.original = Some(QueuedOriginal { expires, payload });
                    PublisherControl::Ignored
                } else {
                    let unavailable = data_control::PublisherControl {
                        record: Some(data_control::publisher_control::Record::Unavailable(
                            data_control::OriginalUnavailable {
                                demand: Some(demand),
                            },
                        )),
                    };
                    PublisherControl::Reply(NativeDataControl {
                        endpoint: Some(native_data_control::Endpoint::Publisher(
                            OwnedDataControl {
                                owned: control.owned.get(),
                                record: data_control::encode(endpoint, &unavailable)
                                    .map_err(Error::Control)?,
                            },
                        )),
                    })
                }
            }
            None => return Err(Error::Control(data_control::Error::Structure)),
        };
        Ok(result)
    }
}

#[derive(Debug, PartialEq)]
pub enum ReceiverControl {
    Expected {
        response: NativeDataControl,
        assembled: Option<AssembledUnit>,
    },
    ReceiptAck {
        settled: bool,
    },
    Ignored,
}

#[derive(Debug, PartialEq)]
pub enum ReceiverReceipt {
    Send(NativeDataControl),
    Unconfirmed { unit_seq: u64 },
}

impl ReceiveChannel {
    /// `control` has been decoded by the session's explicit Native frame
    /// decoder. Recheck the current SCTP association and owned receiver here.
    pub fn handle_control(
        &mut self,
        generation: Generation,
        signaling: ChannelId,
        control: NativeDataControl,
        now: Duration,
    ) -> Result<ReceiverControl, Error> {
        let lease = self.control.as_ref().ok_or(Error::Binding)?;
        if self.closed
            || generation != self.channel.binding.channel.association
            || signaling != lease.signaling
        {
            return Err(Error::Binding);
        }
        let Some(native_data_control::Endpoint::Subscriber(frame)) = control.endpoint else {
            return Err(Error::Binding);
        };
        if frame.owned != lease.owned.get() {
            return Err(Error::Binding);
        }
        let Endpoint::Subscriber(endpoint) = self.channel.binding.endpoint else {
            return Err(Error::Binding);
        };
        let observation = data_control::decode::<SubscriberObservation>(endpoint, &frame.record)
            .map_err(Error::Control)?;
        match observation.record {
            Some(subscriber_observation::Record::Expectation(expectation)) => {
                let (outcome, assembled) =
                    match self
                        .subscriber
                        .expect(self.channel.binding, expectation, now)
                    {
                        Ok(assembled) => (ControlOutcome::Confirmed, assembled),
                        Err(Error::Capacity | Error::Conflict) => (ControlOutcome::Failed, None),
                        Err(Error::Retired) => (ControlOutcome::Unknown, None),
                        Err(error) => return Err(error),
                    };
                let reply = SubscriberControl {
                    record: Some(subscriber_control::Record::ExpectationOutcome(
                        ExpectationOutcome {
                            expectation: Some(expectation),
                            outcome: outcome as i32,
                        },
                    )),
                };
                Ok(ReceiverControl::Expected {
                    response: encode(lease.owned, endpoint, &reply)?,
                    assembled,
                })
            }
            Some(subscriber_observation::Record::ReceiptAck(expectation)) => {
                let settled =
                    self.subscriber
                        .receipt_ack(self.channel.binding, expectation, now)?;
                Ok(ReceiverControl::ReceiptAck { settled })
            }
            Some(subscriber_observation::Record::Repair(_)) => Ok(ReceiverControl::Ignored),
            None => Err(Error::Control(data_control::Error::Structure)),
        }
    }

    pub(crate) fn repair_deadline(&self) -> Option<Duration> {
        self.control.as_ref().filter(|_| !self.closed)?;
        self.subscriber.repair_deadline()
    }

    pub(crate) fn next_repair_control(
        &mut self,
        now: Duration,
    ) -> Result<Option<NativeDataControl>, Error> {
        let Some(lease) = self.control.as_ref().filter(|_| !self.closed) else {
            return Ok(None);
        };
        let Endpoint::Subscriber(endpoint) = self.channel.binding.endpoint else {
            return Err(Error::Binding);
        };
        self.subscriber
            .next_repair(now)
            .map(|repair| encode(lease.owned, endpoint, &repair))
            .transpose()
    }

    pub fn next_receipt_control(
        &mut self,
        now: Duration,
    ) -> Result<Option<ReceiverReceipt>, Error> {
        let Some(lease) = self.control.as_ref().filter(|_| !self.closed) else {
            return Ok(None);
        };
        let Endpoint::Subscriber(endpoint) = self.channel.binding.endpoint else {
            return Err(Error::Binding);
        };
        let Some(action) = self.subscriber.next_receipt(now) else {
            return Ok(None);
        };
        let control = match action {
            ReceiptAction::Send(reply) => {
                ReceiverReceipt::Send(encode(lease.owned, endpoint, &reply)?)
            }
            ReceiptAction::Unconfirmed { unit_seq } => ReceiverReceipt::Unconfirmed { unit_seq },
        };
        Ok(Some(control))
    }
}

fn encode(
    owned: NonZeroU64,
    endpoint: SubscriberBinding,
    control: &SubscriberControl,
) -> Result<NativeDataControl, Error> {
    Ok(NativeDataControl {
        endpoint: Some(native_data_control::Endpoint::Subscriber(
            OwnedDataControl {
                owned: owned.get(),
                record: data_control::encode(endpoint, control).map_err(Error::Control)?,
            },
        )),
    })
}
