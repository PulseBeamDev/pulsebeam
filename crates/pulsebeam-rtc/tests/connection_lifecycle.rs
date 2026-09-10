#![allow(
    clippy::disallowed_types,
    reason = "the public network input contract is intentionally Bytes-backed"
)]

use std::{net::SocketAddr, time::Instant};

use bytes::Bytes;
use pulsebeam_rtc::{
    AcceptError, CloseReason, Command, CommandError, Connection, ConnectionConfig,
    ConnectionEntropy, ConnectionState, GlobalMediaTime, LocalCandidate, NetworkInput, Output,
    ReceiveError, SdpOffer, TimePoint,
};

fn at(monotonic: Instant) -> TimePoint {
    TimePoint {
        monotonic,
        global: GlobalMediaTime::from_micros(1),
    }
}

fn connection(start: Instant) -> Result<Connection, AcceptError> {
    let accepted = Connection::accept(
        ConnectionConfig {
            local_candidates: vec![LocalCandidate::Udp(SocketAddr::from((
                [192, 0, 2, 1],
                5000,
            )))],
            ..ConnectionConfig::default()
        },
        SdpOffer::new(include_str!("fixtures/chrome-representative.sdp")),
        at(start),
        ConnectionEntropy::new([13; 32]),
    )?;
    Ok(accepted.connection)
}

#[test]
fn abort_discards_queued_output_and_emits_terminal_once() -> Result<(), AcceptError> {
    let now = Instant::now();
    let mut connection = connection(now)?;

    assert_eq!(connection.command(at(now), Command::Abort), Ok(()));
    assert!(matches!(
        connection.poll(at(now)),
        Output::Closed(CloseReason::Aborted)
    ));
    assert!(matches!(
        connection.poll(at(now)),
        Output::Idle { next_wakeup: None }
    ));
    assert_eq!(
        connection.command(at(now), Command::Abort),
        Err(CommandError::Closed)
    );
    assert_eq!(
        connection.receive(
            at(now),
            NetworkInput::Udp {
                local: SocketAddr::from(([192, 0, 2, 1], 5000)),
                remote: SocketAddr::from(([192, 0, 2, 2], 5001)),
                ecn: None,
                payload: Bytes::new(),
            },
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

    assert_eq!(
        connection.command(at(now), Command::CloseGracefully { deadline: now }),
        Ok(())
    );
    assert_eq!(
        connection.command(at(now), Command::CloseGracefully { deadline: now }),
        Ok(())
    );
    assert!(matches!(
        connection.poll(at(now)),
        Output::Closed(CloseReason::Graceful)
    ));
    assert!(matches!(
        connection.poll(at(now)),
        Output::Idle { next_wakeup: None }
    ));
    assert_eq!(
        connection.command(at(now), Command::CloseGracefully { deadline: now }),
        Err(CommandError::Closed)
    );
    Ok(())
}

#[test]
fn stats_are_owned_snapshots_and_include_negotiated_senders() -> Result<(), AcceptError> {
    let now = Instant::now();
    let connection = connection(now)?;
    let before = connection.stats();
    let mut owned = before.clone();
    owned.senders.clear();

    let after = connection.stats();
    assert_eq!(before, after);
    assert_eq!(after.connection.state, ConnectionState::Open);
    assert_eq!(after.connection.close_reason, None);
    assert!(after.connection.feedback.is_some());
    Ok(())
}
