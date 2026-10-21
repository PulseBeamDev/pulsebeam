#![allow(
    clippy::disallowed_types,
    reason = "the public network contract is intentionally Bytes-backed"
)]

use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

use bytes::Bytes;
use pulsebeam_rtc::{
    AcceptError, Connection, ConnectionConfig, ConnectionEntropy, LocalCandidate, NetworkInput,
    Output, SdpOffer,
};

fn at(monotonic: Instant) -> Instant {
    monotonic
}

fn connection(start: Instant) -> Result<Connection, AcceptError> {
    Ok(Connection::accept(
        ConnectionConfig {
            local_candidates: vec![LocalCandidate::Udp(SocketAddr::from((
                [192, 0, 2, 1],
                5000,
            )))],
            ..ConnectionConfig::default()
        },
        SdpOffer::new(include_str!("fixtures/chrome-representative.sdp")),
        at(start),
        std::time::UNIX_EPOCH,
        ConnectionEntropy::new([7; 32]),
    )?
    .connection)
}

#[allow(
    clippy::panic,
    reason = "bounded test fixture stops at the first protocol invariant violation"
)]
fn next_deadline(connection: &mut Connection, at: Instant) -> Instant {
    for _ in 0..16 {
        match connection.poll(at) {
            Output::Idle {
                next_wakeup: Some(next),
            } if next > at => return next,
            Output::Idle {
                next_wakeup: Some(next),
            } => assert_eq!(next, at),
            Output::Event(pulsebeam_rtc::Event::BandwidthEstimate(_)) => {}
            output => panic!("expected bounded ICE setup progress: {output:?}"),
        }
    }
    panic!("ICE did not expose a future timer");
}

#[test]
fn public_runtime_waits_for_authenticated_ice_before_transmitting() -> Result<(), AcceptError> {
    let start = Instant::now();
    let mut connection = connection(start)?;

    let deadline = next_deadline(&mut connection, start);

    assert!(deadline >= start);
    assert!(!matches!(
        connection.poll(at(deadline)),
        Output::Transmit(_)
    ));
    Ok(())
}

#[test]
fn malformed_input_is_dropped_without_disrupting_the_runtime_timer() -> Result<(), AcceptError> {
    let start = Instant::now();
    let mut connection = connection(start)?;
    let before = next_deadline(&mut connection, start);

    assert_eq!(
        connection.receive(
            at(start + Duration::from_millis(1)),
            at(start + Duration::from_millis(1)),
            NetworkInput::Udp {
                local: SocketAddr::from(([192, 0, 2, 1], 5000)),
                remote: SocketAddr::from(([192, 0, 2, 2], 5001)),
                ecn: None,
                payload: Bytes::new(),
            },
        ),
        Ok(())
    );

    assert_eq!(
        next_deadline(&mut connection, start + Duration::from_millis(1)),
        before
    );
    Ok(())
}
