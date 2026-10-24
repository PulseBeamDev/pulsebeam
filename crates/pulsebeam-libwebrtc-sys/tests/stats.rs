use pulsebeam_webrtc_sys::*;
use std::collections::BTreeMap;

#[test]
fn attributes_preserve_every_native_optional_variant_and_full_width_values() {
    let values = vec![
        RtcStatsValue::Bool(Some(false)),
        RtcStatsValue::Int32(Some(i32::MIN)),
        RtcStatsValue::Uint32(Some(u32::MAX)),
        RtcStatsValue::Int64(Some(i64::MIN)),
        RtcStatsValue::Uint64(Some(u64::MAX)),
        RtcStatsValue::Double(Some(-0.25)),
        RtcStatsValue::String(Some(vec![0xff, 0, 0x80])),
        RtcStatsValue::Bools(Some(vec![false, true])),
        RtcStatsValue::Int32s(Some(vec![i32::MIN, i32::MAX])),
        RtcStatsValue::Uint32s(Some(vec![u32::MAX])),
        RtcStatsValue::Int64s(Some(vec![i64::MIN, i64::MAX])),
        RtcStatsValue::Uint64s(Some(vec![u64::MAX])),
        RtcStatsValue::Doubles(Some(vec![0.5, -1.25])),
        RtcStatsValue::Strings(Some(vec![vec![], vec![0xff, 0]])),
        RtcStatsValue::Uint64Map(Some(BTreeMap::from([(vec![0xff, 0], u64::MAX)]))),
        RtcStatsValue::DoubleMap(Some(BTreeMap::from([(vec![0x80, 0], -0.125)]))),
    ];
    let undefined = vec![
        RtcStatsValue::Bool(None),
        RtcStatsValue::Int32(None),
        RtcStatsValue::Uint32(None),
        RtcStatsValue::Int64(None),
        RtcStatsValue::Uint64(None),
        RtcStatsValue::Double(None),
        RtcStatsValue::String(None),
        RtcStatsValue::Bools(None),
        RtcStatsValue::Int32s(None),
        RtcStatsValue::Uint32s(None),
        RtcStatsValue::Int64s(None),
        RtcStatsValue::Uint64s(None),
        RtcStatsValue::Doubles(None),
        RtcStatsValue::Strings(None),
        RtcStatsValue::Uint64Map(None),
        RtcStatsValue::DoubleMap(None),
    ];
    for (index, (value, undefined)) in values.into_iter().zip(undefined).enumerate() {
        let attr = RtcStatsAttribute::new(c"metric", value.clone());
        assert_eq!(attr.name(), b"metric");
        assert_eq!(attr.value(), value);
        assert!(attr.has_value());
        assert_eq!(attr.is_sequence(), (7..14).contains(&index));
        assert_eq!(attr.is_string(), index == 6);
        assert_eq!(attr, attr.clone());
        let missing = RtcStatsAttribute::new(c"metric", undefined.clone());
        assert_eq!(missing.value(), undefined);
        assert!(!missing.has_value());
        assert_ne!(attr, missing);
        assert!(!attr.to_native_string().is_empty());
    }
    let empty = RtcStatsAttribute::new(c"empty", RtcStatsValue::Strings(Some(vec![])));
    assert_eq!(empty.value(), RtcStatsValue::Strings(Some(vec![])));
    assert!(empty.has_value());
}

