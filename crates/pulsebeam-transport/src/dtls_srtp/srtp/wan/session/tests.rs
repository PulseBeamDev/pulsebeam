use std::{collections::VecDeque, num::NonZeroU64};

use super::*;

const LIMITS: Limits = Limits {
    active_windows: 3,
    epoch_windows: 4,
};

fn binding(client: bool) -> Binding {
    Binding {
        epoch: NonZeroU64::MIN,
        generation: NonZeroU64::MIN,
        peer: if client {
            "127.0.0.1:10001"
        } else {
            "127.0.0.1:10002"
        }
        .parse()
        .unwrap(),
    }
}

fn session(role: Role, local: &LocalIdentity, peer: &LocalIdentity, now: Instant) -> Session {
    Session::new(
        Config {
            binding: binding(role == Role::Client),
            role,
            local,
            peer: peer.fingerprint(),
            bootstrap_mtu: 1200,
            limits: LIMITS,
        },
        now,
    )
    .unwrap()
}

#[derive(Default)]
struct Wire {
    frames: VecDeque<(bool, Vec<u8>)>,
    drop_accept: bool,
    probes: [usize; 2],
}

impl Wire {
    fn send(&mut self, server: bool) -> impl FnMut(SocketAddr, &[u8]) -> SocketOutcome + '_ {
        move |destination, bytes| {
            assert_eq!(destination, binding(!server).peer);
            if bytes.len() == super::super::MAX_DATAGRAM_BYTES {
                let count = &mut self.probes[usize::from(server)];
                *count = count.checked_add(1).unwrap();
            } else {
                assert!(bytes.len() <= 1200);
            }
            if !(server && bytes.len() == 28 && std::mem::take(&mut self.drop_accept)) {
                self.frames.push_back((server, bytes.to_vec()));
            }
            SocketOutcome::Accepted { bytes: bytes.len() }
        }
    }

    fn deliver(&mut self, now: Instant, client: &mut Session, server: &mut Session) {
        for _ in 0..64 {
            let Some((from_server, bytes)) = self.frames.pop_front() else {
                return;
            };
            let receiver = if from_server {
                &mut *client
            } else {
                &mut *server
            };
            assert!(
                receiver
                    .receive(
                        now,
                        binding(from_server),
                        &bytes,
                        &mut self.send(!from_server)
                    )
                    .unwrap()
            );
        }
        panic!("WAN exchange did not quiesce");
    }
}

#[test]
fn authenticated_offer_retries_lost_accept_and_each_direction_proves_its_path() {
    let client_id = LocalIdentity::generate().unwrap();
    let server_id = LocalIdentity::generate().unwrap();
    let start = Instant::now();
    let mut client = session(Role::Client, &client_id, &server_id, start);
    let mut server = session(Role::Server, &server_id, &client_id, start);
    let mut wire = Wire {
        drop_accept: true,
        ..Wire::default()
    };
    client.tick(start, &mut wire.send(false)).unwrap();
    server.tick(start, &mut wire.send(true)).unwrap();
    wire.deliver(start, &mut client, &mut server);
    assert!(!wire.drop_accept);
    assert!(server.selected());
    assert!(!client.selected());
    assert_eq!(wire.probes, [0, 0]);
    assert!(server.lease(start).is_none());
    assert!(client.lease(start).is_none());

    let retry = start + RETRY;
    client.tick(retry, &mut wire.send(false)).unwrap();
    wire.deliver(retry, &mut client, &mut server);
    assert!(server.selected() && client.selected());
    assert_eq!(wire.probes, [1, 1]);
    assert!(server.lease(retry).is_some());
    assert!(client.lease(retry).is_some());
    exercise_windows(&mut client, &mut server, retry);
    let expired = retry + Duration::from_secs(6);
    assert!(server.lease(expired).is_none());
    assert!(client.lease(expired).is_none());
}

fn feedback_wire(session: &mut Session, now: Instant) -> Vec<u8> {
    let mut feedback = None;
    session
        .tick(now, &mut |_, bytes| {
            if bytes[1] == 205 {
                assert!(feedback.replace(bytes.to_vec()).is_none());
            }
            SocketOutcome::Accepted { bytes: bytes.len() }
        })
        .unwrap();
    feedback.expect("expected one protected CCFB report")
}

