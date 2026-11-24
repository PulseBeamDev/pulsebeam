#![allow(
    clippy::arithmetic_side_effects,
    clippy::disallowed_types,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "native wire fixtures stop at violated RTP invariants"
)]

mod support;

use pulsebeam_rtc::{BoundedRtp, MediaScope, WriteError};
use support::{AuthenticatedVideoSample, PeerFixture, assert_decoded_stock_keyframe, rtp_write};

const PROTECTED_UDP_CEILING: usize = 1_216;

#[test]
fn stock_native_media_negotiates_aes256_even_when_the_client_offers_legacy_first() {
    use pulsebeam_rtc_simulation::{CryptoOptions, DtlsSrtpCipherSuite as Suite};

    let mut legacy_first = CryptoOptions::prefer_gcm();
    legacy_first.srtp.prefer_gcm_crypto_suites = false;
    let legacy_only = CryptoOptions::no_gcm();
    for (crypto, offered, selected) in [
        (
            legacy_first,
            &[
                Suite::AES128_CM_SHA1_80,
                Suite::AEAD_AES256_GCM,
                Suite::AEAD_AES128_GCM,
            ][..],
            0x0008,
        ),
        (legacy_only, &[Suite::AES128_CM_SHA1_80][..], 0x0001),
    ] {
        assert_eq!(
            crypto
                .supported_dtls_srtp_cipher_suites()
                .expect("stock native SRTP offers"),
            offered,
        );
        let fixture = PeerFixture::connected_with_media_crypto(crypto);
        assert_eq!(
            fixture.native_media_dtls_parameters(&fixture.sender_mid),
            (Some(0xfefc), Some(selected)),
        );
    }
}

fn assert_gcm(fixture: &PeerFixture) {
    assert!(matches!(
        fixture.native_media_srtp_profile(&fixture.sender_mid),
        Some(0x0007 | 0x0008)
    ));
}

fn authenticated_stock_keyframe_and_repair(
    fixture: &mut PeerFixture,
    layered: bool,
) -> Vec<AuthenticatedVideoSample> {
    assert_gcm(fixture);
    let packets = fixture.send_stock_h264_keyframe_with_repair(layered);
    let (_, lost_sequence) = fixture.lost_source_packet();
    assert!(packets.len() >= 3);
    assert!(
        packets
            .iter()
            .any(|packet| packet.is_repair && packet.sequence == lost_sequence)
    );
    assert!(
        packets
            .iter()
            .all(|packet| packet.timestamp == packets[0].timestamp)
    );
    for packet in &packets {
        let expected_pt = if packet.is_repair {
            packet.repair_payload_type.expect("negotiated source RTX")
        } else {
            packet.primary_payload_type
        };
        assert_eq!(packet.payload_type, expected_pt);
        assert!(packet.protected_udp_bytes <= PROTECTED_UDP_CEILING);
    }
    packets
}

#[test]
fn stock_native_whole_source_h264_and_rtx_fit_without_dd() {
    let mut fixture = PeerFixture::connected_video_with_rfc8888();
    let packets = authenticated_stock_keyframe_and_repair(&mut fixture, false);
    assert!(packets.iter().all(|packet| packet.descriptor.is_none()));
    let peak = |is_repair| {
        packets
            .iter()
            .filter(|packet| packet.is_repair == is_repair)
            .map(|packet| packet.protected_udp_bytes)
            .max()
            .expect("observed protected datagrams")
    };
    let primary = peak(false);
    let repair = peak(true);
    assert!(
        (1_100..=PROTECTED_UDP_CEILING - 2).contains(&primary),
        "measured primary must leave room for the standard RTX OSN: {primary}"
    );
    assert!(
        (1_100..=PROTECTED_UDP_CEILING).contains(&repair),
        "measured stock RTX: {repair}"
    );
}