#[test]
fn native_records_report_transfer_copy_filters_and_base_values_are_owned() {
    let time = Timestamp::from_microseconds(17).unwrap();
    let mut record = RtcStats::new(RtcStatsClass::DataChannelStats, b"opaque\xff\0", time).unwrap();
    let RtcStatsValues::DataChannelStats(mut values) = record.values() else {
        panic!()
    };
    assert_eq!(values.bytes_sent, None);
    values.label = Some(b"label\xff\0".to_vec());
    values.protocol = Some(b"proto\x80\0".to_vec());
    values.messages_sent = Some(u32::MAX);
    values.bytes_sent = Some(u64::MAX);
    assert!(record.set_values(RtcStatsValues::DataChannelStats(values.clone())));
    assert_eq!(record.values(), RtcStatsValues::DataChannelStats(values));
    let mut clone = record.clone();
    clone.set_timestamp(Timestamp::from_microseconds(18).unwrap());
    assert_eq!(clone, record, "native record equality ignores timestamp");
    assert!(!clone.set_values(RtcStatsValues::CodecStats(Default::default())));
    assert_eq!(clone.values(), record.values());
    assert_eq!(
        record
            .attributes()
            .iter()
            .find(|v| v.name() == b"bytesSent")
            .unwrap()
            .value(),
        RtcStatsValue::Uint64(Some(u64::MAX))
    );
    let mut builder = RtcStatsReportBuilder::new(time);
    assert!(builder.add(record));
    assert!(!builder.try_add(clone));
    let audio = RtcStats::new(RtcStatsClass::AudioSourceStats, b"audio", time).unwrap();
    let video = RtcStats::new(RtcStatsClass::VideoSourceStats, b"video", time).unwrap();
    assert_eq!(
        audio.native_type(),
        video.native_type(),
        "the SDK uses a shared type string"
    );
    let base = audio.copy_as(RtcStatsClass::MediaSourceStats).unwrap();
    assert_eq!(base.class(), RtcStatsClass::MediaSourceStats);
    assert!(base.copy_as(RtcStatsClass::AudioSourceStats).is_none());
    assert!(builder.add(audio));
    assert!(builder.add(video));
    let report = builder.freeze();
    let alias = report.clone();
    assert!(report.same_resource(&alias));
    assert_eq!(report.timestamp(), time);
    assert_eq!(report.len(), 3);
    assert!(
        report
            .get_as(b"video", RtcStatsClass::AudioSourceStats)
            .is_none()
    );
    assert_eq!(report.of_class(RtcStatsClass::AudioSourceStats).len(), 1);
    assert_eq!(report.of_class(RtcStatsClass::VideoSourceStats).len(), 1);
    assert_eq!(
        report.iter().map(|v| v.id()).collect::<Vec<_>>(),
        vec![
            b"audio".to_vec(),
            b"opaque\xff\0".to_vec(),
            b"video".to_vec()
        ]
    );
    let mut copy = report.copy_builder();
    let taken = copy.take(b"opaque\xff\0").unwrap();
    assert_eq!(taken.id(), b"opaque\xff\0");
    assert_eq!(copy.len(), 2);
    assert_eq!(report.len(), 3);
    let mut other = RtcStatsReportBuilder::new(time);
    assert!(other.add(base));
    assert!(!copy.take_members_from(&mut other));
    assert_eq!(other.len(), 1);
    copy.take(b"audio").unwrap();
    assert!(copy.take_members_from(&mut other));
    assert!(other.is_empty());
    drop(report);
    drop(alias);
    std::thread::spawn(move || {
        assert_eq!(copy.freeze().len(), 2);
        assert_eq!(taken.timestamp(), time);
    })
    .join()
    .unwrap();
}

#[test]
fn actual_peer_report_is_not_truncated_at_the_old_projection_limit() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let mut peer = factory.create_peer_connection(Default::default()).unwrap();
    let channels: Vec<_> = (0..300)
        .map(|i| {
            peer.create_data_channel(&format!("native-{i}"), Default::default())
                .unwrap()
        })
        .collect();
    let id = peer.request_stats().unwrap();
    assert!(
        peer.request_stats().is_err(),
        "unconsumed report keeps admission"
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let report = loop {
        if let Some(event) = peer.try_next_event() {
            match event {
                PeerConnectionEvent::Stats(value) if value.operation_id == id => {
                    break value.report;
                }
                PeerConnectionEvent::OperationComplete(value) if value.operation_id == id => {
                    panic!("stats was not a native report: {value:?}")
                }
                _ => {}
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "native report timeout"
        );
        std::thread::yield_now();
    };
    assert_eq!(report.of_class(RtcStatsClass::DataChannelStats).len(), 300);
    assert!(report.len() >= 300);
    peer.clear_stats_cache();
    peer.close().unwrap();
    drop(channels);
    drop(peer);
    drop(factory);
    std::thread::spawn(move || {
        assert_eq!(report.of_class(RtcStatsClass::DataChannelStats).len(), 300)
    })
    .join()
    .unwrap();
}

#[test]
fn native_timestamp_infinities_and_serialization_preconditions_are_preserved() {
    let mut value = RtcStats::new(
        RtcStatsClass::CodecStats,
        b"codec",
        Timestamp::minus_infinity(),
    )
    .unwrap();
    assert_eq!(value.timestamp(), Timestamp::minus_infinity());
    assert_eq!(value.to_native_json(), Err(TimeValueError::InfiniteValue));
    value.set_timestamp(Timestamp::plus_infinity());
    assert_eq!(value.timestamp(), Timestamp::plus_infinity());
    value.set_timestamp(Timestamp::zero());
    let json = value.to_native_json().unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(parsed["id"], "codec");
    let mut report = RtcStatsReportBuilder::new(Timestamp::plus_infinity());
    assert!(report.add(value));
    let report = report.freeze();
    assert_eq!(report.timestamp(), Timestamp::plus_infinity());
    assert!(report.to_native_json().is_ok());
    fn send_sync<T: Send + Sync>() {}
    send_sync::<RtcStatsReport>();
    send_sync::<RtcStats>();
    send_sync::<RtcStatsAttribute>();
}