#[test]
fn srtcp_accounts_for_accepted_loss_not_unsent_holes() {
    use crate::dtls_srtp::wan::{Direction, handoff::Submission};
    use std::num::NonZeroU32;

    let client_id = LocalIdentity::generate().unwrap();
    let server_id = LocalIdentity::generate().unwrap();
    let now = Instant::now();
    let mut client = session(Role::Client, &client_id, &server_id, now);
    let mut server = session(Role::Server, &server_id, &client_id, now);
    let mut wire = Wire::default();
    client.tick(now, &mut wire.send(false)).unwrap();
    server.tick(now, &mut wire.send(true)).unwrap();
    wire.deliver(now, &mut client, &mut server);
    let spec = WindowSpec {
        authority: NonZeroU64::MIN,
        start: 0x18000,
        ssrc: NonZeroU32::new(0x0506_0708).unwrap(),
        direction: Direction::Send,
        purpose: Purpose::Live,
        expires: now + Duration::from_secs(5),
    };
    let send_window = client.stage_window(now, binding(true), spec).unwrap();
    client.activate_window(now, send_window).unwrap();
    let receive_window = server
        .stage_window(
            now,
            binding(false),
            WindowSpec {
                direction: Direction::Receive,
                ..spec
            },
        )
        .unwrap();
    server.activate_window(now, receive_window).unwrap();
    let mut delayed = Vec::new();
    // 1 is a burned but ambiguous submission; 2 was accepted and delayed.
    for slot in [3u8, 2, 0, 1] {
        let data = [0x80, 0xe6, 0x80, slot, 1, 2, 3, 4, 5, 6, 7, 8, slot];
        let prepared = client
            .prepare_media(now, send_window, spec.start + u64::from(slot), &data)
            .unwrap();
        let mut sealed = Vec::new();
        let result = client.submit_media(now, prepared, &mut |_, bytes| {
            if slot == 1 {
                return SocketOutcome::Unknown;
            }
            sealed.extend_from_slice(bytes);
            SocketOutcome::AcceptedEct0 { bytes: bytes.len() }
        });
        assert_eq!(
            matches!(result, Submission::Accepted { bytes: 29 }),
            slot != 1
        );
        if slot == 2 {
            delayed = sealed;
        } else if slot != 1 {
            let ecn = if slot == 3 { Ecn::Ce } else { Ecn::Ect0 };
            server
                .open_media_with_ecn(now, binding(false), &sealed, Some(ecn))
                .unwrap();
        }
    }
    let report_at = now + Duration::from_millis(10);
    let feedback = feedback_wire(&mut server, report_at);
    assert_eq!(client.history.send.observations().bytes_in_flight, 87);
    client
        .receive_feedback(report_at, binding(true), &feedback)
        .unwrap();
    let reported = &client.history.send.observations().feedback;
    assert_eq!(reported.iter().filter(|entry| entry.received).count(), 2);
    assert!(
        reported
            .iter()
            .all(|entry| entry.packet.0 != spec.start + 1)
    );
    assert!(client.ecn_marking_allowed());
    assert!(
        reported
            .iter()
            .any(|entry| entry.packet.0 == spec.start + 3 && entry.ecn == Some(Ecn::Ce))
    );
    assert!(
        reported
            .iter()
            .any(|entry| entry.packet.0 == spec.start && entry.ecn == Some(Ecn::Ect0))
    );
    assert!(
        client
            .receive_feedback(report_at, binding(true), &feedback)
            .is_err()
    );
    let loss_at = report_at + Duration::from_millis(31);
    client
        .tick(loss_at, &mut |_, bytes| SocketOutcome::Accepted {
            bytes: bytes.len(),
        })
        .unwrap();
    let loss = &client.history.send.observations().synthetic;
    assert_eq!(loss.len(), 1);
    assert_eq!(loss[0].packet.0, spec.start + 2);
    assert_eq!(loss[0].transport_bytes, 29);
    assert_eq!(client.history.send.observations().bytes_in_flight, 0);

    server
        .open_media(loss_at, binding(false), &delayed)
        .unwrap();
    let recovery_at = loss_at + Duration::from_millis(10);
    let feedback = feedback_wire(&mut server, recovery_at);
    client
        .receive_feedback(recovery_at, binding(true), &feedback)
        .unwrap();
    let observations = &client.history.send.observations().feedback;
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].packet.0, spec.start + 2);
    assert!(observations[0].recovered);
    assert!(
        !client.ecn_marking_allowed(),
        "bleached ECT(0) disables marking for the lease"
    );
    assert_eq!(client.history.send.observations().bytes_in_flight, 0);

    let mut reports = Vec::new();
    let mut at = recovery_at;
    for slot in [4u8, 5] {
        let data = [0x80, 0xe6, 0x80, slot, 1, 2, 3, 4, 5, 6, 7, 8, slot];
        let prepared = client
            .prepare_media(at, send_window, spec.start + u64::from(slot), &data)
            .unwrap();
        assert!(matches!(
            client.submit_media(at, prepared, &mut |_, bytes| {
                server.open_media(at, binding(false), bytes).unwrap();
                SocketOutcome::Accepted { bytes: bytes.len() }
            }),
            Submission::Accepted { bytes: 29 }
        ));
        at += Duration::from_millis(10);
        reports.push(feedback_wire(&mut server, at));
    }
    for (slot, report) in [5, 4].into_iter().zip(reports.iter().rev()) {
        client.receive_feedback(at, binding(true), report).unwrap();
        let input = client.history.send.observations();
        let received: Vec<_> = input
            .feedback
            .iter()
            .filter(|entry| entry.received)
            .collect();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].packet.0, spec.start + slot);
        assert_eq!(received[0].recovered, slot == 4);
        assert_eq!(input.bytes_in_flight, 0);
        assert!(client.receive_feedback(at, binding(true), report).is_err());
    }

    let after_idle = at + Duration::from_secs(2);
    client
        .tick(after_idle, &mut |_, bytes| SocketOutcome::Accepted {
            bytes: bytes.len(),
        })
        .unwrap();
    assert!(
        !client.is_closed(),
        "feedback is not required for idle sessions"
    );
    let prepared = client
        .prepare_media(
            after_idle,
            send_window,
            spec.start + 6,
            &[0x80, 0xe6, 0x80, 6, 1, 2, 3, 4, 5, 6, 7, 8, 6],
        )
        .unwrap();
    assert!(matches!(
        client.submit_media(after_idle, prepared, &mut |_, bytes| {
            SocketOutcome::Accepted { bytes: bytes.len() }
        }),
        Submission::Accepted { .. }
    ));
    assert!(matches!(
        client.tick(after_idle + Duration::from_secs(2), &mut |_, _| {
            panic!("expired feedback lease must fence transport output")
        }),
        Err(Error::FeedbackTimeout)
    ));
    assert!(client.is_closed());
}

