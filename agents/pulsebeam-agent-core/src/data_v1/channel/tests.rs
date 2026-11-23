#![allow(
    clippy::unwrap_used,
    reason = "test channels and fixtures use valid prepared facts"
)]

use super::*;
use crate::id::IdGenerator;
use core::num::{NonZeroU16, NonZeroU64};
use pulsebeam_proto::{
    data_control::{PublisherBinding, SubscriberBinding},
    data_endpoint::{self, Delivery, Endpoint, Format},
    state::{DataV1Delivery, SctpDataV1Parameters, SctpParameters},
};

fn accepted(parameters: SctpDataV1Parameters) -> TrackBinding {
    TrackBinding {
        transport: Some(Transport::SctpDataV1(parameters)),
    }
}

fn pair(ids: &mut IdGenerator) -> (Channel, Channel) {
    let generation = ids.generation();
    let source = NonZeroU64::MIN;
    let stream = NonZeroU16::MIN;
    let physical = data_endpoint::Channel {
        association: generation,
        stream,
        incarnation: source,
    };
    let prepared = PreparedDataChannel {
        stream_id: stream.get(),
        negotiated: true,
        ordered: false,
        reliability: DataChannelReliability::MaxRetransmits(0),
        association_max_channels: Some(16),
    };
    let format = Format::negotiate(Some(data_endpoint::DATA_V1), Delivery::Unreliable).unwrap();
    let pub_binding = Binding {
        channel: physical,
        format,
        endpoint: Endpoint::Publisher(PublisherBinding { source }),
    };
    let sub_binding = Binding {
        endpoint: Endpoint::Subscriber(SubscriberBinding {
            source,
            branch: source,
        }),
        ..pub_binding
    };
    let parameters = SctpDataV1Parameters {
        stream_id: Some(u32::from(stream.get())),
        version: u32::from(data_endpoint::DATA_V1),
        channel_incarnation: source.get(),
        delivery: DataV1Delivery::Unreliable as i32,
    };
    let tx = Channel::from_accepted(ids.channel(), pub_binding, &prepared, &accepted(parameters))
        .unwrap();
    let rx = Channel::from_accepted(ids.channel(), sub_binding, &prepared, &accepted(parameters))
        .unwrap();
    for rejected in [
        TrackBinding {
            transport: Some(Transport::Sctp(SctpParameters {
                stream_id: parameters.stream_id,
            })),
        },
        accepted(SctpDataV1Parameters {
            stream_id: Some(2),
            ..parameters
        }),
        accepted(SctpDataV1Parameters {
            version: 2,
            ..parameters
        }),
        accepted(SctpDataV1Parameters {
            channel_incarnation: 2,
            ..parameters
        }),
        accepted(SctpDataV1Parameters {
            delivery: DataV1Delivery::Latest as i32,
            ..parameters
        }),
    ] {
        assert_eq!(
            Channel::from_accepted(tx.id, pub_binding, &prepared, &rejected),
            Err(Error::Binding)
        );
    }
    assert_eq!(
        Channel::from_accepted(
            tx.id,
            pub_binding,
            &PreparedDataChannel {
                ordered: true,
                ..prepared
            },
            &accepted(parameters),
        ),
        Err(Error::Binding)
    );
    (tx, rx)
}

