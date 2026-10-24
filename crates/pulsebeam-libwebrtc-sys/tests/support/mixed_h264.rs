use super::*;

const KEY: &[u8] = &[
    0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x0a, 0xd9, 0x1e, 0x84, 0, 0, 3, 0, 4, 0, 0, 3, 0, 0xf0, 0x3c,
    0x48, 0x99, 0x20, 0, 0, 0, 1, 0x68, 0xcb, 0x80, 0xc4, 0xb2, 0, 0, 0, 1, 0x65, 0x88, 0x84, 0xf1,
    0x18, 0xa0, 0, 0x20, 0x5b, 0x1c, 0, 4, 7, 0xe3, 0x80, 0, 0x80, 0xfe,
];
fn unit(world: &ControlledWorld, profile: H264SourceProfile, tag: u8) -> EncodedVideoAccessUnit {
    let mut data = KEY.to_vec();
    // An unregistered-user-data SEI, not a fabricated decoded-pixel witness.
    data.extend_from_slice(&[0, 0, 0, 1, 6, 5, 17]);
    data.extend_from_slice(&[tag; 16]);
    data.extend_from_slice(&[tag, 128]);
    EncodedVideoAccessUnit {
        data,
        width: if profile == H264SourceProfile::SimulcastL1T1 {
            320
        } else {
            16
        },
        height: if profile == H264SourceProfile::SimulcastL1T1 {
            180
        } else {
            16
        },
        timestamp_us: world.now().as_micros().try_into().unwrap(),
        key_frame: true,
        qp: None,
        metadata: EncodedVideoMetadata {
            codec: EncodedVideoCodec::H264 {
                base_layer_sync: false,
            },
            simulcast_index: None,
            spatial_index: None,
            temporal_index: (profile == H264SourceProfile::L1T3).then_some(0),
            end_of_picture: true,
        },
    }
}
fn contains_tag(frame: &EncodedReceivedVideoFrame, tag: u8) -> bool {
    frame.data.windows(16).any(|bytes| bytes == [tag; 16])
}
fn run_pair(profiles: [H264SourceProfile; 2], impaired: bool) -> Trace {
    eprintln!("CONTROLLED_MEDIA_BEGIN");
    let trace = run_pair_inner(profiles, impaired);
    eprintln!("CONTROLLED_MEDIA_END");
    trace
}
fn run_pair_inner(profiles: [H264SourceProfile; 2], impaired: bool) -> Trace {
    let world = ControlledWorld::acquire(1209, Duration::from_secs(10)).unwrap();
    let network = world.create_network().unwrap();
    let input = EncodedVideoInput::new_h264_profiles([64, 128, 255]).unwrap();
    let endpoints: Vec<_> = [Ipv4Addr::new(10, 83, 0, 1), Ipv4Addr::new(10, 83, 0, 2)]
        .into_iter()
        .map(|ip| network.register_endpoint(ip.into()).unwrap())
        .collect();
    let factories: Vec<_> = endpoints
        .iter()
        .map(|endpoint| {
            world
                .peer_factory_builder()
                .unwrap()
                .network_manager(endpoint.network_manager().unwrap())
                .packet_socket_factory(endpoint.packet_socket_factory().unwrap())
                .audio_encoder_factory(AudioEncoderFactory::with_opus_frames().unwrap())
                .audio_decoder_factory(AudioDecoderFactory::builtin_opus().unwrap())
                .video_encoder_factory(input.encoder_factory())
                .video_decoder_factory(input.encoded_receive_factory().unwrap())
                .build()
                .unwrap()
        })
        .collect();
    let mut peers = [
        factories[0]
            .create_peer_connection(PeerConfiguration::default())
            .unwrap(),
        factories[1]
            .create_peer_connection(PeerConfiguration::default())
            .unwrap(),
    ];
    let mut sources: Vec<_> = profiles
        .iter()
        .map(|&p| input.create_source_with_profile(&factories[0], p).unwrap())
        .collect();
    let senders: Vec<_> = sources
        .iter()
        .enumerate()
        .map(|(i, source)| {
            let track = source
                .create_track(&factories[0], &format!("mixed-{i}"))
                .unwrap();
            if profiles[i] == H264SourceProfile::SimulcastL1T1 {
                peers[0]
                    .add_video_transceiver_with_rids(
                        &track,
                        RtpTransceiverDirection::SendOnly,
                        &["q".into(), "h".into(), "f".into()],
                    )
                    .unwrap()
                    .sender()
            } else {
                peers[0]
                    .add_video_transceiver(&track, RtpTransceiverDirection::SendOnly)
                    .unwrap()
                    .sender()
            }
        })
        .collect();
    let mut observations = Observations {
        impaired,
        encoded_video_only: true,
        ..Observations::default()
    };
    observations.negotiate_with_layers(&world, &network, &peers, 0, true);
    for (i, sender) in senders.iter().enumerate() {
        if profiles[i] == H264SourceProfile::L1T3 {
            let mut parameters = sender.parameters().unwrap();
            parameters.encodings[0].scalability_mode = Some("L1T3".into());
            sender.set_parameters(parameters).unwrap();
        }
    }
    for round in 0..3 {
        if round > 0 {
            world.advance(Duration::from_millis(40)).unwrap();
        }
        let before = observations.trace.encoded_video.len();
        for i in 0..2 {
            if round > 0 && profiles[i] == H264SourceProfile::SimulcastL1T1 {
                continue;
            }
            push_h264(
                &sources[i],
                unit(&world, profiles[i], 0x51 + i as u8),
                profiles[i] == H264SourceProfile::SimulcastL1T1,
            );
        }
        let mut declined = [false; 2];
        let mut done = false;
        for tick in 0..2000 {
            // Continue capture under caller-driven progress. A lost bootstrap
            // keyframe cannot be recovered from a source that never pushes again.
            if tick > 0 && tick % 40 == 0 {
                for i in 0..2 {
                    if profiles[i] != H264SourceProfile::SimulcastL1T1 {
                        push_h264(
                            &sources[i],
                            unit(&world, profiles[i], 0x51 + i as u8),
                            false,
                        );
                    }
                }
            }
            observations.step(&world, &network, &peers, true);
            for i in 0..2 {
                if let Some(error) = sources[i].take_encoder_error() {
                    assert_eq!(profiles[i], H264SourceProfile::SimulcastL1T1);
                    assert_eq!(error, CodecError::InvalidConfiguration);
                    declined[i] = true;
                    observations
                        .trace
                        .events
                        .push(format!("source-{i}:native-collapsed-simulcast"));
                }
            }
            done = (0..2).all(|i| {
                if profiles[i] == H264SourceProfile::SimulcastL1T1 {
                    round > 0 || declined[i]
                } else {
                    observations.trace.encoded_video[before..]
                        .iter()
                        .any(|(_, frame)| contains_tag(frame, 0x51 + i as u8))
                }
            });
            if done {
                break;
            }
            world.advance(Duration::from_millis(1)).unwrap();
        }
        assert!(
            done,
            "mixed source progress exhausted: {profiles:?} impaired={impaired} round={round}; tail={:?}",
            &observations.trace.events[observations.trace.events.len().saturating_sub(30)..]
        );
        for i in 0..2 {
            if profiles[i] != H264SourceProfile::SimulcastL1T1 {
                let frame = &observations.trace.encoded_video[before..]
                    .iter()
                    .find(|(_, frame)| contains_tag(frame, 0x51 + i as u8))
                    .unwrap()
                    .1;
                // Negotiated DD describes even L1T1's base temporal layer.
                // Its native inference is not an input metadata echo.
                assert_eq!(frame.temporal_index, Some(0));
                assert!(sources[i].latest_rate_control().is_some());
            }
        }
    }
    // Replace one sender across profiles without rebuilding its peer/factory.
    let replacement_profile = if profiles[0] == H264SourceProfile::L1T3 {
        H264SourceProfile::L1T1
    } else {
        H264SourceProfile::L1T3
    };
    let mut replacement = input
        .create_source_with_profile(&factories[0], replacement_profile)
        .unwrap();
    let track = replacement
        .create_track(&factories[0], "mixed-replacement")
        .unwrap();
    senders[0].set_track(Some(&track)).unwrap();
    let mut parameters = senders[0].parameters().unwrap();
    parameters.encodings[0].scalability_mode =
        (replacement_profile == H264SourceProfile::L1T3).then(|| "L1T3".into());
    senders[0].set_parameters(parameters).unwrap();
    // Caller-driven reconfiguration progress, not an implicit SetTrack ack.
    for _ in 0..20 {
        observations.step(&world, &network, &peers, true);
        world.advance(Duration::from_millis(1)).unwrap();
    }
    sources[0].close().unwrap();
    world.advance(Duration::from_millis(40)).unwrap();
    let mut delivered = false;
    for step in 0..2000 {
        // Native source/mode reconfiguration may drop its first frame. Keep
        // publishing within the caller's explicit capture/progress budget.
        if step % 40 == 0 {
            replacement
                .push_encoded(unit(&world, replacement_profile, 0x71))
                .unwrap();
        }
        observations.step(&world, &network, &peers, true);
        assert!(replacement.take_encoder_error().is_none());
        delivered = observations
            .trace
            .encoded_video
            .iter()
            .any(|(_, frame)| contains_tag(frame, 0x71));
        if delivered {
            break;
        }
        world.advance(Duration::from_millis(1)).unwrap();
    }
    assert!(
        delivered,
        "cross-profile replacement {profiles:?} -> {replacement_profile:?} did not deliver; drops={}, rates={:?}, factory={:?}",
        replacement.dropped_frames(),
        replacement.latest_rate_control(),
        input
            .encoder_factory()
            .failure_observer()
            .unwrap()
            .failure()
    );
    replacement.close().unwrap();
    for source in &mut sources {
        source.close().unwrap();
    }
    for (_, sink) in &mut observations.encoded_video {
        sink.close();
    }
    observations.retired = true;
    for peer in &mut peers {
        peer.close().unwrap();
    }
    for _ in 0..20 {
        observations.step(&world, &network, &peers, false);
        world.advance(Duration::from_millis(1)).unwrap();
    }
    observations.trace
}
#[test]
fn distinct_h264_profile_pairs_keep_source_identity_and_replay() {
    let _guard = CONTROLLED_TEST.lock().unwrap_or_else(|p| p.into_inner());
    use H264SourceProfile::*;
    let mut traces = Vec::new();
    for profiles in [
        [L1T1, L1T3],
        [L1T1, SimulcastL1T1],
        [L1T3, SimulcastL1T1],
        [L1T3, L1T3],
    ] {
        for impaired in [false, true] {
            let first = run_pair(profiles, impaired);
            assert_eq!(first, run_pair(profiles, impaired));
            traces.push((format!("{profiles:?}"), impaired, format!("{first:?}")));
        }
    }
    if let Some(path) = std::env::var_os("PULSEBEAM_H264_TRACE_PATH") {
        // Full meaningful Trace, including bytes and metadata, outside lifecycles.
        std::fs::write(path, serde_json::to_vec(&traces).unwrap()).unwrap();
    }
}