#[test]
fn retired_repair_keeps_feedback_without_reopening_media() {
    use crate::dtls_srtp::wan::{Direction, handoff::Submission};
    use std::num::NonZeroU32;

    let now = Instant::now();
    let client_id = LocalIdentity::generate().unwrap();
    let server_id = LocalIdentity::generate().unwrap();
    let mut client = session(Role::Client, &client_id, &server_id, now);
    let mut server = session(Role::Server, &server_id, &client_id, now);
    let mut wire = Wire::default();
    client.tick(now, &mut wire.send(false)).unwrap();
    server.tick(now, &mut wire.send(true)).unwrap();
    wire.deliver(now, &mut client, &mut server);

    let position = 0x18000;
    let live = WindowSpec {
        authority: NonZeroU64::MIN,
        start: position,
        ssrc: NonZeroU32::MIN,
        direction: Direction::Send,
        purpose: Purpose::Live,
        expires: now + Duration::from_secs(4),
    };
    let repair = WindowSpec {
        ssrc: NonZeroU32::new(2).unwrap(),
        purpose: Purpose::Repair { position },
        expires: now + Duration::from_secs(1),
        ..live
    };
    let live_tx = client.stage_window(now, binding(true), live).unwrap();
    let repair_tx = client.stage_window(now, binding(true), repair).unwrap();
    let repair_rx = server
        .stage_window(
            now,
            binding(false),
            WindowSpec {
                direction: Direction::Receive,
                ..repair
            },
        )
        .unwrap();
    client.activate_window(now, live_tx).unwrap();
    client.activate_window(now, repair_tx).unwrap();
    server.activate_window(now, repair_rx).unwrap();

    let mut payload = [0x80, 0xe6, 0x80, 0, 1, 2, 3, 4, 0, 0, 0, 1, 1];
    let prepared = client
        .prepare_media(now, live_tx, position, &payload)
        .unwrap();
    assert!(matches!(
        client.submit_media(now, prepared, &mut |_, bytes| {
            SocketOutcome::Accepted { bytes: bytes.len() }
        }),
        Submission::Accepted { .. }
    ));

    let at = now + Duration::from_millis(50);
    payload[11] = 2;
    let prepared = client
        .prepare_media(at, repair_tx, position, &payload)
        .unwrap();
    assert!(matches!(
        client.submit_media(at, prepared, &mut |_, bytes| {
            assert_eq!(
                server
                    .open_media(at, binding(false), bytes)
                    .unwrap()
                    .position,
                position
            );
            SocketOutcome::Accepted { bytes: bytes.len() }
        }),
        Submission::Accepted { .. }
    ));
    assert!(server.retire_window(repair_rx));
    assert!(client.retire_window(repair_tx));
    assert!(server.activate_window(at, repair_rx).is_err());
    assert!(client.activate_window(at, repair_tx).is_err());
    assert!(
        client
            .prepare_media(at, repair_tx, position, &payload)
            .is_err()
    );

    let report_at = at + Duration::from_millis(10);
    let feedback = feedback_wire(&mut server, report_at);
    client
        .receive_feedback(report_at, binding(true), &feedback)
        .unwrap();
    assert!(client.history.send.observations().fresh_network_feedback);
    assert!(
        client
            .feedback_window(Direction::Send, repair.ssrc, repair.expires)
            .is_none()
    );
    client
        .tick(now + Duration::from_secs(2), &mut wire.send(false))
        .unwrap();
    assert!(
        !client.is_closed(),
        "repair feedback counts as fresh path progress"
    );
}

