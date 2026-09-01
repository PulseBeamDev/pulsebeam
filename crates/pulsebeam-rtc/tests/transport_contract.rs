use std::error::Error;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

use pulsebeam_rtc::transport::{
    DatagramKind, Transport, TransportConfig, TransportError, TransportEvent,
};
use pulsebeam_rtc::{DtlsFingerprint, DtlsRole};
use str0m_reference::format::Codec;
use str0m_reference::media::MediaKind;
use str0m_reference::rtp::{RawPacket, RtpWrite, Ssrc};
use str0m_reference::{Candidate as ReferenceCandidate, Input, Output, Rtc};

#[test]
fn datagram_classifier_preserves_rfc5761_boundaries() {
    assert_eq!(
        Transport::classify(&[0x80, 192, 0, 1, 0, 0, 0, 0]),
        Some(DatagramKind::Rtcp)
    );
    assert_eq!(
        Transport::classify(&[0x80, 223, 0, 1, 0, 0, 0, 0]),
        Some(DatagramKind::Rtcp)
    );
    assert_eq!(
        Transport::classify(&[0x80, 224, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]),
        Some(DatagramKind::Rtp)
    );
    assert_eq!(
        Transport::classify(&[0x80, 201, 0, 1, 0, 0, 0, 0]),
        Some(DatagramKind::Rtcp)
    );
    assert_eq!(
        Transport::classify(&[0x80, 203, 0, 1, 0, 0, 0, 0]),
        Some(DatagramKind::Rtcp)
    );
}

#[test]
fn negotiated_rtp_payload_types_cannot_overlap_rtcp_mux() {
    assert_eq!(
        TransportConfig::validate_rtp_payload_types(&[64]),
        Err(TransportError::Configuration)
    );
    assert_eq!(
        TransportConfig::validate_rtp_payload_types(&[95]),
        Err(TransportError::Configuration)
    );
    assert!(TransportConfig::validate_rtp_payload_types(&[63, 96, 127]).is_ok());
}

fn address(port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
}

fn reference_fingerprint(
    fingerprint: &str0m_reference::crypto::Fingerprint,
) -> Result<DtlsFingerprint, &'static str> {
    DtlsFingerprint::new(
        fingerprint.hash_func.clone(),
        fingerprint.bytes.clone().into_boxed_slice(),
    )
    .ok_or("reference fingerprint is invalid")
}

fn upstream_fingerprint(fingerprint: &DtlsFingerprint) -> str0m_reference::crypto::Fingerprint {
    str0m_reference::crypto::Fingerprint {
        hash_func: fingerprint.algorithm().to_owned(),
        bytes: fingerprint.value().to_vec(),
    }
}

fn config(
    local: SocketAddr,
    remote: SocketAddr,
    certificate: str0m::crypto::dtls::DtlsCert,
    remote_fingerprint: DtlsFingerprint,
    local_credentials: is::IceCreds,
    remote_credentials: is::IceCreds,
) -> Result<TransportConfig, &'static str> {
    Ok(TransportConfig::new(
        local_credentials,
        is::Candidate::host(local, is::Protocol::Udp).map_err(|_| "local candidate")?,
        remote_credentials,
        vec![is::Candidate::host(remote, is::Protocol::Udp)
            .map_err(|_| "remote candidate")?]
        .into_boxed_slice(),
        certificate,
        remote_fingerprint,
        DtlsRole::Active,
    )
    .with_ice_role(true, 2))
}

fn drain_reference(
    reference: &mut Rtc,
    transport: &mut Transport,
    now: Instant,
    deadline: &mut Option<Instant>,
    saw_rtp: &mut bool,
    saw_rtcp: &mut bool,
    drop_dtls: &mut bool,
    last_rtp: &mut Option<(SocketAddr, SocketAddr, Vec<u8>)>,
) -> Result<(), Box<dyn Error>> {
    for _ in 0..256 {
        match reference.poll_output()? {
            Output::Timeout(value) => {
                *deadline = Some(value);
                return Ok(());
            }
            Output::Transmit(transmit) => {
                if Transport::classify(&transmit.contents) == Some(DatagramKind::Rtp) {
                    *last_rtp = Some((
                        transmit.source,
                        transmit.destination,
                        transmit.contents.to_vec(),
                    ));
                }
                if !*drop_dtls
                    && Transport::classify(&transmit.contents) == Some(DatagramKind::Dtls)
                {
                    *drop_dtls = true;
                    continue;
                }
                transport.handle_datagram(
                    now,
                    transmit.source,
                    transmit.destination,
                    transmit.contents.into(),
                )?;
            }
            Output::Event(event) => {
                if let Some(raw) = event.as_raw_packet() {
                    match raw {
                        RawPacket::RtpRx(_, _) => *saw_rtp = true,
                        RawPacket::RtcpRx(_) => *saw_rtcp = true,
                        RawPacket::RtpTx(_, _) | RawPacket::RtcpTx(_) => {}
                    }
                }
            }
        }
    }
    Err("reference output did not reach a timeout".into())
}

