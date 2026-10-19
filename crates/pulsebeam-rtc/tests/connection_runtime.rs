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
    AcceptError, Connection, ConnectionConfig, ConnectionEntropy, GlobalMediaTime, LocalCandidate,
    NetworkInput, Output, SdpOffer, TimePoint,
};

fn at(monotonic: Instant) -> TimePoint {
    TimePoint {
        monotonic,
        global: GlobalMediaTime::from_micros(7),
    }
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
        ConnectionEntropy::new([7; 32]),
    )?
    .connection)
}

#[test]
fn public_runtime_waits_for_authenticated_ice_before_transmitting() -> Result<(), AcceptError> {
    let start = Instant::now();
    let mut connection = connection(start)?;

    let Output::Idle {
        next_wakeup: Some(deadline),
    } = connection.poll(at(start))
    else {
        panic!("ICE-lite must wait for inbound checks and expose its timer");
    };

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
    let _ = connection.poll(at(start));
    let Output::Idle {
        next_wakeup: Some(before),
    } = connection.poll(at(start))
    else {
        panic!("initial ICE timer is available");
    };

    assert_eq!(
        connection.receive(
            at(start + Duration::from_millis(1)),
            pulsebeam_rtc::PacketObservation {
                at: at(start + Duration::from_millis(1)),
                provenance: 0
            },
            NetworkInput::Udp {
                local: SocketAddr::from(([192, 0, 2, 1], 5000)),
                remote: SocketAddr::from(([192, 0, 2, 2], 5001)),
                ecn: None,
                payload: Bytes::new(),
            },
        ),
        Ok(())
    );

    assert!(matches!(
        connection.poll(at(start + Duration::from_millis(1))),
        Output::Idle {
            next_wakeup: Some(after)
        } if after == before
    ));
    Ok(())
}