#[test]
fn feedback_timeout_tracks_only_authorized_send_windows() {
    use crate::dtls_srtp::wan::{Direction, handoff::Submission};
    use std::num::NonZeroU32;

    let client_id = LocalIdentity::generate().unwrap();
    let server_id = LocalIdentity::generate().unwrap();
    for (expire, other_in_flight) in [(false, false), (true, false), (false, true)] {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(2);
        let mut client = session(Role::Client, &client_id, &server_id, now);
        let mut server = session(Role::Server, &server_id, &client_id, now);
        let mut wire = Wire::default();
        client.tick(now, &mut wire.send(false)).unwrap();
        server.tick(now, &mut wire.send(true)).unwrap();
        wire.deliver(now, &mut client, &mut server);
        let mut install = |label, expires, send| {
            let spec = WindowSpec {
                authority: NonZeroU64::MIN,
                start: 0x18000,
                ssrc: NonZeroU32::new(u32::from(label)).unwrap(),
                direction: Direction::Send,
                purpose: Purpose::Live,
                expires,
            };
            let ticket = client.stage_window(now, binding(true), spec).unwrap();
            client.activate_window(now, ticket).unwrap();
            if send {
                let data = [0x80, 0xe6, 0x80, 0, 1, 2, 3, 4, 0, 0, 0, label, 1];
                let prepared = client
                    .prepare_media(now, ticket, spec.start, &data)
                    .unwrap();
                assert!(matches!(
                    client.submit_media(now, prepared, &mut |_, bytes| {
                        SocketOutcome::Accepted { bytes: bytes.len() }
                    }),
                    Submission::Accepted { .. }
                ));
            }
            ticket
        };
        let retired = install(
            1u8,
            if expire {
                deadline
            } else {
                deadline + Duration::from_secs(1)
            },
            true,
        );
        install(2u8, deadline + Duration::from_secs(1), other_in_flight);
        if !expire {
            assert!(client.retire_window(retired));
        }
        let result = client.tick(deadline, &mut wire.send(false));
        if other_in_flight {
            assert!(matches!(result, Err(Error::FeedbackTimeout)));
        } else {
            result.unwrap();
            assert!(client.lease(deadline).is_some());
        }
    }
}

