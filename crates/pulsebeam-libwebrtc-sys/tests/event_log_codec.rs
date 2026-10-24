use pulsebeam_webrtc_sys::*;
use std::collections::BTreeMap;

// Native Encode returns an outer tag/length followed by the payload expected
// by subtype Parse. This test-only framing reader does not implement a codec.
fn varint_read(bytes: &mut &[u8]) -> u64 {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let byte = bytes[0];
        *bytes = &bytes[1..];
        value |= u64::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return value;
        }
    }
    panic!("invalid native varint")
}
fn payload(mut bytes: &[u8]) -> (bool, &[u8]) {
    let tag = varint_read(&mut bytes);
    assert_eq!(varint_read(&mut bytes) as usize, bytes.len());
    (tag & 1 != 0, bytes)
}
fn varint(mut value: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let byte = (value & 127) as u8;
        value >>= 7;
        bytes.push(byte | if value == 0 { 0 } else { 128 });
        if value == 0 {
            return bytes;
        }
    }
}
fn event(data: RtcEventData, time: i64) -> RtcEvent {
    let mut event = RtcEvent::from_data(data).unwrap();
    event.set_timestamp_us(time).unwrap();
    event
}
fn time(us: i64) -> Timestamp {
    Timestamp::from_microseconds(us).unwrap()
}

#[test]
fn native_alr_batch_preserves_order_millisecond_precision_and_existing_prefix() {
    let a = event(RtcEventData::AlrState { in_alr: true }, 1_234_567);
    let b = event(RtcEventData::AlrState { in_alr: false }, -2_000_123);
    let encoded = RtcEvent::encode_batch(RtcEventKind::AlrState, &[&a, &b]).unwrap();
    let (batched, data) = payload(&encoded);
    assert!(batched);
    let prefix = LoggedAlrStateEvent {
        timestamp: time(8),
        in_alr: false,
    };
    let mut output = RtcEventParsedRecords::AlrState(vec![prefix.clone()]);
    assert!(output.parse_payload(data, batched).unwrap().ok());
    assert_eq!(
        output,
        RtcEventParsedRecords::AlrState(vec![
            prefix,
            LoggedAlrStateEvent {
                timestamp: time(1_234_000),
                in_alr: true
            },
            LoggedAlrStateEvent {
                timestamp: time(-2_000_000),
                in_alr: false
            }
        ])
    );
    assert_eq!(a.timestamp_us(), 1_234_567);
    assert!(
        RtcEvent::encode_batch(RtcEventKind::AlrState, &[])
            .unwrap()
            .is_empty()
    );
    let other = event(RtcEventData::AudioPlayout { ssrc: 8 }, 0);
    assert_eq!(
        RtcEvent::encode_batch(RtcEventKind::AlrState, &[&a, &other]),
        Err(RtcEventCodecError::MismatchedEventKind)
    );
}

