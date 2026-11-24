use super::*;

const BINDING: NonZeroU16 = NonZeroU16::MIN;

fn framing() -> PacketFraming {
    PacketFraming::negotiate(1).unwrap()
}

fn encode(binding: NonZeroU16, frame: PacketFrame<'_>) -> Vec<u8> {
    let mut wire = vec![0; MAX_PACKET_BYTES + PacketFrame::SLICE_HEADER_BYTES];
    let len = frame.write(framing(), binding, &mut wire).unwrap();
    wire.truncate(len);
    wire
}

#[test]
fn reorder_and_duplicates_cannot_complete_missing_or_terminal_transfers() {
    let mut receiver = Receiver::new(framing(), BINDING);
    let now = Instant::now();
    let slice = |id, offset, bytes| PacketFrame::Slice {
        transfer: NonZeroU32::new(id).unwrap(),
        total: 6,
        offset,
        bytes,
    };
    let tail = encode(BINDING, slice(1, 3, b"def"));
    let head = encode(BINDING, slice(1, 0, b"abc"));
    assert_eq!(tail, [5, 0, 1, 0, 0, 0, 1, 0, 6, 0, 3, b'd', b'e', b'f']);
    assert_eq!(receiver.receive(now, &tail), Ok(None));
    assert_eq!(receiver.receive(now, &tail), Ok(None));
    assert_eq!(
        receiver.receive(now, &head).unwrap().unwrap().as_ref(),
        b"abcdef"
    );
    assert_eq!(receiver.receive(now, &tail), Ok(None));
    assert_eq!(receiver.receive(now, &head), Ok(None));

    let head = encode(BINDING, slice(2, 0, b"abc"));
    let conflict = encode(BINDING, slice(2, 2, b"X"));
    assert_eq!(receiver.receive(now, &head), Ok(None));
    assert_eq!(
        receiver.receive(now, &conflict),
        Err(ReceiveError::Conflict)
    );
    assert_eq!(
        receiver.receive(now, &encode(BINDING, slice(2, 3, b"def"))),
        Ok(None)
    );
    assert_eq!(receiver.next_deadline(), None);

    let unbound = encode(NonZeroU16::new(2).unwrap(), slice(3, 0, b"abc"));
    assert_eq!(receiver.receive(now, &unbound), Err(ReceiveError::Binding));
    assert!(receiver.active.is_empty());
    assert_eq!(receiver.high, 2);
    assert_eq!(
        receiver.receive(now, &encode(BINDING, slice(3, 0, b"abc"))),
        Ok(None)
    );
    let deadline = receiver.next_deadline().unwrap();
    receiver.expire(deadline);
    assert!(receiver.active.is_empty());
    assert_eq!(
        receiver.receive(deadline, &encode(BINDING, slice(3, 3, b"def"))),
        Ok(None)
    );
    assert_eq!(
        receiver.receive(now, &encode(BINDING, slice(100, 3, b"def"))),
        Ok(None)
    );
    assert_eq!(receiver.receive(now, &head), Ok(None));
}

#[test]
fn disabling_a_binding_abandons_its_incomplete_transfers_without_reopening_ids() {
    let mut receiver = Receiver::new(framing(), BINDING);
    let now = Instant::now();
    let frame = |id, offset, bytes| {
        encode(
            BINDING,
            PacketFrame::Slice {
                transfer: NonZeroU32::new(id).unwrap(),
                total: 6,
                offset,
                bytes,
            },
        )
    };
    let head = frame(1, 0, b"abc");
    let tail = frame(1, 3, b"def");
    assert_eq!(receiver.receive(now, &head), Ok(None));
    receiver.discard_active();
    assert_eq!(receiver.next_deadline(), None);
    assert_eq!(receiver.receive(now, &tail), Ok(None));
    assert_eq!(receiver.receive(now, &head), Ok(None));
    assert_eq!(receiver.receive(now, &frame(2, 0, b"abc")), Ok(None));
    assert_eq!(
        receiver
            .receive(now, &frame(2, 3, b"def"))
            .unwrap()
            .unwrap()
            .as_ref(),
        b"abcdef"
    );
}

#[test]
fn sender_keeps_fixed_offsets_on_backpressure_and_fails_without_replanning() {
    let mut sender = Sender::new(framing(), BINDING);
    let now = Instant::now();
    let payload = vec![7; MAX_PACKET_BYTES].into_boxed_slice();
    sender.start(now, payload.clone()).unwrap();
    let mut frames = Vec::new();
    let mut submit = |bytes: &[u8]| {
        frames.push(bytes.to_vec());
        Ok(())
    };
    assert_eq!(
        sender.offer_with(now, &mut submit),
        Ok(SendProgress::Partial)
    );
    assert_eq!(sender.offer_with(now, |bytes| {
        assert!(matches!(PacketFrame::decode(framing(), bytes).unwrap(), PacketFrame::Slice { transfer, offset: 1139, .. } if transfer.get() == 1));
        Err(DatagramAdmission::Full)
    }), Ok(SendProgress::Deferred(DatagramAdmission::Full)));
    assert_eq!(
        sender.offer_with(now, &mut submit),
        Ok(SendProgress::Admitted)
    );
    let [head, tail] = frames.as_slice() else {
        panic!("expected two sent slices")
    };
    assert_eq!(head.len(), 1150);
    assert_eq!(tail.len(), 152);
    let mut receiver = Receiver::new(framing(), BINDING);
    assert_eq!(receiver.receive(now, tail), Ok(None));
    assert_eq!(
        receiver.receive(now, head).unwrap().unwrap().as_ref(),
        payload.as_ref()
    );

    sender.start(now, payload.clone()).unwrap();
    assert_eq!(
        sender.offer_with(now, |_| Ok(())),
        Ok(SendProgress::Partial)
    );
    assert_eq!(
        sender.offer_with(now, |_| Err(DatagramAdmission::Size)),
        Err(SendError::Size)
    );
    assert_eq!(sender.offer_with(now, |_| panic!()), Ok(SendProgress::Idle));
    sender.start(now, payload.clone()).unwrap();
    assert_eq!(
        sender.offer_with(now, |_| Err(DatagramAdmission::Unsupported)),
        Err(SendError::Unsupported)
    );
    sender.start(now, payload).unwrap();
    assert_eq!(
        sender.offer_with(now.checked_add(LIFETIME).unwrap(), |_| panic!()),
        Err(SendError::Expired)
    );
}

#[test]
fn slice_count_and_live_id_budget_poison_instead_of_admitting_partial_bytes() {
    let mut receiver = Receiver::new(framing(), BINDING);
    let now = Instant::now();
    let receive = |receiver: &mut Receiver, transfer, offset| {
        receiver
            .receive(
                now,
                &encode(
                    BINDING,
                    PacketFrame::Slice {
                        transfer: NonZeroU32::new(transfer).unwrap(),
                        total: 18,
                        offset,
                        bytes: b"x",
                    },
                ),
            )
            .map(|packet| packet.is_some())
    };
    for offset in 0..16 {
        assert_eq!(receive(&mut receiver, 1, offset), Ok(false));
    }
    assert_eq!(receive(&mut receiver, 1, 16), Err(ReceiveError::Capacity));
    assert_eq!(receive(&mut receiver, 1, 17), Ok(false));
    for id in 2..=5 {
        assert_eq!(receive(&mut receiver, id, 0), Ok(false));
    }
    assert_eq!(receive(&mut receiver, 6, 0), Err(ReceiveError::Capacity));
    assert_eq!(receiver.active.len(), MAX_ACTIVE);
}
