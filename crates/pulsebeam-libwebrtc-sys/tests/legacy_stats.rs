use pulsebeam_webrtc_sys::*;
use std::{
    thread,
    time::{Duration, Instant},
};

fn take(peer: &PeerConnection, id: OperationId) -> PeerLegacyStatsSnapshot {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(event) = peer.try_next_event() {
            match event {
                PeerConnectionEvent::LegacyStats(value) if value.operation_id == id => {
                    return value;
                }
                PeerConnectionEvent::OperationComplete(value) if value.operation_id == id => {
                    panic!("unexpected terminal: {value:?}")
                }
                _ => {}
            }
        }
        assert!(Instant::now() < deadline, "legacy native callback timeout");
        thread::yield_now();
    }
}

#[test]
fn native_legacy_reports_keep_bytes_ids_and_admission_after_callback() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let mut peer = factory.create_peer_connection(Default::default()).unwrap();
    let label = b"legacy\xff\0";
    let channel = peer.create_data_channel_bytes(label, None).unwrap();
    let id = peer
        .request_legacy_stats(LegacyStatsOutputLevel::Debug)
        .unwrap();
    assert!(peer.request_stats().is_err());
    assert!(matches!(
        peer.request_legacy_stats(LegacyStatsOutputLevel::Standard),
        Err(LegacyStatsRequestError::Binding(PeerError {
            kind: PeerErrorKind::InvalidState,
            ..
        }))
    ));
    let snapshot = take(&peer, id);
    assert!(!snapshot.reports.is_empty());
    let report = snapshot
        .reports
        .iter()
        .find(|r| r.id.kind() == LegacyStatsType::DataChannel)
        .expect("actual legacy data channel report");
    let value = report.find(LegacyStatsName::Label).unwrap();
    match &value.value {
        LegacyStatsValueData::String(bytes) | LegacyStatsValueData::StaticString(bytes) => {
            assert_eq!(bytes, label)
        }
        other => panic!("label type {other:?}"),
    }
    assert_eq!(value.native_string, label);
    let alias = report.id.clone();
    assert!(alias.same_resource(&report.id));
    assert_eq!(alias, report.id);
    assert!(!report.native_type.is_empty());
    let second = peer
        .request_legacy_stats(LegacyStatsOutputLevel::Standard)
        .unwrap();
    let second = take(&peer, second);
    assert!(second.reports.iter().any(|r| r.id == alias));
    peer.close().unwrap();
    drop(channel);
    drop(peer);
    drop(factory);
    thread::spawn(move || {
        assert!(!snapshot.reports.is_empty());
        assert_eq!(alias.kind(), LegacyStatsType::DataChannel);
        assert!(!alias.to_native_string().is_empty());
    })
    .join()
    .unwrap();
}

#[test]
fn close_preserves_exactly_one_legacy_terminal_and_rejects_new_work() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let mut peer = factory.create_peer_connection(Default::default()).unwrap();
    let id = peer
        .request_legacy_stats(LegacyStatsOutputLevel::Standard)
        .unwrap();
    peer.close().unwrap();
    let mut terminals = 0;
    while let Some(event) = peer.try_next_event() {
        match event {
            PeerConnectionEvent::LegacyStats(value) if value.operation_id == id => terminals += 1,
            PeerConnectionEvent::OperationComplete(value) if value.operation_id == id => {
                terminals += 1;
                assert!(value.result.is_err());
            }
            _ => {}
        }
    }
    assert_eq!(terminals, 1);
    assert!(matches!(
        peer.request_legacy_stats(LegacyStatsOutputLevel::Standard),
        Err(LegacyStatsRequestError::Binding(PeerError {
            kind: PeerErrorKind::Closed,
            ..
        }))
    ));
}
