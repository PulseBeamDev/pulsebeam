#![allow(clippy::disallowed_types, reason = "network input owns Bytes")]

use bytes::Bytes;
use pulsebeam_rtc::{
    AcceptError, CloseReason, Connection, ConnectionConfig, ConnectionEntropy, ConnectionState,
    LocalCandidate, NetworkInput, Output, ReceiveError, SdpOffer, WriteError,
};
use std::{
    net::SocketAddr,
    time::{Instant, UNIX_EPOCH},
};

fn connection(start: Instant) -> Result<Connection, AcceptError> {
    Ok(Connection::accept(
        ConnectionConfig {
            local_candidates: vec![LocalCandidate::Udp(SocketAddr::from((
                [192, 0, 2, 1],
                5000,
            )))],
            ..ConnectionConfig::default()
        },
        SdpOffer::new(
            include_str!("fixtures/chrome-representative.sdp").replace("a=sendonly", "a=sendrecv"),
        ),
        start,
        UNIX_EPOCH,
        ConnectionEntropy::new([13; 32]),
    )?
    .connection)
}

#[test]
fn abort_discards_output_and_emits_terminal_once() -> Result<(), AcceptError> {
    let now = Instant::now();
    let mut connection = connection(now)?;
    connection.abort();
    assert!(matches!(
        connection.poll(now),
        Output::Closed(CloseReason::Aborted)
    ));
    connection.abort();
    assert!(matches!(
        connection.poll(now),
        Output::Idle { next_wakeup: None }
    ));
    assert_eq!(
        connection.receive(
            now,
            now,
            NetworkInput::Udp {
                local: SocketAddr::from(([192, 0, 2, 1], 5000)),
                remote: SocketAddr::from(([192, 0, 2, 2], 5001)),
                ecn: None,
                payload: Bytes::new(),
            }
        ),
        Err(ReceiveError::Closed)
    );
    let stats = connection.stats();
    assert_eq!(stats.connection.state, ConnectionState::Closed);
    assert_eq!(stats.connection.close_reason, Some(CloseReason::Aborted));
    assert_eq!(stats.connection.transmitted_protocol_bytes, 0);
    Ok(())
}

#[test]
fn graceful_close_is_idempotent_and_honors_an_immediate_deadline() -> Result<(), AcceptError> {
    let now = Instant::now();
    let mut connection = connection(now)?;
    assert_eq!(connection.close(now, now), Ok(()));
    assert_eq!(connection.close(now, now), Ok(()));
    for _ in 0..16 {
        match connection.poll(now) {
            Output::Closed(CloseReason::Graceful) => break,
            Output::Event(pulsebeam_rtc::Event::BandwidthEstimate(_)) => {}
            output => panic!("unexpected close output: {output:?}"),
        }
    }
    assert!(matches!(
        connection.poll(now),
        Output::Idle { next_wakeup: None }
    ));
    assert_eq!(connection.close(now, now), Err(WriteError::Closed));
    Ok(())
}

#[test]
fn stats_are_owned_snapshots_and_include_negotiated_senders() -> Result<(), AcceptError> {
    let now = Instant::now();
    let connection = connection(now)?;
    let before = connection.stats();
    let mut owned = before.clone();
    owned.senders.clear();
    assert_eq!(before, connection.stats());
    assert_eq!(before.connection.state, ConnectionState::Open);
    assert_eq!(before.connection.close_reason, None);
    assert!(before.connection.feedback.is_some());
    assert!(!before.senders.is_empty());
    Ok(())
}
