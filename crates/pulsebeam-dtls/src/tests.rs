#![allow(
    clippy::arithmetic_side_effects,
    reason = "Fixture arithmetic failures should fail the test rather than saturate."
)]

use std::time::Duration;

use super::*;

mod cc;

fn test_start() -> Instant {
    Instant::now()
}

#[test]
fn dtls_srtp_exports_only_after_mutual_pinned_authentication() -> Result<(), Error> {
    for version in [SrtpVersion::Dtls12, SrtpVersion::Dtls13] {
        for (wrong_client, wrong_server) in [(false, false), (true, false), (false, true)] {
            let client_identity = LocalIdentity::generate()?;
            let server_identity = LocalIdentity::generate()?;
            let mut now = test_start();
            let mut client = Association::dtls_srtp(
                Role::Client,
                &client_identity,
                if wrong_server {
                    client_identity.fingerprint()
                } else {
                    server_identity.fingerprint()
                },
                1200,
                now,
                SrtpVersion::Auto,
            )?;
            let mut server = Association::dtls_srtp(
                Role::Server,
                &server_identity,
                if wrong_client {
                    server_identity.fingerprint()
                } else {
                    client_identity.fingerprint()
                },
                1200,
                now,
                version,
            )?;
            let mut client_trace = Trace::default();
            let mut server_trace = Trace::default();
            advance(
                &mut client,
                &mut server,
                &mut client_trace,
                &mut server_trace,
                now,
            )?;
            let result = drive(
                &mut client,
                &mut server,
                &mut client_trace,
                &mut server_trace,
                &mut now,
            );
            if wrong_client || wrong_server {
                assert!(matches!(result, Err(Error::PeerMismatch)));
                let (rejected, trace) = if wrong_client {
                    (&server, &server_trace)
                } else {
                    (&client, &client_trace)
                };
                assert!(!rejected.is_connected());
                assert_eq!(trace.connected, 0);
                assert!(trace.material.is_none());
                continue;
            }
            result?;
            let (client_keys, client_profile) = client_trace
                .material
                .take()
                .expect("authenticated client exporter");
            let (server_keys, server_profile) = server_trace
                .material
                .take()
                .expect("authenticated server exporter");
            assert_eq!(&*client_keys, &*server_keys);
            assert_eq!(client_profile, server_profile);
            assert_eq!(client.negotiated_srtp_profile(), Some(client_profile));
            assert_eq!(
                client.protocol_version(),
                Some(match version {
                    SrtpVersion::Dtls12 => ProtocolVersion::DTLS1_2,
                    _ => ProtocolVersion::DTLS1_3,
                })
            );
            client.send_application_data(b"SCTP payload")?;
            deliver(
                &mut client,
                &mut server,
                &mut client_trace,
                &mut server_trace,
            )?;
            assert_eq!(server_trace.application, [b"SCTP payload".to_vec()]);
            client.shutdown()?;
            deliver(
                &mut client,
                &mut server,
                &mut client_trace,
                &mut server_trace,
            )?;
            assert!(!server.is_connected());
        }
    }
    Ok(())
}

#[test]
fn imported_identity_preserves_the_public_pin_and_rejects_invalid_keys() -> Result<(), Error> {
    let generated = LocalIdentity::generate()?;
    let imported = LocalIdentity::from_der(
        generated.certificate_der().to_vec(),
        generated.0.private_key.clone(),
    )?;
    assert_eq!(imported.fingerprint(), generated.fingerprint());
    assert!(matches!(
        LocalIdentity::from_der(Vec::new(), Vec::new()),
        Err(Error::InvalidIdentity)
    ));
    assert!(matches!(
        LocalIdentity::from_der(vec![1], vec![1]),
        Err(Error::InvalidIdentity)
    ));
    Ok(())
}

#[derive(Default)]
struct Trace {
    packets: Vec<Vec<u8>>,
    application: Vec<Vec<u8>>,
    connected: usize,
    material: Option<(KeyingMaterial, SrtpProfile)>,
    application_records: usize,
    control_packets: usize,
    wake: Option<Instant>,
}

fn drain(association: &mut Association, trace: &mut Trace) -> Result<(), Error> {
    let mut buffer = vec![0; 4096];
    for _ in 0..128 {
        match association.poll_output(&mut buffer)? {
            Output::Datagram(bytes) => {
                let records = association.last_output_application_records();
                trace.application_records += records;
                trace.control_packets += usize::from(records == 0);
                trace.packets.push(bytes.to_vec());
            }
            Output::ApplicationData(bytes) => trace.application.push(bytes.to_vec()),
            Output::Connected => trace.connected += 1,
            Output::KeyingMaterial(material, profile) => {
                assert!(association.is_connected());
                assert_eq!(trace.connected, 1);
                assert!(trace.material.is_none());
                trace.material = Some((material, profile));
            }
            Output::Timeout(at) => {
                trace.wake = Some(at);
                return Ok(());
            }
            Output::Progress => {}
            Output::Closed => return Ok(()),
            Output::BufferTooSmall { needed } => buffer.resize(needed, 0),
        }
    }
    panic!("output did not quiesce");
}