fn exercise_windows(client: &mut Session, server: &mut Session, now: Instant) {
    use crate::dtls_srtp::wan::{
        Direction,
        handoff::{Dropped, Submission},
    };
    use std::num::NonZeroU32;

    fn rtp(ssrc: u32, sequence: u16) -> [u8; 14] {
        let [hi, lo] = sequence.to_be_bytes();
        let [a, b, c, d] = ssrc.to_be_bytes();
        [0x80, 0xe6, hi, lo, 1, 2, 3, 4, a, b, c, d, 0x65, 0x88]
    }

    let mut windows = Vec::new();
    for (start, sequence, id, purpose) in [
        (0x18000, 0xffff, 0x05060708, Purpose::Live),
        (0x20000, 0, 0x05060709, Purpose::Live),
        (
            0x18000,
            0xfffd,
            0x0506070a,
            Purpose::Repair { position: 0x1fffd },
        ),
    ] {
        let spec = WindowSpec {
            authority: NonZeroU64::MIN,
            direction: Direction::Send,
            start,
            ssrc: NonZeroU32::new(id).unwrap(),
            purpose,
            expires: now.checked_add(Duration::from_secs(30)).unwrap(),
        };
        let position = (start & !0xffff) | u64::from(sequence);
        let send = client.stage_window(now, binding(true), spec).unwrap();
        let receive = server
            .stage_window(
                now,
                binding(false),
                WindowSpec {
                    direction: Direction::Receive,
                    ..spec
                },
            )
            .unwrap();
        assert_eq!(client.stage_window(now, binding(true), spec).unwrap(), send);
        let clear = rtp(id, sequence);
        assert!(matches!(
            client.prepare_media(now, send, position, &clear),
            Err(Error::Window)
        ));
        assert!(matches!(
            client.activate_window(now, receive),
            Err(Error::Window)
        ));
        client.activate_window(now, send).unwrap();
        let prepared = client.prepare_media(now, send, position, &clear).unwrap();
        let mut wire = Vec::new();
        assert!(matches!(
            client.submit_media(now, prepared, &mut |_, bytes| {
                wire.extend_from_slice(bytes);
                SocketOutcome::Accepted { bytes: bytes.len() }
            }),
            Submission::Accepted { bytes: 30 }
        ));
        assert!(matches!(
            server.open_media(now, binding(false), &wire),
            Err(Error::Window)
        ));
        server.activate_window(now, receive).unwrap();
        let mut wrong = wire.clone();
        wrong[6] ^= 1;
        assert!(matches!(
            server.open_media(now, binding(false), &wrong),
            Err(Error::Protection(_))
        ));
        let received = server.open_media(now, binding(false), &wire).unwrap();
        assert_eq!(received.window, receive);
        assert_eq!(received.position, position);
        assert_eq!(received.plaintext, clear);
        assert!(matches!(
            server.open_media(now, binding(false), &wire),
            Err(Error::Protection(_))
        ));
        let receipt = server
            .history
            .receive
            .receipt(
                history::space(spec.ssrc),
                pulsebeam_feedback::PacketId(position),
            )
            .unwrap();
        assert_eq!(receipt.tag, position);
        windows.push(send);
    }
    assert_eq!(client.history.send.observations().bytes_in_flight, 90);
    assert_eq!(server.history.receive.len(), 3);

    let ssrc = windows[0].spec().ssrc.get();
    let accepted_at = now.checked_add(Duration::from_millis(1)).unwrap();
    for (seq, outcome) in [
        (0x8000, SocketOutcome::Unknown),
        (0x8001, SocketOutcome::Accepted { bytes: 29 }),
        (0x8002, SocketOutcome::Failed),
        (0x8003, SocketOutcome::WouldBlock),
    ] {
        let prepared = client
            .prepare_media(now, windows[0], 0x10000 | u64::from(seq), &rtp(ssrc, seq))
            .unwrap();
        let result = client.submit_media(now, prepared, &mut |_, _| outcome);
        assert_eq!(client.history.send.observations().bytes_in_flight, 90);
        if let Submission::Retry(retry) = result {
            assert!(matches!(
                client.submit_media(accepted_at, retry, &mut |_, bytes| {
                    SocketOutcome::Accepted { bytes: bytes.len() }
                }),
                Submission::Accepted { bytes: 30 }
            ));
        }
    }
    assert_eq!(client.history.send.observations().bytes_in_flight, 120);
    let now = accepted_at;
    let old = windows[0];
    let earlier = rtp(old.spec().ssrc.get(), 0x8004);
    let queued = client.prepare_media(now, old, 0x18004, &earlier).unwrap();
    assert!(client.retire_window(old));
    assert!(matches!(
        client.submit_media(now, queued, &mut |_, _| panic!(
            "retired window reached the socket"
        )),
        Submission::Dropped(Dropped::Stale)
    ));
    assert!(matches!(
        client.stage_window(now, binding(true), old.spec()),
        Err(Error::Protection(_))
    ));
    let repair = windows[2];
    let wrong = rtp(repair.spec().ssrc.get(), 0xfffe);
    assert!(matches!(
        client.prepare_media(now, repair, 0x1fffe, &wrong),
        Err(Error::Window)
    ));
    let next = windows[1];
    let queued = client
        .prepare_media(now, next, 0x20001, &rtp(next.spec().ssrc.get(), 1))
        .unwrap();
    let changed = Binding {
        generation: NonZeroU64::MIN.saturating_add(1),
        ..binding(true)
    };
    assert!(client.replace_path(now, changed));
    assert_eq!(client.history.send.observations().bytes_in_flight, 0);
    assert!(matches!(
        client.activate_window(now, next),
        Err(Error::Window)
    ));
    assert!(matches!(
        client.submit_media(now, queued, &mut |_, _| panic!(
            "old path reached the socket"
        )),
        Submission::Dropped(Dropped::Stale)
    ));
}