fn drive_reference(
    reference: &mut Rtc,
    transport: &mut Transport,
    now: &mut Instant,
    reference_deadline: &mut Option<Instant>,
    saw_rtp: &mut bool,
    saw_rtcp: &mut bool,
    drop_dtls: &mut bool,
    last_rtp: &mut Option<(SocketAddr, SocketAddr, Vec<u8>)>,
) -> Result<(), Box<dyn Error>> {
    for _ in 0..500 {
        let mut progress = false;
        while let Some(transmit) = transport.poll_transmit() {
            progress = true;
            let receive = str0m_reference::net::Receive::new(
                str0m_reference::net::Protocol::Udp,
                transmit.source,
                transmit.destination,
                &transmit.bytes,
            )?;
            reference.handle_input(Input::Receive(*now, receive))?;
            drain_reference(
                reference,
                transport,
                *now,
                reference_deadline,
                saw_rtp,
                saw_rtcp,
                drop_dtls,
                last_rtp,
            )?;
        }
        let before = *reference_deadline;
        drain_reference(
            reference,
            transport,
            *now,
            reference_deadline,
            saw_rtp,
            saw_rtcp,
            drop_dtls,
            last_rtp,
        )?;
        progress |= before != *reference_deadline;
        if transport.state() == pulsebeam_rtc::transport::TransportState::Connected
            && reference.is_connected()
        {
            return Ok(());
        }
        if !progress {
            *now = now
                .checked_add(Duration::from_millis(50))
                .ok_or("reference clock overflow")?;
            if transport.next_deadline().is_some_and(|value| value <= *now) {
                transport.handle_timeout(*now)?;
            }
            if reference_deadline.is_some_and(|value| value <= *now) {
                reference.handle_input(Input::Timeout(*now))?;
                *reference_deadline = None;
            }
        }
    }
    Err("standalone and upstream peers did not connect".into())
}

