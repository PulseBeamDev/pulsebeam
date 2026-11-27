use std::{collections::VecDeque, num::NonZeroU64, time::Duration};

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TestWindow {
    key: NonZeroU32,
    start: u64,
    forms: u8,
}

struct TestCodec;

impl Codec<u8> for TestCodec {
    type Window = TestWindow;

    fn route(&self, window: TestWindow) -> (NonZeroU32, u64) {
        (window.key, window.start)
    }

    fn encode(
        &self,
        window: TestWindow,
        position: u64,
        value: &u8,
        dst: &mut [u8],
    ) -> Result<usize, CodecError> {
        let [_, _, _, _, _, _, high, low] = position.to_be_bytes();
        let [a, b, c, d] = window.key.get().to_be_bytes();
        let out = dst.get_mut(..13).ok_or(CodecError::TooLarge)?;
        out.copy_from_slice(&[0x80, 0x66, high, low, 1, 2, 3, 4, a, b, c, d, *value]);
        Ok(out.len())
    }

    fn decode(&self, window: TestWindow, plaintext: &[u8]) -> Result<(u64, u8), CodecError> {
        let [0x80, 0x66, high, low, .., payload] = plaintext else {
            return Err(CodecError::Malformed);
        };
        Ok((
            (window.start & !0xffff) | u64::from(u16::from_be_bytes([*high, *low])),
            *payload,
        ))
    }
}

