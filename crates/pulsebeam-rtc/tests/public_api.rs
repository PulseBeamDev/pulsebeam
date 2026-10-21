use std::{
    net::{Ipv6Addr, SocketAddr, SocketAddrV6},
    time::{Duration, Instant},
};

use pulsebeam_rtc::{
    AcceptError, Connection, ConnectionConfig, ConnectionLimits, LocalCandidate,
    LocalIceCredentials, NetworkInput, Output, PlayoutDelay, PlayoutDelayError, ReceiveError,
    RtpPacket, RtpWrite, StatsSnapshot,
};

fn assert_send<T: Send>() {}

macro_rules! assert_not_impl {
    ($type:ty, $trait:path) => {
        const _: fn() = || {
            trait AmbiguousIfImpl<A> {
                fn marker() {}
            }

            impl<T: ?Sized> AmbiguousIfImpl<()> for T {}

            struct Invalid;

            impl<T: ?Sized + $trait> AmbiguousIfImpl<Invalid> for T {}

            let _ = <$type as AmbiguousIfImpl<_>>::marker;
        };
    };
}

assert_not_impl!(Connection, Sync);

#[test]
fn direct_values_are_send_but_the_connection_is_not_sync() {
    assert_send::<Connection>();
    assert_send::<RtpPacket>();
    assert_send::<RtpWrite>();
}

#[test]
fn connection_runtime_methods_have_the_public_sans_io_signatures() {
    let _: fn(&mut Connection, Instant, Instant, NetworkInput) -> Result<(), ReceiveError> =
        Connection::receive;
    let _: fn(&mut Connection, Instant) -> Output = Connection::poll;
    let _: fn(&mut Connection, Instant, &str, RtpWrite) -> Result<(), pulsebeam_rtc::RejectedRtp> =
        Connection::write_rtp;
    let _: fn(&mut Connection, Option<pulsebeam_rtc::MediaPayloadBitrate>) =
        Connection::set_desired_bitrate;
    let _: fn(&Connection) -> StatsSnapshot = Connection::stats;
}

#[test]
fn negotiated_codec_facts_are_public_and_directional() {
    use pulsebeam_rtc::{CodecProfile, ConnectionEntropy, MediaKind, SdpOffer};
    let at = Instant::now();
    let offer =
        include_str!("fixtures/chrome-representative.sdp").replacen("a=sendonly", "a=recvonly", 1);
    let accepted = Connection::accept(
        ConnectionConfig {
            local_candidates: vec![LocalCandidate::Udp(SocketAddr::from((
                [192, 0, 2, 1],
                5000,
            )))],
            ..ConnectionConfig::default()
        },
        SdpOffer::new(offer),
        at,
        std::time::UNIX_EPOCH,
        ConnectionEntropy::new([9; 32]),
    )
    .expect("mixed-direction session");
    let inbound = accepted
        .session
        .inbound_media
        .first()
        .expect("video publisher");
    assert_eq!(inbound.kind, MediaKind::Video);
    assert_eq!(accepted.session.inbound_media.len(), 1);
    let codec = inbound.codecs.first().expect("primary source codec");
    assert_eq!(codec.payload_type, 102);
    assert_eq!(codec.retransmission_payload_type, Some(121));
    assert_eq!(codec.clock_rate, 90_000);
    assert_eq!(codec.channels, None);
    assert_eq!(
        codec.profile,
        CodecProfile::H264 {
            profile_level_id: Some(0x42e01f),
            packetization_mode: 1,
            level_asymmetry_allowed: true
        }
    );
    let sender = accepted.session.senders.first().expect("audio subscriber");
    assert_eq!(accepted.session.senders.len(), 1);
    assert_eq!(sender.codec.profile, CodecProfile::Opus);
    assert_eq!(sender.codec.payload_type, 111);
    assert_eq!(sender.codec.channels, Some(2));
    assert_eq!(sender.codec.clock_rate, 48_000);
    assert_eq!(sender.codec.retransmission_payload_type, None);
    let sections = &accepted.session.offered_media;
    assert_eq!(sections.len(), 3);
    assert_eq!(sections[0].media_index, 0);
    assert_eq!(sections[0].mid.as_ref(), "audio");
    assert_eq!(
        sections[0].direction,
        pulsebeam_rtc::MediaDirection::SendOnly
    );
    assert_eq!(
        sections[1].direction,
        pulsebeam_rtc::MediaDirection::ReceiveOnly
    );
    assert_eq!(
        sections[2].kind,
        pulsebeam_rtc::MediaSectionKind::Application
    );
    assert!(sections.iter().all(|section| !section.disabled));
}