fn deliver(
    source: &mut Association,
    destination: &mut Association,
    source_trace: &mut Trace,
    destination_trace: &mut Trace,
) -> Result<bool, Error> {
    drain(source, source_trace)?;
    let packets = std::mem::take(&mut source_trace.packets);
    let moved = !packets.is_empty();
    for packet in packets {
        destination.handle_packet(&packet)?;
        drain(destination, destination_trace)?;
    }
    Ok(moved)
}

fn advance(
    client: &mut Association,
    server: &mut Association,
    client_trace: &mut Trace,
    server_trace: &mut Trace,
    now: Instant,
) -> Result<(), Error> {
    client.handle_timeout(now)?;
    drain(client, client_trace)?;
    server.handle_timeout(now)?;
    drain(server, server_trace)
}

fn drive(
    client: &mut Association,
    server: &mut Association,
    client_trace: &mut Trace,
    server_trace: &mut Trace,
    now: &mut Instant,
) -> Result<(), Error> {
    for _ in 0..64 {
        let forward = deliver(client, server, client_trace, server_trace)?;
        let reverse = deliver(server, client, server_trace, client_trace)?;
        if !forward && !reverse {
            if client.is_connected() && server.is_connected() {
                return Ok(());
            }
            *now = client_trace.wake.unwrap().min(server_trace.wake.unwrap());
            advance(client, server, client_trace, server_trace, *now)?;
        }
    }
    panic!("handshake or datagrams did not converge");
}

#[test]
fn public_associations_authenticate_retransmit_and_preserve_datagrams() -> Result<(), Error> {
    let client_identity = LocalIdentity::generate()?;
    let server_identity = LocalIdentity::generate()?;
    let origin = test_start();
    let mut now = origin;
    let mut client = Association::new(
        Role::Client,
        &client_identity,
        server_identity.fingerprint(),
        4096,
        now,
    )?;
    let mut server = Association::new(
        Role::Server,
        &server_identity,
        client_identity.fingerprint(),
        8192,
        now,
    )?;
    let (mut client_trace, mut server_trace) = (Trace::default(), Trace::default());
    assert!(matches!(
        client.send_application_data(b"not yet authenticated"),
        Err(Error::NotConnected)
    ));
    advance(
        &mut client,
        &mut server,
        &mut client_trace,
        &mut server_trace,
        now,
    )?;
    assert!(!client_trace.packets.is_empty());
    client_trace.packets.clear(); // Lose the initial client flight.
    now = client_trace.wake.unwrap();
    assert!(now > origin);
    advance(
        &mut client,
        &mut server,
        &mut client_trace,
        &mut server_trace,
        now,
    )?;
    drive(
        &mut client,
        &mut server,
        &mut client_trace,
        &mut server_trace,
        &mut now,
    )?;
    assert_eq!((client_trace.connected, server_trace.connected), (1, 1));
    assert!(client_trace.application.is_empty() && server_trace.application.is_empty());
    assert_eq!(
        (
            client_trace.application_records,
            server_trace.application_records
        ),
        (0, 0)
    );
    assert!(client_trace.control_packets > 0 && server_trace.control_packets > 0);

    for payload in [Vec::new(), b"client payload".to_vec(), vec![0xa5; 3072]] {
        client.send_application_data(&payload)?;
        drive(
            &mut client,
            &mut server,
            &mut client_trace,
            &mut server_trace,
            &mut now,
        )?;
    }
    server.send_application_data(&[0x5a; 6144])?;
    drive(
        &mut client,
        &mut server,
        &mut client_trace,
        &mut server_trace,
        &mut now,
    )?;
    assert_eq!(
        server_trace.application,
        [Vec::new(), b"client payload".to_vec(), vec![0xa5; 3072]]
    );
    assert_eq!(client_trace.application, [vec![0x5a; 6144]]);
    assert_eq!(
        (
            client_trace.application_records,
            server_trace.application_records
        ),
        (3, 1)
    );
    assert!(matches!(
        client.handle_timeout(origin),
        Err(Error::TimeWentBackwards)
    ));
    assert!(client.is_connected());
    client.close();
    assert!(!client.is_connected());
    assert!(matches!(
        client.send_application_data(b"closed"),
        Err(Error::Closed)
    ));
    assert!(matches!(client.poll_output(&mut [])?, Output::Closed));
    Ok(())
}