#[test]
fn exact_window_ack_gates_link_delivery_of_full_position() {
    type Link = Driver<TestCodec, TestWindow>;
    let client = LocalIdentity::generate().unwrap();
    let server = LocalIdentity::generate().unwrap();
    let now = Instant::now();
    let build = |role, local: &LocalIdentity, peer: &LocalIdentity, port| {
        Link::with_control(
            Config {
                binding: Binding {
                    epoch: NonZeroU64::MIN,
                    generation: NonZeroU64::MIN,
                    peer: SocketAddr::from(([127, 0, 0, 1], port)),
                },
                role,
                local,
                peer: peer.fingerprint(),
                bootstrap_mtu: 1200,
                limits: Limits {
                    active_windows: 1,
                    epoch_windows: 2,
                },
            },
            TestCodec,
            now,
        )
        .unwrap()
    };
    let (mut tx, tx_control) = build(Role::Client, &client, &server, 10001);
    let (mut rx, mut rx_control) = build(Role::Server, &server, &client, 10002);
    let ownership: Box<[u8]> = Box::new([1, 11, 0x55]);
    assert_eq!(
        tx_control.try_send(ownership.clone(), LinkTime(u64::MAX)),
        Err(crate::SendErrorKind::NotReady)
    );
    let mut wire = VecDeque::new();
    let mut send = |destination, bytes: &[u8]| {
        wire.push_back((destination, bytes.to_vec()));
        SocketOutcome::Accepted { bytes: bytes.len() }
    };
    tx.tick(now, &mut send).unwrap();
    rx.tick(now, &mut send).unwrap();
    let window = TestWindow {
        start: (1 << 48) | 0x18000,
        key: NonZeroU32::new(0x05060708).unwrap(),
        forms: 1,
    };
    let position = window.start | 0x7fff;
    let spec = WindowSpec {
        authority: NonZeroU64::MIN,
        direction: Direction::Send,
        start: window.start,
        ssrc: window.key,
        purpose: Purpose::Repair { position },
        expires: now.checked_add(Duration::from_secs(1)).unwrap(),
    };
    assert!(tx.stage(now, tx.binding(), spec, window).is_err());
    for _ in 0..64 {
        let Some((destination, packet)) = wire.pop_front() else {
            break;
        };
        let target = if destination.port() == 10001 {
            &mut rx
        } else {
            &mut tx
        };
        assert!(
            target
                .receive(now, target.binding(), &packet, &mut |to, bytes| {
                    wire.push_back((to, bytes.to_vec()));
                    SocketOutcome::Accepted { bytes: bytes.len() }
                })
                .unwrap()
                .is_none()
        );
    }
    assert!(wire.is_empty());
    assert!(tx.lease(now).is_some());
    assert!(rx.lease(now).is_some());
    assert!(tx.send_budget(now).is_none());
    tx_control
        .try_send(ownership.clone(), LinkTime(u64::MAX))
        .unwrap();
    tx.tick(now, &mut |destination, bytes| {
        wire.push_back((destination, bytes.to_vec()));
        SocketOutcome::Accepted { bytes: bytes.len() }
    })
    .unwrap();
    let (_, control) = wire.pop_front().unwrap();
    assert_eq!(control.len(), 25);
    assert!(
        rx.receive(now, rx.binding(), &control, &mut |to, bytes| {
            wire.push_back((to, bytes.to_vec()));
            SocketOutcome::Accepted { bytes: bytes.len() }
        })
        .unwrap()
        .is_none()
    );
    assert_eq!(rx_control.try_recv(), Some(ownership.clone()));
    assert!(wire.is_empty());
    let send = tx.stage(now, tx.binding(), spec, window).unwrap();
    let receive_spec = WindowSpec {
        direction: Direction::Receive,
        ..spec
    };
    let receive = rx.stage(now, rx.binding(), receive_spec, window).unwrap();
    let wrong = TestWindow { forms: 2, ..window };
    assert!(matches!(
        tx.stage(now, tx.binding(), spec, wrong),
        Err(Error::Scope)
    ));
    assert!(matches!(
        tx.activate(now, send, wrong, spec),
        Err(Error::Scope)
    ));
    assert!(matches!(
        tx.activate(
            now,
            send,
            window,
            WindowSpec {
                authority: NonZeroU64::MIN.saturating_add(1),
                ..spec
            }
        ),
        Err(Error::Scope)
    ));
    assert!(tx.prepare(now, send, position, &42).is_err());
    tx.activate(now, send, window, spec).unwrap();
    let sealed = tx
        .prepare_with(now, send, position, |codec, staged, dst| {
            assert_eq!(staged, send);
            codec.encode(staged.descriptor(), position, &42, dst)
        })
        .unwrap();
    assert!(matches!(
        tx.submit(now, sealed, &mut |destination, bytes| {
            wire.push_back((destination, bytes.to_vec()));
            SocketOutcome::Accepted { bytes: bytes.len() }
        }),
        Submission::Accepted { bytes: 29 }
    ));
    let (_, packet) = wire.pop_front().unwrap();
    let mut collect = |destination, bytes: &[u8]| {
        wire.push_back((destination, bytes.to_vec()));
        SocketOutcome::Accepted { bytes: bytes.len() }
    };
    assert!(
        rx.receive_with_decoder::<()>(
            now,
            rx.binding(),
            &packet,
            None,
            &mut collect,
            |_, _, _| panic!("inactive receive ticket")
        )
        .is_err()
    );
    rx.activate(now, receive, window, receive_spec).unwrap();
    let opened = rx
        .receive_with_decoder(
            now,
            rx.binding(),
            &packet,
            None,
            &mut collect,
            |codec, staged, bytes| {
                assert_eq!(staged, receive);
                let (position, value) = codec.decode(staged.descriptor(), bytes)?;
                Ok((position, (value, staged.spec().purpose)))
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(opened.position, position);
    assert_eq!(opened.route, receive);
    assert_eq!(opened.packet, (42, Purpose::Repair { position }));
    assert!(rx.retire(receive));
    assert!(
        rx.receive(now, rx.binding(), &packet, &mut collect)
            .is_err()
    );
    assert!(wire.is_empty());
    tx_control.try_send(ownership, LinkTime(u64::MAX)).unwrap();
    let feedback_at = now + Duration::from_millis(20);
    rx.tick(feedback_at, &mut |destination, bytes| {
        wire.push_back((destination, bytes.to_vec()));
        SocketOutcome::Accepted { bytes: bytes.len() }
    })
    .unwrap();
    for (_, feedback) in wire.drain(..) {
        tx.receive(feedback_at, tx.binding(), &feedback, &mut |_, bytes| {
            SocketOutcome::Accepted { bytes: bytes.len() }
        })
        .unwrap();
    }
    let budget = tx
        .send_budget(feedback_at)
        .expect("fresh outbound feedback");
    assert_eq!(budget.feedback_at, feedback_at);
    assert!(budget.pacing_bps > 0);
    assert!(rx.send_budget(feedback_at).is_none());
    assert!(
        tx.send_budget(feedback_at + Duration::from_millis(500))
            .is_none()
    );
    let binding = Binding {
        generation: NonZeroU64::MIN.saturating_add(1),
        ..tx.binding()
    };
    assert!(tx.replace_path(feedback_at, binding));
    assert!(tx.send_budget(feedback_at).is_none());
    assert!(tx_control.is_closed());
}