#[test]
fn standalone_transport_interoperates_with_upstream_reference_peer() -> Result<(), Box<dyn Error>> {
    let start = Instant::now();
    let local = address(6000);
    let remote = address(6001);
    let local_credentials = is::IceCreds {
        ufrag: "standalone".to_owned(),
        pass: "standalone-password-123456789".to_owned(),
    };
    let mut reference = Rtc::builder()
        .set_rtp_mode(true)
        .enable_raw_packets(true)
        .set_rtcp_report_interval_audio(Duration::from_millis(100))
        .build(start);
    reference.add_local_candidate(
        ReferenceCandidate::host(remote, "udp").expect("reference local candidate"),
    );
    reference.add_remote_candidate(
        ReferenceCandidate::host(local, "udp").expect("reference remote candidate"),
    );
    let (reference_fingerprint_value, reference_credentials) = {
        let direct = reference.direct_api();
        (
            direct.local_dtls_fingerprint().clone(),
            direct.local_ice_credentials(),
        )
    };
    let certificate = str0m::crypto::from_feature_flags()
        .dtls_provider
        .generate_certificate()
        .expect("standalone certificate");
    let mut transport = Transport::new(
        config(
            local,
            remote,
            certificate,
            reference_fingerprint(&reference_fingerprint_value)?,
            local_credentials.clone(),
            is::IceCreds {
                ufrag: reference_credentials.ufrag.clone(),
                pass: reference_credentials.pass,
            },
        )?,
        start,
    )?;
    {
        let mut direct = reference.direct_api();
        direct.set_remote_fingerprint(upstream_fingerprint(transport.local_fingerprint()));
        direct.set_remote_ice_credentials(str0m_reference::IceCreds {
            ufrag: local_credentials.ufrag,
            pass: local_credentials.pass,
        });
        direct.set_ice_controlling(false);
        direct.start_dtls(false)?;
        direct.declare_media("aud".into(), MediaKind::Audio);
        direct.expect_stream_rx(Ssrc::from(7), None, "aud".into(), None);
        direct.declare_stream_tx(Ssrc::from(8), None, "aud".into(), None);
    }
    let mut now = start;
    let mut reference_deadline = None;
    let mut saw_reference_rtp = false;
    let mut saw_reference_rtcp = false;
    let mut drop_reference_dtls = false;
    let mut last_reference_rtp = None;
    transport.handle_datagram(
        now,
        address(6999),
        local,
        vec![22; 13],
    )?;
    drive_reference(
        &mut reference,
        &mut transport,
        &mut now,
        &mut reference_deadline,
        &mut saw_reference_rtp,
        &mut saw_reference_rtcp,
        &mut drop_reference_dtls,
        &mut last_reference_rtp,
    )?;
    assert_eq!(
        transport.state(),
        pulsebeam_rtc::transport::TransportState::Connected
    );
    assert!(drop_reference_dtls);

    let rtp = vec![0x80, 111, 0, 1, 0, 0, 0, 1, 0, 0, 0, 7, 1, 2, 3];
    transport.send_rtp(&rtp)?;
    drive_reference(
        &mut reference,
        &mut transport,
        &mut now,
        &mut reference_deadline,
        &mut saw_reference_rtp,
        &mut saw_reference_rtcp,
        &mut drop_reference_dtls,
        &mut last_reference_rtp,
    )?;
    assert!(saw_reference_rtp);

    let pt = reference
        .codec_config()
        .find(|params| params.spec().codec == Codec::Opus)
        .ok_or("reference Opus payload missing")?
        .pt();
    {
        let mut direct = reference.direct_api();
        let stream = direct
            .stream_tx(&Ssrc::from(8))
            .ok_or("reference stream missing")?;
        stream.write_rtp(RtpWrite::new(pt, 1_u64.into(), 1, now, [4, 5, 6]));
    }
    now = now
        .checked_add(Duration::from_millis(100))
        .ok_or("reference clock overflow")?;
    reference.handle_input(Input::Timeout(now))?;
    drive_reference(
        &mut reference,
        &mut transport,
        &mut now,
        &mut reference_deadline,
        &mut saw_reference_rtp,
        &mut saw_reference_rtcp,
        &mut drop_reference_dtls,
        &mut last_reference_rtp,
    )?;
    assert!(
        std::iter::from_fn(|| transport.poll_event()).any(
            |event| matches!(event, TransportEvent::Rtp { metadata, .. } if metadata.ssrc == 8)
        )
    );
    let (source, destination, packet) = last_reference_rtp
        .as_ref()
        .ok_or("reference RTP transmit was not captured")?
        .clone();
    transport.handle_datagram(now, source, destination, packet.clone())?;
    transport.handle_datagram(now, source, destination, packet)?;
    assert!(!std::iter::from_fn(|| transport.poll_event()).any(
        |event| matches!(event, TransportEvent::Rtp { metadata, .. } if metadata.ssrc == 8)
    ));

    transport.send_rtcp(&[0x80, 201, 0, 1, 0, 0, 0, 7])?;
    drive_reference(
        &mut reference,
        &mut transport,
        &mut now,
        &mut reference_deadline,
        &mut saw_reference_rtp,
        &mut saw_reference_rtcp,
        &mut drop_reference_dtls,
        &mut last_reference_rtp,
    )?;
    assert!(saw_reference_rtcp);

    let mut saw_standalone_rtcp = false;
    for _ in 0..8 {
        now = now
            .checked_add(Duration::from_millis(500))
            .ok_or("reference clock overflow")?;
        reference.handle_input(Input::Timeout(now))?;
        drain_reference(
            &mut reference,
            &mut transport,
            now,
            &mut reference_deadline,
            &mut saw_reference_rtp,
            &mut saw_reference_rtcp,
            &mut drop_reference_dtls,
            &mut last_reference_rtp,
        )?;
        while let Some(event) = transport.poll_event() {
            if matches!(event, TransportEvent::Rtcp(_)) {
                saw_standalone_rtcp = true;
            }
        }
        if saw_standalone_rtcp {
            break;
        }
    }
    assert!(saw_standalone_rtcp);

    transport.close(now)?;
    let close_transmit = transport
        .poll_transmit()
        .ok_or("standalone close did not emit DTLS alert")?;
    assert_eq!(close_transmit.kind, DatagramKind::Dtls);
    let receive = str0m_reference::net::Receive::new(
        str0m_reference::net::Protocol::Udp,
        close_transmit.source,
        close_transmit.destination,
        &close_transmit.bytes,
    )?;
    reference.handle_input(Input::Receive(now, receive))?;
    let mut reference_closed = false;
    for _ in 0..64 {
        match reference.poll_output()? {
            Output::Event(event) if matches!(event, str0m_reference::Event::Closed) => {
                reference_closed = true;
                break;
            }
            Output::Timeout(_) => break,
            Output::Transmit(_) | Output::Event(_) => {}
        }
    }
    assert!(reference_closed);
    Ok(())
}

