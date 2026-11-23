use super::*;
use std::{
    num::NonZeroU64,
    time::{Duration, Instant},
};

use handoff::{Dropped, PrepareError, SocketOutcome, Submission};
use path::{Binding, Path};
use pulsebeam_dtls::Role;

const PROFILE: SrtpProfile = SrtpProfile::AEAD_AES_256_GCM;
const SSRC: NonZeroU32 = NonZeroU32::new(0x0506_0708).unwrap();
const UPPER: u64 = 0x18000;
const LIMITS: Limits = Limits {
    active_windows: 1,
    epoch_windows: 2,
};

fn peers() -> (Protector, Protector) {
    let provider = str0m::crypto::from_feature_flags();
    let mut exporter = vec![0x17; PROFILE.keying_material_len()];
    exporter[0] = 0x90;
    let tx = Protector::new(&exporter, PROFILE, true, &provider, LIMITS).unwrap();
    let rx = Protector::new(&exporter, PROFILE, false, &provider, LIMITS).unwrap();
    (tx, rx)
}

fn rtp(sequence: u16, ssrc: NonZeroU32, bytes: usize) -> Vec<u8> {
    let [hi, lo] = sequence.to_be_bytes();
    let [a, b, c, d] = ssrc.get().to_be_bytes();
    let mut plaintext = vec![0x80, 0xe6, hi, lo, 1, 2, 3, 4, a, b, c, d];
    plaintext.resize(bytes, 0x61);
    plaintext
}