#[test]
fn native_audio_map_and_bwe_fields_are_owning_and_append_on_native_failure() {
    let a = event(RtcEventData::AudioPlayout { ssrc: 9 }, 1000);
    let b = event(RtcEventData::AudioPlayout { ssrc: 3 }, 2000);
    let c = event(RtcEventData::AudioPlayout { ssrc: 9 }, 3000);
    let encoded = RtcEvent::encode_batch(RtcEventKind::AudioPlayout, &[&a, &b, &c]).unwrap();
    let (batched, data) = payload(&encoded);
    let prefix = LoggedAudioPlayoutEvent {
        timestamp: time(0),
        ssrc: 99,
    };
    let mut output =
        RtcEventParsedRecords::AudioPlayout(BTreeMap::from([(3, vec![prefix.clone()])]));
    assert!(output.parse_payload(data, batched).unwrap().ok());
    let RtcEventParsedRecords::AudioPlayout(map) = output else {
        panic!()
    };
    assert_eq!(
        map[&3],
        vec![
            prefix,
            LoggedAudioPlayoutEvent {
                timestamp: time(2000),
                ssrc: 3
            }
        ]
    );
    assert_eq!(
        map[&9]
            .iter()
            .map(|row| row.log_time_us().unwrap())
            .collect::<Vec<_>>(),
        vec![1000, 3000]
    );
    let bwe = event(
        RtcEventData::BweUpdateDelayBased {
            bitrate_bps: -17,
            detector_state: BandwidthUsage::Overusing,
        },
        4000,
    );
    let encoded = RtcEvent::encode_batch(RtcEventKind::BweUpdateDelayBased, &[&bwe]).unwrap();
    let (batched, data) = payload(&encoded);
    let mut output = RtcEventParsedRecords::for_kind(RtcEventKind::BweUpdateDelayBased).unwrap();
    assert!(output.parse_payload(data, batched).unwrap().ok());
    assert_eq!(
        output,
        RtcEventParsedRecords::BweUpdateDelayBased(vec![LoggedBweDelayBasedUpdate {
            timestamp: time(4000),
            bitrate_bps: -17,
            detector_state: BandwidthUsage::Overusing
        }])
    );
    let mut invalid = data.to_vec();
    assert_eq!(*invalid.last().unwrap(), 2);
    *invalid.last_mut().unwrap() = 3;
    let status = output.parse_payload(&invalid, batched).unwrap();
    assert!(!status.ok());
    assert!(status.message().contains("BandwidthUsage"));
    let RtcEventParsedRecords::BweUpdateDelayBased(rows) = output else {
        panic!()
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].bitrate_bps, -17);
    assert_eq!(rows[1].detector_state, BandwidthUsage::Normal);
}

#[test]
fn begin_end_and_partial_native_records_preserve_utc_and_infinity() {
    let begin = RtcEvent::begin_log(1_234_567, 8_765_432).unwrap();
    let encoded = RtcEvent::encode_batch(RtcEventKind::BeginLog, &[&begin]).unwrap();
    let (batched, data) = payload(&encoded);
    let mut output = RtcEventParsedRecords::for_kind(RtcEventKind::BeginLog).unwrap();
    assert!(output.parse_payload(data, batched).unwrap().ok());
    let RtcEventParsedRecords::BeginLog(rows) = &output else {
        panic!()
    };
    assert_eq!(rows[0].log_time_ms().unwrap(), 1234);
    assert_eq!(rows[0].utc_time(), time(8_765_000));
    let status = output.parse_payload(&[], false).unwrap();
    assert!(!status.ok());
    assert!(!status.message().is_empty());
    let RtcEventParsedRecords::BeginLog(rows) = output else {
        panic!()
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].timestamp, Timestamp::plus_infinity());
    assert_eq!(rows[1].utc_time(), Timestamp::plus_infinity());
    assert_eq!(rows[1].log_time_us(), Err(TimeValueError::InfiniteValue));
    // EndLog's native encoder only reads common timestamps, not subtype fields.
    let encoded = RtcEvent::encode_batch(RtcEventKind::EndLog, &[&begin]).unwrap();
    let (batched, data) = payload(&encoded);
    let mut end = RtcEventParsedRecords::for_kind(RtcEventKind::EndLog).unwrap();
    assert!(end.parse_payload(data, batched).unwrap().ok());
    assert_eq!(
        end,
        RtcEventParsedRecords::EndLog(vec![LoggedStopEvent {
            timestamp: time(1_234_000)
        }])
    );
    // The native parser accepts an explicitly zero-count empty batch.
    assert!(end.parse_payload(&[0], true).unwrap().ok());
    let mut alr = RtcEventParsedRecords::for_kind(RtcEventKind::AlrState).unwrap();
    assert!(!alr.parse_payload(&[1], false).unwrap().ok());
    assert_eq!(
        alr,
        RtcEventParsedRecords::AlrState(vec![LoggedAlrStateEvent {
            timestamp: time(1000),
            in_alr: false
        }])
    );
    let mut audio = RtcEventParsedRecords::for_kind(RtcEventKind::AudioPlayout).unwrap();
    assert!(!audio.parse_payload(&[1], false).unwrap().ok());
    assert_eq!(
        audio,
        RtcEventParsedRecords::AudioPlayout(BTreeMap::from([(
            0,
            vec![LoggedAudioPlayoutEvent {
                timestamp: time(1000),
                ssrc: 0
            }]
        )]))
    );
}