#[cfg(feature = "datagram")]
#[test]
fn mixed_real_dtls_peers_never_accept_the_other_carrier() {
    use crate::wire;

    for role in [Role::Client, Role::Server] {
        let now = Instant::now();
        let wan_id = LocalIdentity::generate().unwrap();
        let legacy_id = LocalIdentity::generate().unwrap();
        let wan_is_client = role == Role::Client;
        let legacy_role = if wan_is_client {
            Role::Server
        } else {
            Role::Client
        };
        let mut wan = session(role, &wan_id, &legacy_id, now);
        let mut legacy = Association::single_record_datagrams(
            legacy_role,
            &legacy_id,
            wan_id.fingerprint(),
            1200,
            now,
        )
        .unwrap();
        let mut to_legacy = VecDeque::new();
        let mut to_wan = VecDeque::new();
        let send = |_, bytes: &[u8]| -> SocketOutcome {
            to_legacy.push_back(bytes.to_vec());
            SocketOutcome::Accepted { bytes: bytes.len() }
        };
        wan.tick(now, &mut { send }).unwrap();
        legacy.handle_timeout(now).unwrap();
        let mut legacy_offer = !wan_is_client;
        let mut rejected_offer = false;
        let mut rejected_legacy = false;
        for _ in 0..64 {
            let mut buffer = [0; 4096];
            match legacy.poll_output(&mut buffer).unwrap() {
                Output::Datagram(bytes) => to_wan.push_back(bytes.to_vec()),
                Output::ApplicationData(bytes) => {
                    assert!(wire::decode_for(bytes, true).is_err());
                    assert_eq!(Capability::decode(bytes), Some(Capability::Offer));
                    rejected_offer = true;
                }
                Output::Timeout(_) => {
                    if legacy_offer && legacy.is_connected() {
                        legacy_offer = false;
                        legacy
                            .send_application_data(&wire::Capability::Offer.encode())
                            .unwrap();
                        continue;
                    }
                    if let Some(packet) = to_legacy.pop_front() {
                        legacy.handle_packet(&packet).unwrap();
                    } else if let Some(packet) = to_wan.pop_front() {
                        let incoming =
                            wan.receive(now, binding(wan_is_client), &packet, &mut |_, bytes| {
                                to_legacy.push_back(bytes.to_vec());
                                SocketOutcome::Accepted { bytes: bytes.len() }
                            });
                        if let Err(error) = incoming {
                            assert!(matches!(error, Error::Incompatible));
                            rejected_legacy = true;
                            break;
                        }
                    } else {
                        break;
                    }
                }
                Output::Connected | Output::Progress => {}
                _ => panic!("unexpected historical DTLS event"),
            }
        }
        assert!(!wan.selected());
        assert!(wan.lease(now).is_none());
        if wan_is_client {
            assert!(rejected_offer);
            assert!(matches!(
                wan.tick(now + CAPABILITY_TIMEOUT, &mut |_, _| panic!(
                    "expired WAN retried another carrier"
                )),
                Err(Error::Incompatible)
            ));
        } else {
            assert!(rejected_legacy);
            assert!(matches!(
                wan.tick(now, &mut |_, _| panic!(
                    "rejected WAN retried another carrier"
                )),
                Err(Error::Closed)
            ));
        }
    }
}