#[test]
fn reliable_control_dispatch_uses_only_the_exact_active_receiver_lease() {
    use pulsebeam_proto::{
        data_control::{
            self, BranchUnit, ControlOutcome, Expectation, SourceUnit, SubscriberControl,
            SubscriberObservation, subscriber_control, subscriber_observation,
        },
        state::{NativeDataControl, OwnedDataControl, codec, native_data_control},
    };
    let mut ids = IdGenerator::new();
    let (tx, mut rx) = pair(&mut ids);
    rx.binding.format =
        Format::negotiate(Some(data_endpoint::DATA_V1), Delivery::Reliable).unwrap();
    let Endpoint::Subscriber(endpoint) = rx.binding.endpoint else {
        panic!("subscriber")
    };
    let generation = rx.binding.channel.association;
    let signaling = ids.channel();
    let owned = NonZeroU64::MIN;
    let now = Duration::ZERO;
    let mut receiver = ReceiveChannel::new_reliable(rx, None, owned, signaling).unwrap();
    let mut sender = Publisher::new(tx.binding, None, 0).unwrap();
    let unit = sender
        .admit(tx.binding, vec![7], DataSendOptions::default())
        .unwrap();
    let mut bytes = vec![0; MAX_RECORD_BYTES];
    let count = unit.write(0, &mut bytes).unwrap();
    bytes.truncate(count);
    assert_eq!(
        receiver.offer_binary(generation, rx.id, &bytes, now),
        Ok(None)
    );
    assert_eq!(receiver.next_receipt_control(now), Ok(None));
    let expectation = Expectation {
        unit: Some(BranchUnit {
            source: Some(SourceUnit {
                source_incarnation: endpoint.source.get(),
                unit_seq: Some(0),
            }),
            branch_incarnation: endpoint.branch.get(),
        }),
        fragment_count: 1,
    };
    let wrap = |owned, endpoint, observation| {
        let carrier = NativeDataControl {
            endpoint: Some(native_data_control::Endpoint::Subscriber(
                OwnedDataControl {
                    owned,
                    record: data_control::encode(
                        endpoint,
                        &SubscriberObservation {
                            record: Some(observation),
                        },
                    )
                    .unwrap(),
                },
            )),
        };
        let bytes = codec::encode_data_control(&carrier).unwrap();
        let codec::NativeFrame::Data(decoded) = codec::decode_server_frame(&bytes).unwrap() else {
            panic!("explicit control frame")
        };
        decoded
    };
    let offer = subscriber_observation::Record::Expectation(expectation);
    assert_eq!(
        receiver.handle_control(
            ids.generation(),
            signaling,
            wrap(1, endpoint, offer.clone()),
            now
        ),
        Err(Error::Binding)
    );
    assert_eq!(
        receiver.handle_control(generation, rx.id, wrap(1, endpoint, offer.clone()), now),
        Err(Error::Binding)
    );
    assert_eq!(
        receiver.handle_control(generation, signaling, wrap(9, endpoint, offer.clone()), now),
        Err(Error::Binding)
    );
    let different = SubscriberBinding {
        branch: NonZeroU64::MAX,
        ..endpoint
    };
    let wrong = Expectation {
        unit: Some(BranchUnit {
            branch_incarnation: different.branch.get(),
            ..expectation.unit.unwrap()
        }),
        ..expectation
    };
    assert_eq!(
        receiver.handle_control(
            generation,
            signaling,
            wrap(
                1,
                different,
                subscriber_observation::Record::Expectation(wrong)
            ),
            now
        ),
        Err(Error::Control(data_control::Error::Binding))
    );
    assert_eq!(receiver.next_receipt_control(now), Ok(None));
    let ReceiverControl::Expected {
        response,
        assembled: Some(assembled),
    } = receiver
        .handle_control(generation, signaling, wrap(1, endpoint, offer.clone()), now)
        .unwrap()
    else {
        panic!("assembled only after matching expectation")
    };
    assert_eq!(assembled.bytes, vec![7]);
    let unwrap = |carrier: NativeDataControl| {
        let Some(native_data_control::Endpoint::Subscriber(frame)) = carrier.endpoint else {
            panic!("subscriber response")
        };
        assert_eq!(frame.owned, 1);
        data_control::decode::<SubscriberControl>(endpoint, &frame.record)
            .unwrap()
            .record
            .unwrap()
    };
    let subscriber_control::Record::ExpectationOutcome(propagation) = unwrap(response) else {
        panic!("propagation ACK")
    };
    assert_eq!(propagation.expectation, Some(expectation));
    assert_eq!(propagation.outcome, ControlOutcome::Confirmed as i32);
    let Some(ReceiverReceipt::Send(receipt)) = receiver.next_receipt_control(now).unwrap() else {
        panic!("assembly receipt")
    };
    assert_eq!(
        unwrap(receipt),
        subscriber_control::Record::AssemblyReceipt(expectation)
    );
    let ack = subscriber_observation::Record::ReceiptAck(expectation);
    assert_eq!(
        receiver.handle_control(generation, signaling, wrap(1, endpoint, ack), now),
        Ok(ReceiverControl::ReceiptAck { settled: true })
    );
    assert_eq!(receiver.next_receipt_control(LIFETIME), Ok(None));
    assert!(receiver.close(generation, rx.id));
    assert_eq!(
        receiver.handle_control(generation, signaling, wrap(1, endpoint, offer), LIFETIME),
        Err(Error::Binding)
    );
}