#[test]
fn upstream_reference_corruption_fails_standalone_terminally() -> Result<(), Box<dyn Error>> {
    let start = Instant::now();
    let local = address(6010);
    let remote = address(6011);
    let local_credentials = is::IceCreds {
        ufrag: "standalone-corrupt".to_owned(),
        pass: "standalone-password-987654321".to_owned(),
    };
    let mut reference = Rtc::builder()
        .set_rtp_mode(true)
        .enable_raw_packets(true)
        .build(start);
    reference.add_local_candidate(
        ReferenceCandidate::host(remote, "udp").map_err(|_| "reference local candidate")?,
    );
    reference.add_remote_candidate(
        ReferenceCandidate::host(local, "udp").map_err(|_| "reference remote candidate")?,
    );
    let (reference_fingerprint_value, reference_credentials) = {
        let direct = reference.direct_api();
        (
            direct.local_dtls_fingerprint().clone(),
            direct.local_ice_credentials(),
        )
    };
    let certificate = str0m::crypto::from_feature_flags()
        .dtls_provider
        .generate_certificate()
        .ok_or("standalone certificate")?;
    let mut transport = Transport::new(
        config(
            local,
            remote,
            certificate,
            reference_fingerprint(&reference_fingerprint_value)?,
            local_credentials.clone(),
            is::IceCreds {
                ufrag: reference_credentials.ufrag.clone(),
                pass: reference_credentials.pass,
            },
        )?,
        start,
    )?;
    {
        let mut direct = reference.direct_api();
        direct.set_remote_fingerprint(upstream_fingerprint(transport.local_fingerprint()));
        direct.set_remote_ice_credentials(str0m_reference::IceCreds {
            ufrag: local_credentials.ufrag,
            pass: local_credentials.pass,
        });
        direct.set_ice_controlling(false);
        direct.start_dtls(false)?;
        direct.declare_media("aud".into(), MediaKind::Audio);
        direct.declare_stream_tx(Ssrc::from(18), None, "aud".into(), None);
    }
    let mut now = start;
    let mut reference_deadline = None;
    let mut saw_reference_rtp = false;
    let mut saw_reference_rtcp = false;
    let mut drop_reference_dtls = false;
    let mut last_reference_rtp = None;
    drive_reference(
        &mut reference,
        &mut transport,
        &mut now,
        &mut reference_deadline,
        &mut saw_reference_rtp,
        &mut saw_reference_rtcp,
        &mut drop_reference_dtls,
        &mut last_reference_rtp,
    )?;
    let pt = reference
        .codec_config()
        .find(|params| params.spec().codec == Codec::Opus)
        .ok_or("reference Opus payload missing")?
        .pt();
    {
        let mut direct = reference.direct_api();
        let stream = direct
            .stream_tx(&Ssrc::from(18))
            .ok_or("reference stream missing")?;
        stream.write_rtp(RtpWrite::new(pt, 5_u64.into(), 5, now, [8, 9, 10]));
    }
    now = now
        .checked_add(Duration::from_millis(100))
        .ok_or("reference clock overflow")?;
    reference.handle_input(Input::Timeout(now))?;
    drive_reference(
        &mut reference,
        &mut transport,
        &mut now,
        &mut reference_deadline,
        &mut saw_reference_rtp,
        &mut saw_reference_rtcp,
        &mut drop_reference_dtls,
        &mut last_reference_rtp,
    )?;
    let (source, destination, mut corrupt) = last_reference_rtp
        .ok_or("reference RTP transmit was not captured")?;
    let last = corrupt.len().checked_sub(1).ok_or("RTP auth tag missing")?;
    corrupt[last] ^= 1;
    assert_eq!(
        transport.handle_datagram(now, source, destination, corrupt),
        Err(TransportError::Crypto)
    );
    assert_eq!(transport.state(), pulsebeam_rtc::transport::TransportState::Failed);
    assert!(transport.poll_transmit().is_none());
    Ok(())
}