#[test]
fn offer_facts_retain_inactive_disabled_and_bidirectional_sections() {
    use pulsebeam_rtc::{ConnectionEntropy, MediaDirection, SdpOffer};
    for (replacement, direction, disabled) in [
        ("a=inactive", MediaDirection::Inactive, false),
        ("a=sendrecv", MediaDirection::Bidirectional, false),
        ("a=sendonly", MediaDirection::Inactive, true),
    ] {
        let mut offer = include_str!("fixtures/chrome-representative.sdp").replace(
            "a=mid:video\na=sendonly",
            &format!("a=mid:video\n{replacement}"),
        );
        if disabled {
            offer = offer.replace("m=video 9 ", "m=video 0 ");
        }
        let accepted = Connection::accept(
            ConnectionConfig {
                local_candidates: vec![LocalCandidate::Udp("192.0.2.1:5000".parse().unwrap())],
                ..ConnectionConfig::default()
            },
            SdpOffer::new(offer),
            Instant::now(),
            std::time::UNIX_EPOCH,
            ConnectionEntropy::new([9; 32]),
        )
        .expect("supported RTC section direction");
        let section = &accepted.session.offered_media[1];
        assert_eq!(section.media_index, 1);
        assert_eq!(section.mid.as_ref(), "video");
        assert_eq!(section.direction, direction);
        assert_eq!(section.disabled, disabled);
        assert_eq!(accepted.session.offered_media.len(), 3);
        assert_eq!(
            accepted.session.inbound_media.len(),
            if direction == MediaDirection::Bidirectional {
                2
            } else {
                1
            }
        );
    }
}

#[test]
fn configured_hard_limits_reject_invalid_values() {
    let defaults = ConnectionLimits::default();
    assert_eq!(defaults.validate(), Ok(defaults));

    let invalid = ConnectionLimits {
        max_unsignaled_encodings: ConnectionLimits::HARD_MAX_UNSIGNALED_ENCODINGS + 1,
        ..defaults
    };
    assert!(invalid.validate().is_err());

    let invalid = ConnectionLimits {
        max_data_channels: ConnectionLimits::HARD_MAX_DATA_CHANNELS + 1,
        ..defaults
    };
    assert!(invalid.validate().is_err());

    let invalid = ConnectionLimits {
        max_inbound_data_message_bytes: ConnectionLimits::HARD_MAX_INBOUND_DATA_MESSAGE_BYTES + 1,
        ..defaults
    };
    assert!(invalid.validate().is_err());

    let invalid = ConnectionLimits {
        max_buffered_data_bytes: ConnectionLimits::HARD_MAX_BUFFERED_DATA_BYTES + 1,
        ..defaults
    };
    assert!(invalid.validate().is_err());

    let invalid = ConnectionLimits {
        max_queued_media_bytes: ConnectionLimits::HARD_MAX_QUEUED_MEDIA_BYTES + 1,
        ..defaults
    };
    assert!(invalid.validate().is_err());

    let invalid = ConnectionLimits {
        max_retransmission_bytes: ConnectionLimits::HARD_MAX_RETRANSMISSION_BYTES + 1,
        ..defaults
    };
    assert!(invalid.validate().is_err());

    assert!(ConnectionConfig::default().validate().is_ok());
}

#[test]
fn caller_ice_credentials_validate_bounds_and_redact_the_password() {
    let password = "0123456789ABCDEFGHIJKLMN";
    let credentials =
        LocalIceCredentials::new("0123456789ABCDEF", password).expect("routing credentials");
    assert_eq!(credentials.ufrag(), "0123456789ABCDEF");
    assert!(!format!("{credentials:?}").contains(password));
    assert!(LocalIceCredentials::new("a+/0", "a".repeat(22)).is_ok());
    assert!(LocalIceCredentials::new("a".repeat(256), "a".repeat(256)).is_ok());
    for (ufrag, password) in [
        ("a".repeat(3), "a".repeat(22)),
        ("a".repeat(257), "a".repeat(22)),
        ("a".repeat(4), "a".repeat(21)),
        ("a".repeat(4), "a".repeat(257)),
        ("route:peer".into(), "a".repeat(22)),
        ("route".into(), "invalid password with spaces".into()),
        ("routé".into(), "a".repeat(22)),
    ] {
        assert_eq!(
            LocalIceCredentials::new(ufrag, password),
            Err(AcceptError::InvalidConfiguration)
        );
    }
}