#[test]
fn unsafe_timestamp_scaling_is_rejected_without_fabricating_native_status_or_output() {
    let mut end = RtcEventParsedRecords::EndLog(vec![LoggedStopEvent { timestamp: time(1) }]);
    let before = end.clone();
    assert_eq!(
        end.parse_payload(&varint(i64::MAX as u64), false),
        Err(RtcEventCodecError::TimestampOutOfRange)
    );
    assert_eq!(end, before);
    let begin = RtcEvent::begin_log(0, 0).unwrap();
    let encoded = RtcEvent::encode_batch(RtcEventKind::BeginLog, &[&begin]).unwrap();
    let (_, data) = payload(&encoded);
    let mut malicious = data.to_vec();
    assert_eq!(malicious.pop(), Some(0));
    malicious.extend(varint(i64::MAX as u64));
    let mut output = RtcEventParsedRecords::for_kind(RtcEventKind::BeginLog).unwrap();
    assert_eq!(
        output.parse_payload(&malicious, false),
        Err(RtcEventCodecError::TimestampOutOfRange)
    );
    assert_eq!(output, RtcEventParsedRecords::BeginLog(Vec::new()));
}

#[test]
fn every_native_stub_reports_its_real_not_implemented_status_and_empty_encoding() {
    let kinds = [
        RtcEventKind::RouteChange,
        RtcEventKind::RemoteEstimate,
        RtcEventKind::AudioNetworkAdaptation,
        RtcEventKind::AudioReceiveStreamConfig,
        RtcEventKind::AudioSendStreamConfig,
        RtcEventKind::BweUpdateLossBased,
        RtcEventKind::BweUpdateScream,
        RtcEventKind::DtlsTransportState,
        RtcEventKind::DtlsWritableState,
        RtcEventKind::IceCandidatePairConfig,
        RtcEventKind::IceCandidatePair,
        RtcEventKind::ProbeClusterCreated,
        RtcEventKind::ProbeResultFailure,
        RtcEventKind::ProbeResultSuccess,
        RtcEventKind::RtcpPacketIncoming,
        RtcEventKind::RtcpPacketOutgoing,
        RtcEventKind::RtpPacketIncoming,
        RtcEventKind::RtpPacketOutgoing,
        RtcEventKind::VideoReceiveStreamConfig,
        RtcEventKind::VideoSendStreamConfig,
        RtcEventKind::FrameDecoded,
    ];
    let sample = RtcEvent::begin_log(0, 0).unwrap();
    for kind in kinds {
        assert!(RtcEvent::encode_batch(kind, &[&sample]).unwrap().is_empty());
        let mut output = RtcEventParsedRecords::for_kind(kind).unwrap();
        let status = output.parse_payload(b"arbitrary\0bytes", true).unwrap();
        assert!(!status.ok());
        assert!(status.message().contains("Not Implemented"));
        assert_eq!(output, RtcEventParsedRecords::NativeUnimplemented(kind));
    }
    assert!(RtcEventLogParseStatus::success().ok());
    assert_eq!(
        RtcEventLogParseStatus::error("bad", "caller", 7).message(),
        "bad (caller: 7)"
    );
    assert_eq!(
        RtcEvent::encode_batch(RtcEventKind::NetEqSetMinimumDelay, &[]),
        Err(RtcEventCodecError::UnsupportedEventKind)
    );
    assert!(matches!(
        RtcEventParsedRecords::for_kind(RtcEventKind::Unknown(99)),
        Err(RtcEventCodecError::UnsupportedEventKind)
    ));
    assert_eq!(
        RtcEventParsedRecords::NativeUnimplemented(RtcEventKind::AlrState)
            .parse_payload(&[], false),
        Err(RtcEventCodecError::MismatchedOutputKind)
    );
}