fn hex(text: &str) -> Vec<u8> {
    let (pairs, tail) = text.as_bytes().as_chunks::<2>();
    assert!(tail.is_empty());
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

#[test]
fn independent_exporter_vectors_pin_sparse_rtp_and_srtcp_layout() {
    // RFC 3711/7714 AES-CM KDF and AEAD vectors from an independent OpenSSL
    // implementation with exporter input 00..57, not already-derived keys.
    let exporter: Vec<u8> = (0..88).collect();
    let provider = str0m::crypto::from_feature_flags();
    let limits = Limits {
        active_windows: 4,
        epoch_windows: 4,
    };
    let mut tx = Protector::new(&exporter, PROFILE, true, &provider, limits).unwrap();
    let mut rx = Protector::new(&exporter, PROFILE, false, &provider, limits).unwrap();
    for (start, ssrc, clear, sealed) in [
        (
            0x18000,
            0x05060708,
            "80e6800001020304050607086588",
            "80e68000010203040506070855a76d02cd5fb8dff93ea5248d2172314a2e",
        ),
        (
            0x18000,
            0x05060709,
            "80e6ffff01020304050607096588",
            "80e6ffff0102030405060709360f7bf195541490de699685029b380a4c76",
        ),
        (
            0,
            0x0506070a,
            "80e60000000000000506070a50440100000000000000000000000001000000000000",
            "80e60000000000000506070a1161f4d7c677408f1574636335dc5be734015f2eba6de776a72f89aace3ffb745134d527e36e",
        ),
    ] {
        let ssrc = NonZeroU32::new(ssrc).unwrap();
        tx.install(Direction::Send, ssrc, start).unwrap();
        rx.install(Direction::Receive, ssrc, start).unwrap();
        let clear = hex(clear);
        let sealed = hex(sealed);
        assert_eq!(tx.seal_rtp(&clear), Ok(sealed.clone()));
        assert_eq!(rx.open_rtp(&sealed), Ok(clear));
    }
    let feedback = hex("8bcd00050a0b0c0d05060708000100018000000000000000");
    for sealed in [
        "8bcd00050a0b0c0d6654a1283aaa057c27650e25c0107d4b16febac27634a999d4ae05db4b1af94980000000",
        "8bcd00050a0b0c0d5e3b96a5aa4cdc9bd443031fc752a90a492e26fa9088107119928e6bcc49a5b780000001",
    ] {
        let sealed = hex(sealed);
        assert_eq!(tx.seal_feedback(&feedback), Ok(sealed.clone()));
        assert_eq!(rx.open_feedback(&sealed), Ok(feedback.clone()));
    }
    #[cfg(feature = "datagram")]
    {
        use sha2::{Digest, Sha256};
        let ssrc = NonZeroU32::new(0x0506070b).unwrap();
        tx.install(Direction::Send, ssrc, UPPER).unwrap();
        for (seq, len, wire, digest) in [
            (
                0x8000,
                1199,
                1215,
                "659c08eb5163ca2c602e0cbc40a80b34a7a2383fab793991fea2b5563dc47302",
            ),
            (
                0x8001,
                1200,
                1216,
                "22a75b69222e9c4ad74723cff0cb8836046115218beadd6123b2345cd5bc9ff5",
            ),
        ] {
            let mut plain = rtp(seq, ssrc, len);
            plain[1] = 0x66;
            let sealed = tx.seal_rtp(&plain).unwrap();
            assert_eq!(sealed.len(), wire);
            assert_eq!(&Sha256::digest(&sealed)[..], hex(digest));
        }
    }
}

#[test]
fn rtp_rejects_unknown_windows_and_wrong_half_before_authentication() {
    let (mut tx, mut rx) = peers();
    let upper = rtp(0xffff, SSRC, 14);
    assert_eq!(tx.seal_rtp(&upper), Err(Error::Binding));
    tx.install(Direction::Send, SSRC, UPPER).unwrap();
    assert_eq!(tx.seal_rtp(&rtp(0, SSRC, 14)), Err(Error::Binding));
    assert_eq!(
        rx.install(Direction::Receive, SSRC, UPPER + 1),
        Err(Error::Binding)
    );
    let sealed = tx.seal_rtp(&upper).unwrap();
    assert_eq!(rx.open_rtp(&sealed), Err(Error::Binding));
    rx.install(Direction::Receive, SSRC, UPPER).unwrap();
    let mut wrong = sealed.clone();
    wrong[2] = 0x7f;
    assert_eq!(rx.open_rtp(&wrong), Err(Error::Binding));
    wrong[2] = 0xff;
    wrong[6] ^= 1;
    assert_eq!(rx.open_rtp(&wrong), Err(SrtpError::Crypto.into()));
    assert_eq!(rx.open_rtp(&sealed), Ok(upper.clone()));
    assert_eq!(rx.open_rtp(&sealed), Err(SrtpError::Replay.into()));
    assert_eq!(tx.seal_rtp(&upper), Err(SrtpError::Replay.into()));

    // The first received packet was at the end of the upper half. Accept a
    // much earlier in-window packet without SRTP rollover inference or a 64-slot limit.
    let sparse = rtp(0x8000, SSRC, 14);
    let sealed = tx.seal_rtp(&sparse).unwrap();
    assert_eq!(rx.open_rtp(&sealed), Ok(sparse));
}

#[test]
fn rtp_protected_envelope_and_epoch_limits_do_not_reuse_ssrcs() {
    let (mut tx, mut rx) = peers();
    tx.install(Direction::Send, SSRC, UPPER).unwrap();
    rx.install(Direction::Receive, SSRC, UPPER).unwrap();
    let cap = MAX_DATAGRAM_BYTES - RTP_TAG_BYTES;
    assert_eq!(
        tx.seal_rtp(&rtp(0x8000, SSRC, cap + 1)),
        Err(SrtpError::OutputTooLarge.into())
    );
    for (sequence, plaintext_bytes) in [(0x8000, cap - 1), (0x8001, cap)] {
        let wire_bytes = plaintext_bytes + RTP_TAG_BYTES;
        let plaintext = rtp(sequence, SSRC, plaintext_bytes);
        let sealed = tx.seal_rtp(&plaintext).unwrap();
        assert_eq!(sealed.len(), wire_bytes);
        let mut oversize = sealed.clone();
        oversize.resize(MAX_DATAGRAM_BYTES + 1, 0);
        assert_eq!(
            rx.open_rtp(&oversize),
            Err(SrtpError::OutputTooLarge.into())
        );
        assert_eq!(rx.open_rtp(&sealed), Ok(plaintext));
    }
    let next = SSRC.saturating_add(1);
    assert_eq!(
        tx.install(Direction::Send, next, 0x20000),
        Err(Error::Exhausted)
    );
    tx.retire(Direction::Send, SSRC);
    assert_eq!(
        tx.install(Direction::Send, SSRC, 0x20000),
        Err(Error::Binding)
    );
    tx.install(Direction::Send, next, 0x20000).unwrap();
    rx.install(Direction::Send, next, 0x20000).unwrap(); // Other key direction.
    assert_eq!(tx.seal_rtp(&rtp(0xffff, SSRC, 14)), Err(Error::Binding));
    rx.retire(Direction::Receive, SSRC);
    rx.install(Direction::Receive, next, 0x20000).unwrap();
    let first = rtp(0, next, 14);
    assert_eq!(rx.open_rtp(&tx.seal_rtp(&first).unwrap()), Ok(first));
    tx.retire(Direction::Send, next);
    assert_eq!(
        tx.install(Direction::Send, next.saturating_add(1), 0x28000),
        Err(Error::Exhausted)
    );
}

#[test]
fn feedback_codec_matches_the_wan_vector_and_rejects_invalid_blocks() {
    use feedback::{Metric, Report};

    let plaintext = hex("8bcd00060a0b0c0d0506070880000003c0020000e000000012345678");
    let metrics = [
        Metric::received(2, 2).unwrap(),
        Metric::MISSING,
        Metric::received(3, 0).unwrap(),
    ];
    assert_eq!(
        Report::encode(SSRC, 0x8000, 0x12345678, &metrics),
        Ok(plaintext.clone())
    );
    let report = Report::parse(&plaintext).unwrap();
    assert_eq!(
        (report.ssrc, report.begin, report.timestamp),
        (SSRC, 0x8000, 0x12345678)
    );
    assert!(report.metrics().eq(metrics));
    assert!(
        report
            .metrics()
            .map(Metric::observation)
            .eq([Some((2, 2)), None, Some((3, 0))])
    );
    for (offset, byte) in [(7, 0), (15, 0), (19, 1), (23, 1)] {
        let mut malformed = plaintext.clone();
        malformed[offset] = byte;
        assert!(matches!(
            Report::parse(&malformed),
            Err(SrtpError::InvalidPacket)
        ));
    }
    for begin in [0x7fffu16, 0xffff] {
        let mut crossing = plaintext.clone();
        crossing[12..14].copy_from_slice(&begin.to_be_bytes());
        assert!(matches!(
            Report::parse(&crossing),
            Err(SrtpError::InvalidPacket)
        ));
        assert!(Report::encode(SSRC, begin, 0, &metrics[..1]).is_ok());
        assert!(Report::encode(SSRC, begin, 0, &metrics[..2]).is_err());
    }
}

#[test]
fn feedback_is_encrypted_authenticated_and_separately_bounded() {
    use feedback::{MAX_METRICS, Metric, Report};

    let (mut tx, mut rx) = peers();
    let mut plaintext = Report::encode(SSRC, 0x8000, 1, &[Metric::MISSING; MAX_METRICS]).unwrap();
    let cap = MAX_DATAGRAM_BYTES - RTCP_TRAILER_BYTES;
    let index_offset = MAX_DATAGRAM_BYTES - 4;
    assert_eq!(plaintext.len(), cap);
    let sealed = tx.seal_feedback(&plaintext).unwrap();
    assert_eq!(sealed.len(), MAX_DATAGRAM_BYTES);
    assert_eq!(sealed[..8], plaintext[..8]);
    assert_ne!(sealed[8..cap], plaintext[8..]);
    assert_eq!(sealed[index_offset] & 0x80, 0x80);
    let mut corrupt = sealed.clone();
    corrupt[index_offset] &= 0x7f;
    assert_eq!(
        rx.open_feedback(&corrupt),
        Err(SrtpError::InvalidPacket.into())
    );
    corrupt[index_offset] |= 0x80;
    corrupt[12] ^= 1;
    assert_eq!(rx.open_feedback(&corrupt), Err(SrtpError::Crypto.into()));
    assert_eq!(rx.open_feedback(&sealed), Ok(plaintext.clone()));
    assert_eq!(rx.open_feedback(&sealed), Err(SrtpError::Replay.into()));
    plaintext.resize(cap + 4, 0);
    plaintext[2..4].copy_from_slice(&u16::try_from(cap / 4).unwrap().to_be_bytes());
    assert_eq!(
        tx.seal_feedback(&plaintext),
        Err(SrtpError::OutputTooLarge.into())
    );
    assert_eq!(
        tx.install(Direction::Send, NonZeroU32::new(FEEDBACK_SSRC).unwrap(), 0),
        Err(Error::Binding)
    );
}

fn qualify(path: &mut Path, now: Instant) {
    let probe = path.begin_probe(now).unwrap();
    assert!(path.probe_submitted(now, probe, MAX_DATAGRAM_BYTES));
    let mut response = [0; 13];
    response.copy_from_slice(&probe.encode()[..13]);
    response[2] = 2;
    path.receive(now, path.binding(), &response, 35);
    assert!(path.lease(now).is_some());
}

fn path(now: Instant) -> Path {
    Path::after_accept(
        Binding {
            epoch: NonZeroU64::MIN,
            generation: NonZeroU64::MIN,
            peer: "127.0.0.1:12345".parse().unwrap(),
        },
        Role::Client,
        now,
    )
}

#[test]
fn only_definite_zero_handoff_retries_the_same_seal() {
    let (mut tx, mut rx) = peers();
    let now = Instant::now();
    let expires = now + Duration::from_secs(3);
    let mut path = path(now);
    tx.install(Direction::Send, SSRC, UPPER).unwrap();
    rx.install(Direction::Receive, SSRC, UPPER).unwrap();
    let plaintext = rtp(0x8000, SSRC, 1200);
    assert!(matches!(
        tx.prepare_rtp(&path, now, expires, &plaintext),
        Err(PrepareError::Stale)
    ));
    qualify(&mut path, now);
    let pending = tx.prepare_rtp(&path, now, expires, &plaintext).unwrap();
    let mut first_image = Vec::new();
    let Submission::Retry(retry) = tx.submit(&mut path, now, pending, |_, bytes| {
        first_image.extend_from_slice(bytes);
        SocketOutcome::WouldBlock
    }) else {
        panic!("definite zero must retain the sealed datagram")
    };
    let destination = path.binding().peer;
    assert!(matches!(
        tx.submit(&mut path, now, retry, |address, bytes| {
            assert_eq!(address, destination);
            assert_eq!(bytes, first_image);
            assert_eq!(rx.open_rtp(bytes), Ok(plaintext.clone()));
            SocketOutcome::Accepted { bytes: bytes.len() }
        }),
        Submission::Accepted { bytes: 1216 }
    ));
    assert_eq!(tx.seal_rtp(&plaintext), Err(SrtpError::Replay.into()));
    for (seq, outcome, expected) in [
        (0x8001, SocketOutcome::Unknown, Dropped::Uncertain),
        (
            0x8002,
            SocketOutcome::Accepted { bytes: 1215 },
            Dropped::Uncertain,
        ),
        (0x8003, SocketOutcome::Failed, Dropped::Failed),
    ] {
        let plaintext = rtp(seq, SSRC, 1200);
        let prepared = tx.prepare_rtp(&path, now, expires, &plaintext).unwrap();
        assert!(
            matches!(tx.submit(&mut path, now, prepared, |_, _| outcome), Submission::Dropped(reason) if reason == expected)
        );
        assert_eq!(tx.seal_rtp(&plaintext), Err(SrtpError::Replay.into()));
    }
}

#[test]
fn queued_ciphertext_is_fenced_by_mtu_path_lease_and_window_retirement() {
    let (mut tx, _) = peers();
    let start = Instant::now();
    let expires = start + Duration::from_secs(20);
    let mut path = path(start);
    qualify(&mut path, start);
    tx.install(Direction::Send, SSRC, UPPER).unwrap();
    let stale = tx
        .prepare_rtp(&path, start, expires, &rtp(0x8000, SSRC, 1199))
        .unwrap();
    let mtu = tx
        .prepare_rtp(&path, start, expires, &rtp(0x8001, SSRC, 1200))
        .unwrap();
    assert!(matches!(
        tx.submit(&mut path, start, mtu, |_, _| SocketOutcome::Mtu),
        Submission::Dropped(Dropped::Mtu)
    ));
    assert!(path.lease(start).is_none());
    let now = start + Duration::from_secs(2);
    qualify(&mut path, now);
    let blocked = |_, _: &[u8]| panic!("stale ciphertext reached the socket");
    assert!(matches!(
        tx.submit(&mut path, now, stale, blocked),
        Submission::Dropped(Dropped::Stale)
    ));
    let stale = tx
        .prepare_rtp(&path, now, expires, &rtp(0x8002, SSRC, 1200))
        .unwrap();
    let changed = Binding {
        generation: NonZeroU64::MIN.saturating_add(1),
        ..path.binding()
    };
    assert!(path.replace(now, changed));
    let now = start + Duration::from_secs(4);
    qualify(&mut path, now);
    assert!(matches!(
        tx.submit(&mut path, now, stale, blocked),
        Submission::Dropped(Dropped::Stale)
    ));
    let stale = tx
        .prepare_rtp(&path, now, expires, &rtp(0x8003, SSRC, 1200))
        .unwrap();
    let expired = tx
        .prepare_rtp(
            &path,
            now,
            now + Duration::from_secs(1),
            &rtp(0x8004, SSRC, 1199),
        )
        .unwrap();
    assert!(matches!(
        tx.submit(&mut path, now + Duration::from_secs(1), expired, blocked),
        Submission::Dropped(Dropped::Stale)
    ));
    tx.retire(Direction::Send, SSRC);
    assert!(matches!(
        tx.submit(&mut path, now + Duration::from_secs(1), stale, blocked),
        Submission::Dropped(Dropped::Stale)
    ));
}

#[cfg(feature = "datagram")]
#[test]
fn mutually_pinned_dtls_drives_a_directional_lease_and_protected_media() {
    use crate::fabric_wan::Capability;
    use pulsebeam_dtls::{Association, LocalIdentity, Output};

    struct Peer {
        association: Association,
        role: Role,
        protector: Option<Protector>,
        control: Vec<(Vec<u8>, usize)>,
    }

    impl Peer {
        fn drain(&mut self) -> Vec<Vec<u8>> {
            let mut output = Vec::new();
            let mut buffer = [0; 4096];
            for _ in 0..64 {
                match self.association.poll_output(&mut buffer).unwrap() {
                    Output::Datagram(bytes) => output.push(bytes.to_vec()),
                    Output::ApplicationData(bytes) => self.control.push((
                        bytes.to_vec(),
                        self.association.last_wan_application_wire_bytes().unwrap(),
                    )),
                    Output::KeyingMaterial(material, profile) => {
                        assert!(self.protector.is_none());
                        self.protector = Some(
                            Protector::from_dtls_exporter(material, profile, self.role, LIMITS)
                                .unwrap(),
                        );
                    }
                    Output::Progress | Output::Connected => {}
                    Output::Timeout(_) => return output,
                    _ => panic!("WAN DTLS emitted unexpected output"),
                }
            }
            panic!("DTLS did not drain")
        }

        fn receive(&mut self, wire: &[u8]) -> Vec<Vec<u8>> {
            self.association.handle_packet(wire).unwrap();
            self.drain()
        }

        fn send_capability(&mut self, peer: &mut Peer, capability: Capability) {
            self.association
                .send_application_data(&capability.encode())
                .unwrap();
            for bytes in self.drain() {
                assert!(peer.receive(&bytes).is_empty());
            }
            let (app, wire) = peer.control.pop().unwrap();
            assert_eq!((Capability::decode(&app), wire), (Some(capability), 28));
        }
    }

    let now = Instant::now();
    let alice = LocalIdentity::generate().unwrap();
    let bob = LocalIdentity::generate().unwrap();
    let mut client = Peer {
        association: Association::fabric_wan(Role::Client, &alice, bob.fingerprint(), 1200, now)
            .unwrap(),
        role: Role::Client,
        protector: None,
        control: Vec::new(),
    };
    let mut server = Peer {
        association: Association::fabric_wan(Role::Server, &bob, alice.fingerprint(), 1200, now)
            .unwrap(),
        role: Role::Server,
        protector: None,
        control: Vec::new(),
    };
    client.association.handle_timeout(now).unwrap();
    server.association.handle_timeout(now).unwrap();
    let mut packets = client.drain();
    let mut reverse = server.drain();
    for _ in 0..16 {
        if packets.is_empty() && reverse.is_empty() {
            break;
        }
        for bytes in std::mem::take(&mut packets) {
            assert!(bytes.len() <= 1200);
            reverse.extend(server.receive(&bytes));
        }
        for bytes in std::mem::take(&mut reverse) {
            assert!(bytes.len() <= 1200);
            packets.extend(client.receive(&bytes));
        }
    }
    assert!(packets.is_empty() && reverse.is_empty());
    client.send_capability(&mut server, Capability::Offer);
    server.send_capability(&mut client, Capability::Accept);

    let mut outgoing_path = path(now);
    let mut reverse_path = Path::after_accept(outgoing_path.binding(), Role::Server, now);
    let probe = outgoing_path.begin_probe(now).unwrap();
    client
        .association
        .send_fabric_wan_probe(&probe.encode())
        .unwrap();
    let mut probe_wire = client.drain();
    assert_eq!(probe_wire.len(), 1);
    let probe_wire = probe_wire.pop().unwrap();
    assert!(outgoing_path.probe_submitted(now, probe, probe_wire.len()));
    assert!(server.receive(&probe_wire).is_empty());
    let (app, wire) = server.control.pop().unwrap();
    let receipt = reverse_path
        .receive(now, reverse_path.binding(), &app, wire)
        .unwrap();
    server
        .association
        .send_application_data(&receipt.encode())
        .unwrap();
    let mut receipt_wire = server.drain();
    assert_eq!(receipt_wire.len(), 1);
    assert!(client.receive(&receipt_wire.pop().unwrap()).is_empty());
    let (app, wire) = client.control.pop().unwrap();
    outgoing_path.receive(now, outgoing_path.binding(), &app, wire);
    assert!(outgoing_path.lease(now).is_some());
    assert_eq!(reverse_path.lease(now), None);

    let mut tx = client.protector.unwrap();
    let mut rx = server.protector.unwrap();
    tx.install(Direction::Send, SSRC, UPPER).unwrap();
    rx.install(Direction::Receive, SSRC, UPPER).unwrap();
    let expires = now + Duration::from_secs(1);
    let plaintext = rtp(0xffff, SSRC, 1200);
    let pending = tx
        .prepare_rtp(&outgoing_path, now, expires, &plaintext)
        .unwrap();
    assert!(matches!(
        tx.submit(&mut outgoing_path, now, pending, |_, payload| {
            assert_eq!(rx.open_rtp(payload), Ok(plaintext));
            SocketOutcome::Accepted {
                bytes: payload.len(),
            }
        }),
        Submission::Accepted { bytes: 1216 }
    ));
}

#[test]
fn only_the_negotiated_aes_256_profile_and_exporter_length_are_accepted() {
    let provider = str0m::crypto::from_feature_flags();
    assert!(matches!(
        Protector::new(&[], SrtpProfile::AEAD_AES_128_GCM, true, &provider, LIMITS),
        Err(Error::Protection(SrtpError::UnsupportedProfile))
    ));
    assert!(matches!(
        Protector::new(&[], PROFILE, true, &provider, LIMITS),
        Err(Error::Protection(SrtpError::InvalidPacket))
    ));
}