#[test]
fn either_roles_wrong_pin_fails_closed_before_application_access() -> Result<(), Error> {
    let client_identity = LocalIdentity::generate()?;
    let server_identity = LocalIdentity::generate()?;
    for wrong_client_pin in [true, false] {
        let mut client_pin = server_identity.fingerprint().to_bytes();
        let mut server_pin = client_identity.fingerprint().to_bytes();
        if wrong_client_pin {
            client_pin[0] ^= 1;
        } else {
            server_pin[0] ^= 1;
        }
        let mut now = test_start();
        let mut client = Association::new(
            Role::Client,
            &client_identity,
            Fingerprint::from_bytes(client_pin),
            1500,
            now,
        )?;
        let mut server = Association::new(
            Role::Server,
            &server_identity,
            Fingerprint::from_bytes(server_pin),
            1500,
            now,
        )?;
        let (mut client_trace, mut server_trace) = (Trace::default(), Trace::default());
        advance(
            &mut client,
            &mut server,
            &mut client_trace,
            &mut server_trace,
            now,
        )?;
        assert!(matches!(
            drive(
                &mut client,
                &mut server,
                &mut client_trace,
                &mut server_trace,
                &mut now
            ),
            Err(Error::PeerMismatch)
        ));
        assert!(client_trace.application.is_empty() && server_trace.application.is_empty());
        let (rejected, trace) = if wrong_client_pin {
            (&mut client, &client_trace)
        } else {
            (&mut server, &server_trace)
        };
        assert_eq!(trace.connected, 0);
        assert!(!rejected.is_connected());
        assert!(matches!(
            rejected.send_application_data(b"must not escape"),
            Err(Error::Closed)
        ));
        assert!(matches!(rejected.poll_output(&mut [])?, Output::Closed));
    }
    Ok(())
}

fn drain_legacy(peer: &mut Dtls, packets: &mut Vec<Vec<u8>>) {
    let mut buffer = vec![0; 4096];
    for _ in 0..128 {
        match peer.poll_output(&mut buffer) {
            dimpl::Output::Packet(bytes) => packets.push(bytes.to_vec()),
            dimpl::Output::Timeout(_) | dimpl::Output::CloseNotify => return,
            dimpl::Output::Connected => panic!("legacy peer negotiated a fallback"),
            dimpl::Output::BufferTooSmall { needed } => buffer.resize(needed, 0),
            _ => {}
        }
    }
    panic!("legacy output did not quiesce");
}

#[test]
fn a_real_dtls12_peer_cannot_negotiate_a_fallback() -> Result<(), Error> {
    let local = LocalIdentity::generate()?;
    let legacy_identity = LocalIdentity::generate()?;
    let origin = test_start();
    let mut server = Association::new(
        Role::Server,
        &local,
        legacy_identity.fingerprint(),
        1500,
        origin,
    )?;
    #[allow(
        clippy::disallowed_types,
        reason = "The negative-test peer requires immutable Arc<Config> at construction."
    )]
    let legacy_config = std::sync::Arc::new(
        Config::builder()
            .with_crypto_provider(dimpl::crypto::aws_lc_rs::default_provider())
            .build()?,
    );
    let mut legacy = Dtls::new_12(legacy_config, legacy_identity.0, origin);
    legacy.set_active(true);
    let mut server_trace = Trace::default();
    let mut attempted = false;
    let mut terminated = false;
    let mut legacy_packets = Vec::new();
    'simulation: for step in 0..64 {
        let now = origin + Duration::from_secs(step);
        if server.handle_timeout(now).is_err() || legacy.handle_timeout(now).is_err() {
            terminated = true;
            break;
        }
        drain(&mut server, &mut server_trace)?;
        drain_legacy(&mut legacy, &mut legacy_packets);
        for packet in std::mem::take(&mut legacy_packets) {
            attempted = true;
            match server.handle_packet(&packet) {
                Ok(()) => drain(&mut server, &mut server_trace)?,
                Err(Error::Protocol(_)) => {
                    terminated = true;
                    break 'simulation;
                }
                Err(error) => return Err(error),
            }
        }
        for packet in std::mem::take(&mut server_trace.packets) {
            if legacy.handle_packet(&packet).is_err() {
                terminated = true;
                break 'simulation;
            }
            drain_legacy(&mut legacy, &mut legacy_packets);
        }
        assert!(!server.is_connected());
    }
    assert!(attempted && terminated);
    assert!(!server.is_connected());
    assert_eq!(server_trace.connected, 0);
    assert!(server_trace.application.is_empty());
    Ok(())
}