#[test]
fn reliable_sender_retries_original_bytes_until_exact_publisher_control_ack() {
    use pulsebeam_proto::{
        data_control::{self, PublisherObservation, SourceUnit, publisher_observation},
        state::{NativeDataControl, OwnedDataControl, native_data_control::Endpoint as Role},
    };
    let mut ids = IdGenerator::new();
    let (mut tx, _) = pair(&mut ids);
    tx.binding.format =
        Format::negotiate(Some(data_endpoint::DATA_V1), Delivery::Reliable).unwrap();
    let Endpoint::Publisher(endpoint) = tx.binding.endpoint else {
        panic!("publisher")
    };
    let generation = tx.binding.channel.association;
    let signaling = ids.channel();
    let mut sender = SendChannel::new_reliable(tx, None, 8, NonZeroU64::MIN, signaling).unwrap();
    assert_eq!(sender.reliable_source(), Some(endpoint.source));
    let now = Duration::ZERO;
    let retry = crate::data_v1::CONTROL_RETRY;
    assert_eq!(
        sender.admit(vec![1; 1025], DataSendOptions::default(), now),
        Ok(8)
    );
    let mut originals = Vec::new();
    for _ in 0..2 {
        let operation = ids.operation();
        let DataChannelEffect::Send { payload, .. } =
            sender.next_effect(operation, now).unwrap().unwrap();
        originals.push(payload);
        sender.handle(
            &DataChannelEvent::Sent {
                operation,
                generation,
                channel: tx.id,
            },
            now,
        );
    }
    assert_eq!(sender.deadline(), Some(retry));
    let retry_operation = ids.operation();
    let DataChannelEffect::Send { payload, .. } =
        sender.next_effect(retry_operation, retry).unwrap().unwrap();
    assert_eq!(Some(&payload), originals.first());
    assert_eq!(sender.next_effect(ids.operation(), retry), Ok(None));
    let ack = |owned| NativeDataControl {
        endpoint: Some(Role::Publisher(OwnedDataControl {
            owned,
            record: data_control::encode(
                endpoint,
                &PublisherObservation {
                    record: Some(publisher_observation::Record::Admission(SourceUnit {
                        source_incarnation: endpoint.source.get(),
                        unit_seq: Some(8),
                    })),
                },
            )
            .unwrap(),
        })),
    };
    assert_eq!(
        sender.handle_publisher_control(ids.generation(), signaling, ack(1), retry),
        Err(Error::Binding)
    );
    assert_eq!(
        sender.handle_publisher_control(generation, tx.id, ack(1), retry),
        Err(Error::Binding)
    );
    assert_eq!(
        sender.handle_publisher_control(generation, signaling, ack(7), retry),
        Err(Error::Binding)
    );
    assert_eq!(
        sender.handle_publisher_control(generation, signaling, ack(1), retry),
        Ok(PublisherControl::Admitted { unit_seq: 8 })
    );
    assert_eq!(
        sender.handle_publisher_control(generation, signaling, ack(1), retry),
        Ok(PublisherControl::Ignored)
    );
    assert_eq!(
        sender.handle(
            &DataChannelEvent::Sent {
                operation: retry_operation,
                generation,
                channel: tx.id
            },
            retry
        ),
        Progress::Ready
    );
    assert_eq!(
        sender.next_effect(ids.operation(), retry.saturating_mul(2)),
        Ok(None)
    );
    assert_eq!(sender.take_unconfirmed(LIFETIME, false), None);
    assert_eq!(sender.deadline(), None);

    assert_eq!(
        sender.admit(vec![2], DataSendOptions::default(), LIFETIME),
        Ok(9)
    );
    assert_eq!(
        sender.take_unconfirmed(LIFETIME.saturating_mul(2), false),
        Some(9)
    );
    assert_eq!(
        sender.take_unconfirmed(LIFETIME.saturating_mul(2), false),
        None
    );
    assert_eq!(
        sender.next_effect(ids.operation(), LIFETIME.saturating_mul(2)),
        Ok(None)
    );
}