#[test]
fn local_candidate_configuration_is_bounded_and_structural() {
    let address = SocketAddr::from(([192, 0, 2, 1], 5000));
    assert!(ConnectionConfig::default().local_candidates.is_empty());
    let one = ConnectionConfig {
        local_candidates: vec![LocalCandidate::Udp(address)],
        ..ConnectionConfig::default()
    };
    assert!(one.validate().is_ok());

    let sixteen = ConnectionConfig {
        local_candidates: (0..16)
            .map(|index| LocalCandidate::Udp(SocketAddr::from(([192, 0, 2, 1], 5000 + index))))
            .collect(),
        ..ConnectionConfig::default()
    };
    assert_eq!(
        sixteen
            .validate()
            .map(|config| config.local_candidates.len()),
        Ok(16)
    );

    let seventeen = ConnectionConfig {
        local_candidates: (0..17)
            .map(|index| LocalCandidate::Udp(SocketAddr::from(([192, 0, 2, 1], 5000 + index))))
            .collect(),
        ..ConnectionConfig::default()
    };
    assert_eq!(
        seventeen.validate().err(),
        Some(AcceptError::SessionLimitExceeded)
    );

    for candidate in [
        LocalCandidate::Udp(SocketAddr::from(([0, 0, 0, 0], 5000))),
        LocalCandidate::Udp(SocketAddr::from(([224, 0, 0, 1], 5000))),
        LocalCandidate::Udp(SocketAddr::from(([255, 255, 255, 255], 5000))),
        LocalCandidate::Udp(SocketAddr::from(([192, 0, 2, 1], 0))),
        LocalCandidate::Udp(SocketAddr::from((Ipv6Addr::UNSPECIFIED, 5000))),
        LocalCandidate::Udp(SocketAddr::from((Ipv6Addr::LOCALHOST, 0))),
        LocalCandidate::TcpPassive(SocketAddr::from((
            "ff02::1".parse::<Ipv6Addr>().unwrap(),
            5000,
        ))),
        LocalCandidate::TcpPassive(SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::LOCALHOST,
            5000,
            1,
            0,
        ))),
        LocalCandidate::TcpPassive(SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::LOCALHOST,
            5000,
            0,
            1,
        ))),
    ] {
        let invalid = ConnectionConfig {
            local_candidates: vec![candidate],
            ..ConnectionConfig::default()
        };
        assert_eq!(
            invalid.validate().err(),
            Some(AcceptError::InvalidConfiguration)
        );
    }

    for candidate in [
        LocalCandidate::Udp(SocketAddr::from(([10, 0, 0, 1], 5000))),
        LocalCandidate::Udp(SocketAddr::from(([127, 0, 0, 1], 5000))),
        LocalCandidate::TcpPassive(SocketAddr::from(([169, 254, 1, 1], 5000))),
        LocalCandidate::TcpPassive(SocketAddr::from((Ipv6Addr::LOCALHOST, 5000))),
    ] {
        let valid = ConnectionConfig {
            local_candidates: vec![candidate],
            ..ConnectionConfig::default()
        };
        assert!(valid.validate().is_ok());
    }

    let duplicate = ConnectionConfig {
        local_candidates: vec![LocalCandidate::Udp(address), LocalCandidate::Udp(address)],
        ..ConnectionConfig::default()
    };
    assert_eq!(
        duplicate.validate().err(),
        Some(AcceptError::InvalidConfiguration)
    );

    let distinct_transports = ConnectionConfig {
        local_candidates: vec![
            LocalCandidate::Udp(address),
            LocalCandidate::TcpPassive(address),
        ],
        ..ConnectionConfig::default()
    };
    assert!(distinct_transports.validate().is_ok());
}

#[test]
fn explicit_playout_values_reject_unrepresentable_construction() {
    assert_eq!(
        PlayoutDelay::from_millis_exact(1, 10),
        Err(PlayoutDelayError::NotExactlyRepresentable)
    );
    assert_eq!(
        PlayoutDelay::from_ticks(2, 1),
        Err(PlayoutDelayError::Range)
    );
    let delay = PlayoutDelay::from_millis(149, 204).expect("valid millisecond bounds");
    assert_eq!(delay.min(), Duration::from_millis(149));
    assert_eq!(delay.max(), Duration::from_millis(204));
    assert_eq!(
        PlayoutDelay::from_millis(0, 40_951),
        Err(PlayoutDelayError::Range)
    );
    assert_eq!(
        PlayoutDelay::from_millis(2, 1),
        Err(PlayoutDelayError::Range)
    );
    assert_eq!(
        PlayoutDelay::from_millis(0, 40_950).map(PlayoutDelay::max),
        Ok(Duration::from_millis(40_950))
    );
}