#[test]
#[allow(
    clippy::print_stderr,
    reason = "retain measured packet sizes in the qualification test log"
)]
fn stock_native_keyframe_and_rtx_report_fabric_size_with_negotiated_dd() {
    use pulsebeam_core::fabric::{
        Extension, ExtensionKind, FabricPacket, Header, MAX_WAN_RTP_BYTES, Profile, WireError,
    };

    let mut fixture = PeerFixture::connected_bounded_video_dd(PROTECTED_UDP_CEILING);
    fixture.take_udp_srtcp_peaks();
    let packets = authenticated_stock_keyframe_and_repair(&mut fixture, true);
    let [_, edge_srtcp] = fixture.take_udp_srtcp_peaks();
    assert!(
        (1..=PROTECTED_UDP_CEILING).contains(&edge_srtcp),
        "publisher repair must include protected feedback from its edge"
    );
    assert!(
        packets
            .iter()
            .any(|packet| !packet.is_repair && packet.descriptor.is_some())
    );
    let mut primary = 0;
    let mut repair = 0;
    let mut protected_primary = 0;
    let mut protected_repair = 0;
    let mut oversize = 0;
    for packet in &packets {
        eprintln!(
            "RTP seq={} PT={} RTX={} M={} profile={:04x?} mixed={} protected_udp_bytes={} codec_bytes={} ext(id,bytes)={:?}",
            packet.sequence,
            packet.payload_type,
            packet.is_repair,
            packet.marker,
            packet.extension_profile,
            packet.allow_mixed_extensions,
            packet.protected_udp_bytes,
            packet.codec_bytes,
            packet.extension_lengths,
        );
        let extensions = packet.descriptor.as_deref().map(|value| Extension {
            kind: ExtensionKind::DependencyDescriptor,
            value,
        });
        let length = packet.descriptor.as_ref().map_or(0, Vec::len);
        let element_header = if length > 16 { 2 } else { 1 };
        let expected = 12
            + extensions.map_or(0, |_| 4 + (element_header + length).next_multiple_of(4))
            + packet.codec_bytes;
        let payload = vec![0; packet.codec_bytes];
        let fabric_len = FabricPacket::new(
            Header {
                profile: Profile::H264,
                marker: packet.marker,
                sequence: u32::from(packet.sequence),
                timestamp: packet.timestamp,
            },
            extensions.as_slice(),
            payload,
        )
        .map(|packet| packet.wan_plaintext_len());
        if expected > MAX_WAN_RTP_BYTES {
            assert_eq!(fabric_len, Err(WireError::Oversize));
            oversize += 1;
        } else {
            assert_eq!(fabric_len, Ok(expected));
        }
        if packet.is_repair {
            repair = repair.max(expected);
            protected_repair = protected_repair.max(packet.protected_udp_bytes);
        } else {
            primary = primary.max(expected);
            protected_primary = protected_primary.max(packet.protected_udp_bytes);
        }
    }
    eprintln!(
        "native stock H264 L1T3, 1280x720, 1000 kbps, Fabric DD-only: {primary} B primary, {repair} B RTX after OSN stripping, {oversize}/{} oversize",
        packets.len()
    );
    assert!(
        protected_primary >= 1_100 && protected_repair >= 1_100,
        "fixture must exercise large authenticated fragments: primary {protected_primary}, RTX {protected_repair}"
    );
}

#[test]
fn direct_fragmented_video_preserves_caller_timing_markers_and_opaque_dd() {
    let mut fixture = PeerFixture::connected_bounded_video_dd(PROTECTED_UDP_CEILING);
    assert_gcm(&fixture);
    let sources = fixture.send_stock_h264_keyframe_source();
    assert!(sources.len() >= 3);
    let timestamp = sources[0].header.timestamp;
    let mut expected_dd = Vec::new();
    let mid = fixture.sender_mid.clone();
    let policy = BoundedRtp {
        max_wire_bytes: PROTECTED_UDP_CEILING,
        max_primary_wire_bytes: None,
        clock: None,
        scope: Some(MediaScope::new(std::num::NonZeroU128::MIN)),
        replace_queued: false,
    };
    fixture.take_udp_srtcp_peaks();
    fixture.lose_next_handoff(1_001);
    for (index, source) in sources.iter().enumerate() {
        assert_eq!(source.header.timestamp, timestamp);
        expected_dd.push(
            source
                .extensions
                .iter()
                .find(|value| value.uri.as_ref() == pulsebeam_core::dd::URI)
                .map(|value| value.value.to_vec()),
        );
        let mut packet = rtp_write(source.clone(), 1_000 + u64::try_from(index).unwrap());
        let at = fixture.at();
        if index == 0 {
            let rejected = fixture
                .connection
                .write_rtp_bounded(
                    at,
                    &mid,
                    None,
                    packet,
                    BoundedRtp {
                        max_wire_bytes: 12,
                        max_primary_wire_bytes: None,
                        ..policy
                    },
                )
                .expect_err("wire cap must refuse the fragment before consuming its index");
            assert_eq!(rejected.reason, WriteError::RtpTooLarge);
            packet = rejected.packet;
        }
        fixture
            .connection
            .write_rtp_bounded(at, &mid, None, packet, policy)
            .expect("write fragment on the independently bounded subscriber leg");
    }
    assert!(
        expected_dd.iter().any(Option::is_some),
        "stock keyframe emits DD"
    );
    let mut primary_ssrc = None;
    let mut originals = expected_dd
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != 1);
    let mut recovered = false;
    for _ in &sources {
        let header = fixture.receive_video_wire_packet();
        assert_eq!(header.timestamp, timestamp);
        assert!(!header.padding);
        assert!(header.wire_bytes <= policy.max_wire_bytes);
        if header.ssrc == *primary_ssrc.get_or_insert(header.ssrc) {
            let (index, dd) = originals.next().expect("one send per surviving primary");
            assert_eq!(
                header.sequence_number,
                1_000 + u16::try_from(index).unwrap()
            );
            assert_eq!(header.marker, index + 1 == sources.len());
            assert_eq!(header.ext_vals.dependency_descriptor.as_ref(), dd.as_ref());
        } else {
            assert!(!recovered, "one repair for the packet lost after handoff");
            assert_eq!(
                header.ext_vals.dependency_descriptor.as_ref(),
                expected_dd[1].as_ref()
            );
            recovered = true;
        }
    }
    assert!(originals.next().is_none());
    assert!(recovered, "native NACK must produce a separate RTX packet");
    let [native_srtcp, _] = fixture.take_udp_srtcp_peaks();
    assert!(
        (1..=PROTECTED_UDP_CEILING).contains(&native_srtcp),
        "subscriber repair must include protected native feedback"
    );
    assert!(fixture.handoff_losses_consumed());
    let frame = fixture.receive_video_frame();
    assert_decoded_stock_keyframe(&frame);
}