#[test]
fn exact_host_completions_advance_only_one_record_at_a_time() {
    let mut ids = IdGenerator::new();
    let (tx, rx) = pair(&mut ids);
    let mut send = SendChannel::new(tx, None, 40).unwrap();
    let mut receive = ReceiveChannel::new(rx, None).unwrap();
    let now = Duration::ZERO;
    assert_eq!(
        send.admit(vec![0x37; 1_025], DataSendOptions::default(), now),
        Ok(40)
    );
    let first = ids.operation();
    let DataChannelEffect::Send {
        operation,
        generation,
        channel,
        binary,
        payload,
    } = send.next_effect(first, now).unwrap().unwrap();
    assert_eq!(operation, first);
    assert_eq!(channel, tx.id);
    assert!(binary);
    assert_eq!(send.next_effect(ids.operation(), now), Ok(None));
    assert_eq!(
        receive.offer_binary(ids.generation(), rx.id, &payload, now),
        Err(Error::Binding)
    );
    let inbound = |binary| DataChannelEvent::Message {
        generation,
        channel: rx.id,
        binary,
        payload: payload.clone(),
    };
    assert_eq!(receive.handle(&inbound(false), now), Ok(None));
    assert_eq!(receive.deadline(), None);
    assert_eq!(receive.handle(&inbound(true), now), Ok(None));
    assert_eq!(receive.deadline(), Some(LIFETIME));
    let wrong = DataChannelEvent::Sent {
        operation: ids.operation(),
        generation,
        channel,
    };
    assert_eq!(send.handle(&wrong, now), Progress::Ignored);
    assert_eq!(send.next_effect(ids.operation(), now), Ok(None));
    let complete = DataChannelEvent::Sent {
        operation,
        generation,
        channel,
    };
    assert_eq!(send.handle(&complete, now), Progress::Ready);
    let second = ids.operation();
    let DataChannelEffect::Send { payload, .. } = send.next_effect(second, now).unwrap().unwrap();
    assert_eq!(
        receive.offer_binary(generation, rx.id, &payload, now),
        Ok(Some(AssembledUnit {
            unit_seq: 40,
            captured_at: None,
            bytes: vec![0x37; 1_025],
        }))
    );
    assert_eq!(send.handle(&complete, now), Progress::Ignored);
    assert_eq!(
        send.handle(
            &DataChannelEvent::Sent {
                operation: second,
                generation,
                channel
            },
            now
        ),
        Progress::Settled
    );
    assert_eq!(send.next_effect(ids.operation(), now), Ok(None));
    assert_eq!(
        receive.offer_binary(generation, rx.id, b"legacy", now),
        Err(Error::Record(pulsebeam_proto::data_record::Error::Length))
    );
    assert!(receive.close(generation, rx.id));
    assert_eq!(
        receive.offer_binary(generation, rx.id, &payload, now),
        Err(Error::Binding)
    );
}

#[test]
fn expiry_does_not_reissue_work_while_the_host_still_owns_a_send() {
    let mut ids = IdGenerator::new();
    let (tx, _) = pair(&mut ids);
    let generation = tx.binding.channel.association;
    let channel = tx.id;
    let mut send = SendChannel::new(tx, None, 0).unwrap();
    assert_eq!(
        send.admit(vec![0; 1_025], DataSendOptions::default(), Duration::ZERO),
        Ok(0)
    );
    let operation = ids.operation();
    assert!(
        send.next_effect(operation, Duration::ZERO)
            .unwrap()
            .is_some()
    );
    assert!(send.expire(LIFETIME));
    assert_eq!(
        send.admit(vec![], DataSendOptions::default(), LIFETIME),
        Err(Error::Capacity)
    );
    assert_eq!(
        send.handle(
            &DataChannelEvent::Sent {
                operation,
                generation,
                channel
            },
            LIFETIME
        ),
        Progress::Ignored
    );
    assert_eq!(
        send.admit(vec![], DataSendOptions::default(), LIFETIME),
        Ok(1)
    );
    let operation = ids.operation();
    assert!(send.next_effect(operation, LIFETIME).unwrap().is_some());
    assert_eq!(
        send.handle(
            &DataChannelEvent::SendFailed {
                operation,
                generation,
                channel,
                message: "host rejected".into(),
            },
            LIFETIME
        ),
        Progress::Rejected
    );
    assert_eq!(send.next_effect(ids.operation(), LIFETIME), Ok(None));
    assert_eq!(
        send.handle(
            &DataChannelEvent::Closed {
                generation,
                channel,
                reset_complete: false
            },
            LIFETIME
        ),
        Progress::Closed
    );
    assert_eq!(
        send.admit(vec![], DataSendOptions::default(), LIFETIME),
        Err(Error::Binding)
    );
}
