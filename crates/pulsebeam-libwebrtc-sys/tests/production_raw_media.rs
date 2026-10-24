use pulsebeam_webrtc_sys::*;
use std::{
    collections::HashMap,
    thread,
    time::{Duration, Instant},
};

#[derive(Default)]
struct Call {
    operations: HashMap<(SessionPeerId, OperationId), Option<SessionDescription>>,
    gathered: [bool; 2],
    connected: [bool; 2],
    audio: Option<SessionReceiverId>,
    audio_arrivals: Vec<SessionReceiverId>,
    all_audio: Vec<(usize, SessionReceiverId, SessionTransceiverId)>,
    video: Option<SessionReceiverId>,
}
impl Call {
    fn drain(&mut self, session: &mut ProductionSession, peers: [SessionPeerId; 2]) {
        for (index, peer) in peers.into_iter().enumerate() {
            while let Some(event) = session.try_peer_event(peer).unwrap() {
                match event {
                    SessionEvent::OperationComplete(done) => {
                        self.operations
                            .insert((peer, done.operation_id), done.result.unwrap());
                    }
                    SessionEvent::Track {
                        receiver,
                        transceiver,
                    } => match session.receiver_media_type(receiver).unwrap() {
                        MediaType::Audio => {
                            self.all_audio.push((index, receiver, transceiver));
                            if index == 1 {
                                self.audio = Some(receiver);
                                self.audio_arrivals.push(receiver);
                            }
                        }
                        MediaType::Video if index == 1 => self.video = Some(receiver),
                        MediaType::Video => {}
                        kind => panic!("unexpected track kind: {kind:?}"),
                    },
                    SessionEvent::IceGatheringStateChanged(IceGatheringState::Complete) => {
                        self.gathered[index] = true
                    }
                    SessionEvent::ConnectionStateChanged(ConnectionState::Connected) => {
                        self.connected[index] = true
                    }
                    _ => {}
                }
            }
        }
    }
    fn finish(
        &mut self,
        session: &mut ProductionSession,
        peers: [SessionPeerId; 2],
        peer: SessionPeerId,
        operation: OperationId,
    ) -> Option<SessionDescription> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            self.drain(session, peers);
            if let Some(result) = self.operations.remove(&(peer, operation)) {
                return result;
            }
            assert!(
                Instant::now() < deadline,
                "native operation did not complete"
            );
            thread::yield_now();
        }
    }
    fn gather(
        &mut self,
        session: &mut ProductionSession,
        peers: [SessionPeerId; 2],
        index: usize,
        description: SessionDescription,
    ) -> SessionDescription {
        let operation = session
            .set_local_description(peers[index], description)
            .unwrap();
        self.finish(session, peers, peers[index], operation);
        let deadline = Instant::now() + Duration::from_secs(15);
        while !self.gathered[index] {
            self.drain(session, peers);
            assert!(Instant::now() < deadline, "ICE gathering did not complete");
            thread::yield_now();
        }
        let descriptions = session.descriptions(peers[index]).unwrap();
        descriptions
            .current_local
            .or(descriptions.pending_local)
            .unwrap()
    }
    fn negotiate(&mut self, session: &mut ProductionSession, peers: [SessionPeerId; 2]) {
        let _ = self.negotiate_answer_source(session, peers, None);
    }
    fn negotiate_answer_source(
        &mut self,
        session: &mut ProductionSession,
        peers: [SessionPeerId; 2],
        answer_source: Option<SessionSourceId>,
    ) -> Option<SessionSenderId> {
        let operation = session.create_offer(peers[0], false).unwrap();
        let offer = self.finish(session, peers, peers[0], operation).unwrap();
        let offer = self.gather(session, peers, 0, offer);
        let operation = session.set_remote_description(peers[1], offer).unwrap();
        self.finish(session, peers, peers[1], operation);
        let answer_sender = answer_source.map(|source| {
            let transceiver = self
                .all_audio
                .iter()
                .find(|(index, _, _)| *index == 1)
                .unwrap()
                .2;
            let sender = session.sender(transceiver).unwrap();
            session
                .replace_audio_source(sender, Some(source), "tone-1")
                .unwrap();
            session
                .set_direction(transceiver, RtpTransceiverDirection::SendReceive)
                .unwrap();
            sender
        });
        let operation = session.create_answer(peers[1]).unwrap();
        let answer = self.finish(session, peers, peers[1], operation).unwrap();
        let answer = self.gather(session, peers, 1, answer);
        let operation = session.set_remote_description(peers[0], answer).unwrap();
        self.finish(session, peers, peers[0], operation);
        let deadline = Instant::now() + Duration::from_secs(15);
        while self.connected != [true; 2] {
            self.drain(session, peers);
            assert!(Instant::now() < deadline, "native peers did not connect");
            thread::yield_now();
        }
        answer_sender
    }
}

fn drive_pcm_tones(
    session: &mut ProductionSession,
    call: &mut Call,
    peers: [SessionPeerId; 2],
    sources: [SessionSourceId; 2],
) -> Vec<DecodedAudioFrame> {
    let tones = [1000, 1500].map(|frequency| {
        let samples = (0..480)
            .map(|index| {
                if index * frequency * 2 / 48000 % 2 == 0 {
                    12000
                } else {
                    -12000
                }
            })
            .collect();
        AudioPcmFrame::i16_interleaved(48000, 1, 0, samples).unwrap()
    });
    let periods = [48, 32];
    let mut received = [false; 2];
    let mut frames = Vec::new();
    let start = Instant::now();
    let mut next = Duration::ZERO;
    while received != [true; 2] {
        let elapsed = start.elapsed();
        if elapsed >= next {
            for index in 0..2 {
                let capture = session.pcm_capture_time_now(sources[index]).unwrap();
                session
                    .push_pcm_at(sources[index], &tones[index], capture)
                    .unwrap();
            }
            next = elapsed + Duration::from_millis(10);
        }
        call.drain(session, peers);
        for &(peer_index, receiver, _) in &call.all_audio {
            if let Some(frame) = session.try_pcm_frame(receiver).unwrap() {
                let correlation = |lag: usize| {
                    frame
                        .samples
                        .iter()
                        .zip(frame.samples.iter().skip(lag))
                        .fold((0i64, 0i64), |(dot, energy), (&a, &b)| {
                            (
                                dot + i64::from(a) * i64::from(b),
                                energy + i64::from(a) * i64::from(a),
                            )
                        })
                };
                let period = periods[1 - peer_index];
                let (dot, energy) = correlation(period);
                let (opposite, half_energy) = correlation(period / 2);
                if energy > 1_000_000 && dot > energy / 2 && opposite < -half_energy / 2 {
                    let typed = frame.interleaved_view().unwrap();
                    assert_eq!(
                        (typed.num_channels(), typed.samples_per_channel()),
                        (1, 480)
                    );
                    assert_eq!(&*typed, &frame.samples);
                    assert_eq!(
                        (
                            frame.sample_rate_hz,
                            frame.channels,
                            frame.samples_per_channel
                        ),
                        (48000, 1, 480)
                    );
                    received[peer_index] = true;
                    frames.push(frame);
                }
            }
        }
        assert!(
            elapsed < Duration::from_secs(15),
            "bidirectional PCM missing: {received:?}"
        );
        thread::yield_now();
    }
    frames
}

#[test]
fn source_options_reconfigure_active_bidirectional_pcm_without_false_volume_gain() {
    let encoder = AudioEncoderFactory::builtin().unwrap();
    let decoder = AudioDecoderFactory::builtin_opus().unwrap();
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .audio_encoder_factory(&encoder)
        .audio_decoder_factory(&decoder)
        .build()
        .unwrap();
    let peers = [
        session.create_peer(PeerConfiguration::default()).unwrap(),
        session.create_peer(PeerConfiguration::default()).unwrap(),
    ];
    let sources = [
        session.create_pcm_source().unwrap(),
        session.create_pcm_source().unwrap(),
    ];
    let transceiver = session
        .publish_pcm(
            peers[0],
            sources[0],
            "tone-0",
            RtpTransceiverDirection::SendReceive,
            None,
        )
        .unwrap();
    let first_sender = session.sender(transceiver).unwrap();
    let (session, call, warm, senders) = thread::spawn(move || {
        let mut session = session;
        let mut call = Call::default();
        let answer_sender = call
            .negotiate_answer_source(&mut session, peers, Some(sources[1]))
            .unwrap();
        assert_eq!(call.all_audio.len(), 2);
        for &(_, receiver, _) in &call.all_audio {
            session.attach_pcm(receiver).unwrap();
        }
        let warm = drive_pcm_tones(&mut session, &mut call, peers, sources);
        (session, call, warm, [first_sender, answer_sender])
    })
    .join()
    .unwrap();
    let (session, later) = thread::spawn(move || {
        let mut session = session;
        let mut call = call;
        let options = AudioSourceOptions {
            // Source options reach only the send channel, not NetEq. Preserve
            // the full value rather than pretending to set receive capacity.
            audio_jitter_buffer_max_packets: Some(i32::MAX),
            audio_jitter_buffer_fast_accelerate: Some(false),
            audio_jitter_buffer_min_delay_ms: Some(0),
            stereo_swapping: Some(false),
            audio_network_adaptor: Some(false),
            audio_network_adaptor_config: Some(vec![0, 0xff, 0]),
            init_recording_on_send: Some(false),
            ..AudioSourceOptions::default()
        };
        for index in 0..2 {
            session
                .set_audio_source_options(sources[index], &options)
                .unwrap();
            assert_eq!(
                session.audio_source_options(sources[index]).unwrap(),
                options
            );
            let mut rejected = options.clone();
            rejected.noise_suppression_mode = Some(AudioProcessingMode::Unknown(99));
            rejected.stereo_swapping = Some(true);
            assert!(
                session
                    .set_audio_source_options(sources[index], &rejected)
                    .is_err()
            );
            assert_eq!(
                session.audio_source_options(sources[index]).unwrap(),
                options
            );
            assert!(
                session
                    .set_audio_source_volume(sources[index], f64::NAN)
                    .is_err()
            );
            session
                .set_audio_source_volume(sources[index], 0.0)
                .unwrap();
            session.set_audio_track_volume(senders[index], 0.0).unwrap();
            assert_eq!(session.audio_source_volume(sources[index]).unwrap(), 1.0);
            assert_eq!(
                session
                    .audio_track_status(senders[index])
                    .unwrap()
                    .unwrap()
                    .volume,
                1.0
            );
            // Native source notification alone does not notify an unchanged
            // live track. Reattach through SetTrack so SetSend genuinely reads
            // the new options on the actual send channel.
            if index == 0 {
                session
                    .replace_audio_source(senders[index], Some(sources[index]), "retuned-0")
                    .unwrap();
            } else {
                let stream = session.create_media_stream("retuning").unwrap();
                let original = session
                    .create_source_track(sources[index], "retuned-1")
                    .unwrap();
                assert!(
                    session
                        .local_add_media_stream_track(stream, original)
                        .unwrap()
                );
                let alias = session
                    .find_media_stream_audio_track(stream, "retuned-1")
                    .unwrap()
                    .unwrap();
                session.release_media_stream_track(original);
                session.local_release_media_stream(stream);
                session
                    .replace_sender_track(senders[index], Some(alias))
                    .unwrap();
                let installed = session.sender_track(senders[index]).unwrap().unwrap();
                assert!(session.media_stream_tracks_same(alias, installed).unwrap());
                session.release_media_stream_track(alias);
                session.release_media_stream_track(installed);
            }
        }
        let later = drive_pcm_tones(&mut session, &mut call, peers, sources);
        for source in sources {
            session
                .set_audio_source_options(source, &AudioSourceOptions::default())
                .unwrap();
            assert_eq!(
                session.audio_source_options(source).unwrap(),
                AudioSourceOptions::default()
            );
            session
                .push_pcm(
                    source,
                    &AudioPcmFrame::i16_interleaved(48000, 1, 0, vec![99; 480]).unwrap(),
                )
                .unwrap();
        }
        (session, later)
    })
    .join()
    .unwrap();
    thread::spawn(move || {
        let mut session = session;
        session.shutdown().unwrap();
        drop(session);
        assert!(!warm.is_empty() && !later.is_empty());
        assert!(
            warm.iter()
                .chain(&later)
                .all(|frame| frame.samples.iter().any(|&sample| sample != 0))
        );
    })
    .join()
    .unwrap();
}

#[test]
fn remote_audio_views_and_observers_survive_identifier_retirement_and_migration() {
    let encoder = AudioEncoderFactory::builtin().unwrap();
    let decoder = AudioDecoderFactory::builtin_opus().unwrap();
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .audio_encoder_factory(&encoder)
        .audio_decoder_factory(&decoder)
        .build()
        .unwrap();
    let peers = [
        session.create_peer(PeerConfiguration::default()).unwrap(),
        session.create_peer(PeerConfiguration::default()).unwrap(),
    ];
    let sources = [
        session.create_pcm_source().unwrap(),
        session.create_pcm_source().unwrap(),
    ];
    let transceiver = session
        .publish_pcm(
            peers[0],
            sources[0],
            "observed-0",
            RtpTransceiverDirection::SendReceive,
            None,
        )
        .unwrap();
    let sender = session.sender(transceiver).unwrap();
    let (session, mut call, retained, warm) = thread::spawn(move || {
        let mut session = session;
        let mut call = Call::default();
        call.negotiate_answer_source(&mut session, peers, Some(sources[1]))
            .unwrap();
        for &(_, receiver, _) in &call.all_audio {
            session.attach_pcm(receiver).unwrap();
        }
        let warm = drive_pcm_tones(&mut session, &mut call, peers, sources);
        let receiver = call.all_audio[0].1;
        // The pinned stock tracks do not install a per-track AudioProcessor,
        // even while the voice engine is actively processing this real call.
        assert_eq!(session.sender_audio_processor(sender).unwrap(), None);
        assert_eq!(session.receiver_audio_processor(receiver).unwrap(), None);
        let remote = session
            .receiver_audio_source_view(receiver)
            .unwrap()
            .unwrap();
        let alias = session
            .receiver_audio_source_view(receiver)
            .unwrap()
            .unwrap();
        assert!(session.audio_source_views_same(remote, alias).unwrap());
        assert!(session.audio_source_view_is_remote(remote).unwrap());
        let local = session.audio_source_view(sources[0]).unwrap();
        let installed = session.sender_audio_source_view(sender).unwrap().unwrap();
        assert!(session.audio_source_views_same(local, installed).unwrap());
        assert!(!session.audio_source_views_same(local, remote).unwrap());
        let stream = session
            .create_media_stream("active-received-audio")
            .unwrap();
        let stream_observer = session
            .observe_media_stream(stream, std::num::NonZeroUsize::new(2).unwrap())
            .unwrap();
        let original_track = session.receiver_track(receiver).unwrap().unwrap();
        assert!(
            session
                .local_add_media_stream_track(stream, original_track)
                .unwrap()
        );
        let retained_track = session.local_stream_audio_tracks(stream).unwrap()[0];
        assert!(
            session
                .media_stream_tracks_same(original_track, retained_track)
                .unwrap()
        );
        session.release_media_stream_track(original_track);
        let cap = std::num::NonZeroUsize::new(2).unwrap();
        let volume = session.observe_audio_source_volume(remote, cap).unwrap();
        let state = session.observe_audio_source_changes(remote, cap).unwrap();
        let track = session.observe_receiver_audio_track(receiver, cap).unwrap();
        let producer = session.observe_audio_source_changes(local, cap).unwrap();
        session.release_audio_source_view(local);
        session.release_audio_source_view(remote);
        (
            session,
            call,
            (
                alias,
                installed,
                volume,
                state,
                track,
                producer,
                receiver,
                stream,
                stream_observer,
                retained_track,
            ),
            warm,
        )
    })
    .join()
    .unwrap();
    let (session, later) = thread::spawn(move || {
        let mut session = session;
        let (
            alias,
            installed,
            volume,
            _,
            track,
            producer,
            receiver,
            stream,
            stream_observer,
            retained_track,
        ) = retained;
        let fresh_track = session.receiver_track(receiver).unwrap().unwrap();
        assert!(
            session
                .media_stream_tracks_same(fresh_track, retained_track)
                .unwrap()
        );
        assert!(
            !session
                .local_add_media_stream_track(stream, fresh_track)
                .unwrap()
        );
        assert!(
            session
                .local_remove_media_stream_track(stream, fresh_track)
                .unwrap()
        );
        assert!(
            session
                .local_add_media_stream_track(stream, retained_track)
                .unwrap()
        );
        assert_eq!(
            session.take_media_stream_dropped(stream_observer).unwrap(),
            1
        );
        session
            .close_media_stream_observer(stream_observer)
            .unwrap();
        session.release_media_stream_track(fresh_track);
        session.local_release_media_stream(stream);
        assert_eq!(session.sender_audio_processor(sender).unwrap(), None);
        assert_eq!(session.receiver_audio_processor(receiver).unwrap(), None);
        for value in [0.0, 0.25, 0.5] {
            session.set_audio_source_view_volume(alias, value).unwrap();
        }
        assert_eq!(session.audio_source_view_volume(alias).unwrap(), 0.5);
        assert_eq!(session.take_audio_observer_dropped(volume).unwrap(), 1);
        assert_eq!(session.take_audio_observer_dropped(volume).unwrap(), 0);
        session.close_audio_observer(volume).unwrap();
        session.close_audio_observer(volume).unwrap();
        session.set_audio_source_view_volume(alias, 1.0).unwrap();
        assert!(
            session
                .set_receiver_audio_track_enabled(receiver, false)
                .unwrap()
        );
        assert!(
            !session
                .set_receiver_audio_track_enabled(receiver, false)
                .unwrap()
        );
        assert!(
            session
                .set_receiver_audio_track_enabled(receiver, true)
                .unwrap()
        );
        session.close_audio_observer(track).unwrap();
        let mut options = AudioSourceOptions::default();
        options.stereo_swapping = Some(false);
        session
            .set_audio_source_view_options(installed, &options)
            .unwrap();
        assert_eq!(session.audio_source_options(sources[0]).unwrap(), options);
        let later = drive_pcm_tones(&mut session, &mut call, peers, sources);
        session.close_source(sources[0]).unwrap();
        assert_eq!(
            session.audio_source_view_state(installed).unwrap(),
            AudioSourceState::Ended
        );
        session.close_audio_observer(producer).unwrap();
        session.release_audio_source_view(alias);
        session.release_audio_source_view(installed);
        (session, later)
    })
    .join()
    .unwrap();
    thread::spawn(move || {
        let mut session = session;
        let (_, _, volume, state, track, producer, _, _, stream_observer, retained_track) =
            retained;
        for _ in 0..2 {
            assert_eq!(
                session.try_media_stream_event(stream_observer).unwrap(),
                Some(MediaStreamEvent::Changed)
            );
        }
        assert_eq!(
            session.try_media_stream_event(stream_observer).unwrap(),
            None
        );
        assert_eq!(
            session.media_stream_track_kind(retained_track).unwrap(),
            MediaType::Audio
        );
        assert_eq!(
            session.try_audio_observer_event(volume).unwrap(),
            Some(AudioObserverEvent::Volume(0.25))
        );
        assert_eq!(
            session.try_audio_observer_event(volume).unwrap(),
            Some(AudioObserverEvent::Volume(0.5))
        );
        assert_eq!(session.try_audio_observer_event(volume).unwrap(), None);
        for _ in 0..2 {
            assert_eq!(
                session.try_audio_observer_event(track).unwrap(),
                Some(AudioObserverEvent::Changed)
            );
        }
        assert_eq!(session.try_audio_observer_event(track).unwrap(), None);
        // Both options-change and producer-ended callbacks remain available
        // after the producer identifier and all outer source-view IDs retire.
        for _ in 0..2 {
            assert_eq!(
                session.try_audio_observer_event(producer).unwrap(),
                Some(AudioObserverEvent::Changed)
            );
        }
        assert_eq!(session.try_audio_observer_event(producer).unwrap(), None);
        session.close_peer(peers[0]).unwrap();
        session.close_peer(peers[1]).unwrap();
        // Stream ID retirement precedes actual continued media work, source
        // retirement, migration and native peer close on this candidate.
        assert!(
            !session
                .media_stream_track_id(retained_track)
                .unwrap()
                .is_empty()
        );
        session.release_media_stream_observer(stream_observer);
        session.release_media_stream_track(retained_track);
        session.close_audio_observer(state).unwrap();
        assert_eq!(
            session.try_audio_observer_event(state).unwrap(),
            Some(AudioObserverEvent::Changed)
        );
        assert_eq!(session.try_audio_observer_event(state).unwrap(), None);
        for observer in [volume, state, track, producer] {
            session.release_audio_observer(observer);
        }
        session.shutdown().unwrap();
        drop(session);
        assert!(!warm.is_empty() && !later.is_empty());
        assert!(
            warm.iter()
                .chain(&later)
                .all(|frame| frame.samples.iter().any(|&sample| sample != 0))
        );
    })
    .join()
    .unwrap();
}

fn receive_encoded_audio(
    session: &mut ProductionSession,
    call: &mut Call,
    peers: [SessionPeerId; 2],
    expected: &[&[u8]],
) -> Vec<EncodedAudioFrame> {
    let mut frames = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !expected.iter().all(|payload| {
        frames
            .iter()
            .any(|frame: &EncodedAudioFrame| frame.data == *payload)
    }) {
        call.drain(session, peers);
        for receiver in &call.audio_arrivals {
            while let Some(frame) = session.try_audio_frame(*receiver).unwrap() {
                frames.push(frame);
            }
        }
        assert!(
            Instant::now() < deadline,
            "encoded audio did not arrive: {frames:?}"
        );
        thread::yield_now();
    }
    frames
}

#[test]
fn replacing_one_opus_source_preserves_native_sender_and_unrelated_payloads() {
    let decoder = AudioDecoderFactory::builtin_opus().unwrap();
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .encoded_audio()
        .audio_decoder_factory(&decoder)
        .build()
        .unwrap();
    let peers = [
        session.create_peer(PeerConfiguration::default()).unwrap(),
        session.create_peer(PeerConfiguration::default()).unwrap(),
    ];
    let sources = [
        session.create_opus_source(1).unwrap(),
        session.create_opus_source(1).unwrap(),
    ];
    let transceivers = sources.map(|source| {
        session
            .publish_opus(
                peers[0],
                source,
                &format!("opus-{}", source.get()),
                RtpTransceiverDirection::SendOnly,
            )
            .unwrap()
    });
    let senders = transceivers.map(|transceiver| session.sender(transceiver).unwrap());
    let old = vec![0xf8, 0x11, 0x01];
    let independent = vec![0xf8, 0x22, 0x02];
    let replacement = vec![0xf8, 0x33, 0x03];
    let (session, mut call, first, ssrcs, independent) = thread::spawn(move || {
        let mut session = session;
        let mut call = Call::default();
        call.negotiate(&mut session, peers);
        assert_eq!(call.audio_arrivals.len(), 2);
        for receiver in &call.audio_arrivals {
            session.attach_encoded_audio(*receiver).unwrap();
        }
        for source in sources {
            assert_eq!(
                session.audio_source_state(source).unwrap(),
                AudioSourceState::Live
            );
            assert!(!session.audio_source_is_remote(source).unwrap());
            assert_eq!(session.audio_source_volume(source).unwrap(), 1.0);
            assert_eq!(
                session.audio_source_options(source).unwrap(),
                AudioSourceOptions::default()
            );
        }
        for sender in senders {
            let status = session.audio_track_status(sender).unwrap().unwrap();
            assert_eq!(status.kind, "audio");
            assert_eq!(status.state, AudioTrackState::Live);
        }
        let ssrcs = senders.map(|sender| session.sender_ssrc(sender).unwrap());
        assert_ne!(ssrcs[0], ssrcs[1]);
        for ((source, data), level) in sources
            .into_iter()
            .zip([old.clone(), independent.clone()])
            .zip([
                OpusAudioLevel::new(17, true).unwrap(),
                OpusAudioLevel::new(83, false).unwrap(),
            ])
        {
            session
                .push_opus_with_audio_level(
                    source,
                    &OpusInputFrame {
                        data,
                        rtp_timestamp: 0,
                        samples_per_channel: 960,
                    },
                    level,
                )
                .unwrap();
        }
        let frames = receive_encoded_audio(&mut session, &mut call, peers, &[&old, &independent]);
        assert!(
            frames
                .iter()
                .any(|frame| frame.data == old && frame.ssrc == ssrcs[0])
        );
        assert!(
            frames
                .iter()
                .any(|frame| frame.data == independent && frame.ssrc == ssrcs[1])
        );
        let previous = frames.iter().find(|frame| frame.data == old).unwrap();
        assert_eq!(
            (previous.audio_level_dbov, previous.voice_activity),
            (Some(17), Some(true))
        );
        let unrelated = frames
            .iter()
            .find(|frame| frame.data == independent)
            .unwrap();
        assert_eq!(
            (unrelated.audio_level_dbov, unrelated.voice_activity),
            (Some(83), Some(false))
        );
        (session, call, frames, ssrcs, independent)
    })
    .join()
    .unwrap();
    let (session, second) = thread::spawn(move || {
        let mut session = session;
        // Leave old input queued at the same native sender while replacing its
        // producer. Already accepted output may retain the old payload.
        session
            .push_opus(
                sources[0],
                &OpusInputFrame {
                    data: vec![0xf8, 0x11, 0x01],
                    rtp_timestamp: 960,
                    samples_per_channel: 960,
                },
            )
            .unwrap();
        let new_source = session.create_opus_source(1).unwrap();
        session
            .replace_audio_source(senders[0], Some(new_source), "new-opus")
            .unwrap();
        assert_eq!(
            session.audio_track_status(senders[0]).unwrap().unwrap().id,
            "new-opus"
        );
        session.close_source(sources[0]).unwrap();
        assert!(
            session
                .push_opus(
                    sources[0],
                    &OpusInputFrame {
                        data: replacement.clone(),
                        rtp_timestamp: 1920,
                        samples_per_channel: 960
                    }
                )
                .is_err()
        );
        session
            .push_opus_with_audio_level(
                new_source,
                &OpusInputFrame {
                    data: replacement.clone(),
                    rtp_timestamp: 1920,
                    samples_per_channel: 960,
                },
                OpusAudioLevel::new(0, false).unwrap(),
            )
            .unwrap();
        session
            .push_opus_with_audio_level(
                sources[1],
                &OpusInputFrame {
                    data: independent.clone(),
                    rtp_timestamp: 960,
                    samples_per_channel: 960,
                },
                OpusAudioLevel::new(83, false).unwrap(),
            )
            .unwrap();
        let frames = receive_encoded_audio(
            &mut session,
            &mut call,
            peers,
            &[&replacement, &independent],
        );
        assert!(
            frames
                .iter()
                .any(|frame| frame.data == replacement && frame.ssrc == ssrcs[0])
        );
        assert!(
            frames
                .iter()
                .any(|frame| frame.data == independent && frame.ssrc == ssrcs[1])
        );
        let current = frames
            .iter()
            .find(|frame| frame.data == replacement)
            .unwrap();
        assert_eq!(
            (current.audio_level_dbov, current.voice_activity),
            (Some(0), Some(false))
        );
        let unrelated = frames
            .iter()
            .find(|frame| frame.data == independent)
            .unwrap();
        assert_eq!(
            (unrelated.audio_level_dbov, unrelated.voice_activity),
            (Some(83), Some(false))
        );
        assert_eq!(
            senders.map(|sender| session.sender_ssrc(sender).unwrap()),
            ssrcs
        );
        session
            .replace_audio_source(senders[0], None, "unused")
            .unwrap();
        assert_eq!(session.audio_track_status(senders[0]).unwrap(), None);
        session
            .replace_audio_source(senders[0], Some(new_source), "reattached-opus")
            .unwrap();
        session
            .push_opus(
                new_source,
                &OpusInputFrame {
                    data: replacement,
                    rtp_timestamp: 2880,
                    samples_per_channel: 960,
                },
            )
            .unwrap();
        (session, frames)
    })
    .join()
    .unwrap();
    thread::spawn(move || {
        let mut session = session;
        session.shutdown().unwrap();
        drop(session);
        assert!(first.iter().any(|frame| frame.data == [0xf8, 0x11, 0x01]));
        assert!(second.iter().any(|frame| frame.data == [0xf8, 0x33, 0x03]));
    })
    .join()
    .unwrap();
    assert_eq!(decoder.decoder_statistics().unwrap().input_packets, 0);
}

fn video(luma: u8, timestamp_us: i64) -> VideoFrame {
    let mut pixels = vec![128; 64 * 48 * 3 / 2];
    pixels[..64 * 48].fill(luma);
    VideoFrame::i420(64, 48, pixels, timestamp_us, 0).unwrap()
}

#[test]
fn raw_pcm_is_the_headless_default_without_implicitly_selecting_encoded_input() {
    let mut session = ProductionSession::new(ProductionSessionConfig::default()).unwrap();
    assert!(session.create_pcm_source().is_ok());
    assert_eq!(
        session.create_opus_source(1).unwrap_err().kind,
        PeerErrorKind::UnsupportedOperation
    );
    session.shutdown().unwrap();
}

#[test]
fn raw_media_crosses_native_call_and_retains_owned_output_after_replacement_and_migration() {
    let audio_encoder = AudioEncoderFactory::builtin().unwrap();
    let audio_decoder = AudioDecoderFactory::builtin().unwrap();
    let video_encoder = VideoEncoderFactoryHandle::builtin().unwrap();
    let video_decoder = VideoDecoderFactoryHandle::builtin_vp8().unwrap();
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .audio_encoder_factory(&audio_encoder)
        .audio_decoder_factory(&audio_decoder)
        .video_encoder_factory(&video_encoder)
        .video_decoder_factory(&video_decoder)
        .build()
        .unwrap();
    drop((audio_encoder, audio_decoder, video_encoder, video_decoder));
    let peers = [
        session.create_peer(PeerConfiguration::default()).unwrap(),
        session.create_peer(PeerConfiguration::default()).unwrap(),
    ];
    let pcm = session.create_pcm_source().unwrap();
    let raw = session.create_raw_video_source().unwrap();
    let audio_transceiver = session
        .publish_pcm(
            peers[0],
            pcm,
            "pcm",
            RtpTransceiverDirection::SendOnly,
            None,
        )
        .unwrap();
    let audio_sender = session.sender(audio_transceiver).unwrap();
    let transceiver = session
        .publish_video(peers[0], raw, "raw", RtpTransceiverDirection::SendOnly, &[])
        .unwrap();
    let sender = session.sender(transceiver).unwrap();
    // Wrong-kind operations reject before handing input to a native source.
    assert!(session.push_raw_video(pcm, &video(40, 0)).is_err());
    assert!(
        session
            .publish_pcm(
                peers[0],
                raw,
                "wrong",
                RtpTransceiverDirection::SendOnly,
                None
            )
            .is_err()
    );

    let (session, mut call, retained_video) = thread::spawn(move || {
        let mut session = session;
        let mut call = Call::default();
        call.negotiate(&mut session, peers);
        session.attach_pcm(call.audio.unwrap()).unwrap();
        session.attach_raw_video(call.video.unwrap()).unwrap();
        assert!(session.attach_encoded_audio(call.audio.unwrap()).is_err());
        assert!(session.attach_encoded_video(call.video.unwrap()).is_err());
        let local = session.video_source_view(raw).unwrap();
        let local_track = session.sender_track(sender).unwrap().unwrap();
        let installed = session
            .video_track_source_view(local_track)
            .unwrap()
            .unwrap();
        let installed_alias = session
            .video_track_source_view(local_track)
            .unwrap()
            .unwrap();
        assert!(
            session
                .video_source_views_same_interface(installed, installed_alias)
                .unwrap()
        );
        session.release_video_source_view(installed_alias);
        let remote_track = session
            .receiver_track(call.video.unwrap())
            .unwrap()
            .unwrap();
        let remote = session
            .video_track_source_view(remote_track)
            .unwrap()
            .unwrap();
        let defaults = pulsebeam_webrtc_sys::VideoSinkWants::default();
        let mut receiver_wants = defaults.clone();
        receiver_wants.resolution_alignment = 6;
        receiver_wants.black_frames = true;
        session
            .update_raw_video_sink_wants(call.video.unwrap(), &receiver_wants)
            .unwrap();
        receiver_wants.black_frames = false; // SDK track override.
        assert_eq!(
            session.raw_video_sink_wants(call.video.unwrap()).unwrap(),
            Some(receiver_wants)
        );
        let direct_wants = pulsebeam_webrtc_sys::VideoSinkWants {
            resolution_alignment: 10,
            target_pixel_count: Some(0),
            ..defaults.clone()
        };
        let remote_sink = session
            .attach_video_source_sink_with_wants(remote, &direct_wants)
            .unwrap();
        let remote_sink_alias = session.alias_video_sink(remote_sink).unwrap();
        session.release_video_sink(remote_sink);
        assert_eq!(
            session.video_sink_wants(remote_sink_alias).unwrap(),
            Some(direct_wants.clone())
        );
        let bad = pulsebeam_webrtc_sys::VideoSinkWants {
            resolution_alignment: i32::MAX,
            ..defaults.clone()
        };
        assert_eq!(
            session
                .update_video_sink_wants(remote_sink_alias, &bad)
                .unwrap_err()
                .kind,
            PeerErrorKind::InvalidParameter
        );
        assert_eq!(
            session.video_sink_wants(remote_sink_alias).unwrap(),
            Some(direct_wants.clone())
        );
        let local_sink = session
            .attach_video_source_sink_with_wants(local, &direct_wants)
            .unwrap();
        let track_sink = session.attach_video_track_sink(local_track).unwrap();
        let local_status = session.video_source_status(local).unwrap();
        let remote_status = session.video_source_status(remote).unwrap();
        assert!(!local_status.remote && !local_status.supports_encoded_output);
        assert!(remote_status.remote && remote_status.supports_encoded_output);
        assert_eq!(
            session.video_source_rejected_sink_requests(local).unwrap(),
            Some(0)
        );
        assert_eq!(
            session.video_source_rejected_sink_requests(remote).unwrap(),
            None
        );
        for status in [local_status, remote_status] {
            assert!(!status.is_screencast);
            assert_eq!(status.needs_denoising, None);
            assert_eq!(status.stats, None);
        }
        let cap = std::num::NonZeroUsize::new(2).unwrap();
        let source_events = session
            .observe_video_source_changes(installed, cap)
            .unwrap();
        let track_events = session
            .observe_video_track_changes(local_track, cap)
            .unwrap();
        let remote_events = session.observe_video_source_changes(remote, cap).unwrap();
        let receive_events = session
            .observe_video_track_changes(remote_track, cap)
            .unwrap();
        session
            .set_video_track_content_hint(local_track, VideoContentHint::Fluid)
            .unwrap();
        session
            .set_video_track_content_hint(local_track, VideoContentHint::Fluid)
            .unwrap();
        session
            .set_video_track_should_receive(local_track, false)
            .unwrap();
        session
            .set_video_track_should_receive(local_track, false)
            .unwrap();
        let status = session.video_track_status(local_track).unwrap();
        assert_eq!(status.kind, "video");
        assert!(!status.should_receive);
        assert_eq!(status.content_hint, VideoContentHint::Fluid);
        // Exercise the actual receiver stop/restart control before native media.
        session
            .set_video_track_should_receive(remote_track, false)
            .unwrap();
        session
            .set_video_track_should_receive(remote_track, true)
            .unwrap();
        assert!(
            session
                .video_track_status(remote_track)
                .unwrap()
                .should_receive
        );
        session.close_video_observer(receive_events).unwrap();
        session.release_media_stream_track(local_track);
        session.release_media_stream_track(remote_track);
        (
            session,
            call,
            (
                local,
                installed,
                remote,
                source_events,
                track_events,
                remote_events,
                receive_events,
                (local_sink, remote_sink_alias, track_sink),
            ),
        )
    })
    .join()
    .unwrap();

    let (session, first_video, first_audio) = thread::spawn(move || {
        let mut session = session;
        let samples: Vec<i16> = (0..480)
            .map(|i| if i % 48 < 24 { 12000 } else { -12000 })
            .collect();
        let pcm_frame = AudioPcmFrame::i16_interleaved(48000, 1, 0, samples).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let start = Instant::now();
        let mut next_audio = Duration::ZERO;
        let mut next_video = Duration::ZERO;
        let mut audio = None;
        let mut image = None;
        while audio.is_none() || image.is_none() {
            let elapsed = start.elapsed();
            if elapsed >= next_audio {
                let capture_time = session.pcm_capture_time_now(pcm).unwrap();
                session.push_pcm_at(pcm, &pcm_frame, capture_time).unwrap();
                next_audio = elapsed + Duration::from_millis(10);
            }
            if elapsed >= next_video {
                let capture_us = session.clock_timestamp().unwrap().microseconds().unwrap();
                session.push_raw_video(raw, &video(40, capture_us)).unwrap();
                next_video = elapsed + Duration::from_millis(33);
            }
            call.drain(&mut session, peers);
            if let Some(frame) = session.try_pcm_frame(call.audio.unwrap()).unwrap() {
                // Ignore startup concealment/silence. A real nonzero source
                // must survive the native encoder, RTP and decoder paths.
                if frame
                    .samples
                    .iter()
                    .any(|sample| sample.unsigned_abs() > 2000)
                {
                    audio = Some(frame);
                }
            }
            if let Some(frame) = session.try_raw_video_frame(call.video.unwrap()).unwrap() {
                image = Some(frame);
            }
            assert!(Instant::now() < deadline, "raw media was not decoded");
            thread::yield_now();
        }
        let audio = audio.unwrap();
        assert_eq!((audio.sample_rate_hz, audio.channels), (48000, 1));
        assert_eq!(audio.samples.len(), audio.samples_per_channel as usize);
        let image = image.unwrap();
        // Stock sources have not issued either control callback on this call.
        // Frame receipt must not synthesize discarded/constraint notifications.
        assert_eq!(
            session
                .try_raw_video_sink_event(call.video.unwrap())
                .unwrap(),
            None
        );
        assert_eq!(
            session
                .dropped_raw_video_sink_events(call.video.unwrap())
                .unwrap(),
            0
        );
        let (
            local,
            installed_view,
            remote,
            source_events,
            track_events,
            _,
            _,
            (local_sink, remote_sink, track_sink),
        ) = retained_video;
        for sink in [local_sink, track_sink] {
            let frame = session
                .try_video_sink_frame(sink)
                .unwrap()
                .expect("retained native local frame");
            assert_eq!((frame.width, frame.height), (64, 48));
            assert_eq!(frame.buffer.as_bytes()[0], 40);
            assert_eq!(session.try_video_sink_event(sink).unwrap(), None);
            assert_eq!(session.video_sink_dropped_events(sink).unwrap(), 0);
        }
        let direct_remote = session
            .try_video_sink_frame(remote_sink)
            .unwrap()
            .expect("native decoded source frame");
        assert_eq!((direct_remote.width, direct_remote.height), (64, 48));
        assert!((35..=45).contains(&direct_remote.buffer.as_bytes()[0]));
        for view in [local, installed_view, remote] {
            // Exercise actual local/no-op and remote worker-proxied requests
            // during the same live call, without claiming dispatch is delivery.
            session.request_video_source_refresh(view).unwrap();
            session.generate_video_source_key_frame(view).unwrap();
            session
                .process_video_source_constraints(
                    view,
                    VideoSourceConstraints {
                        min_fps: Some(0.0),
                        max_fps: None,
                    },
                )
                .unwrap();
            let status = session.video_source_status(view).unwrap();
            assert_eq!(status.state, VideoSourceState::Live);
            assert_eq!(status.stats, None); // Actual frames do not invent GetStats.
        }
        assert_eq!((image.width, image.height), (64, 48));
        let average = image.buffer.as_bytes()[..64 * 48]
            .iter()
            .map(|&v| u64::from(v))
            .sum::<u64>()
            / (64 * 48);
        assert!(
            (35..=45).contains(&average),
            "unexpected decoded luma {average}"
        );

        let replacement = session.create_raw_video_source().unwrap();
        session
            .replace_video_source(sender, Some(replacement), "replacement")
            .unwrap();
        let stream = session.create_media_stream("video-replacement").unwrap();
        let original = session
            .create_source_track(replacement, "container-replacement")
            .unwrap();
        assert!(
            session
                .local_add_media_stream_track(stream, original)
                .unwrap()
        );
        let alias = session.local_stream_video_tracks(stream).unwrap()[0];
        session.release_media_stream_track(original);
        session.local_release_media_stream(stream);
        session.replace_sender_track(sender, Some(alias)).unwrap();
        let installed = session.sender_track(sender).unwrap().unwrap();
        assert!(session.media_stream_tracks_same(alias, installed).unwrap());
        session.release_media_stream_track(alias);
        session.release_media_stream_track(installed);
        let replacement_pcm = session.create_pcm_source().unwrap();
        session
            .replace_audio_source(audio_sender, Some(replacement_pcm), "replacement-pcm")
            .unwrap();
        assert_eq!(
            session
                .audio_track_status(audio_sender)
                .unwrap()
                .unwrap()
                .id,
            "replacement-pcm"
        );
        let replacement_samples = (0..441)
            .map(|index| {
                if index * 3000 / 44100 % 2 == 0 {
                    12000
                } else {
                    -12000
                }
            })
            .collect();
        let replacement_frame =
            AudioPcmFrame::i16_interleaved(44100, 1, 0, replacement_samples).unwrap();
        session.close_source(pcm).unwrap();
        assert!(session.push_pcm(pcm, &pcm_frame).is_err());
        session.close_source(raw).unwrap();
        assert_eq!(
            session.video_source_status(installed_view).unwrap().state,
            VideoSourceState::Ended
        );
        assert_eq!(
            session.video_source_status(local).unwrap().state,
            VideoSourceState::Ended
        );
        assert_eq!(
            session.take_video_observer_dropped(track_events).unwrap(),
            1
        );
        session.close_video_observer(source_events).unwrap();
        session.close_video_observer(track_events).unwrap();
        session.release_video_source_view(local);
        session.release_video_source_view(installed_view);
        assert!(session.push_raw_video(raw, &video(40, 0)).is_err());
        // Closing and replacing sinks must release their native reservation.
        session.close_media_sink(call.video.unwrap()).unwrap();
        session.attach_raw_video(call.video.unwrap()).unwrap();
        session.close_media_sink(call.audio.unwrap()).unwrap();
        session.attach_pcm(call.audio.unwrap()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let start = Instant::now();
        let mut next_video = Duration::ZERO;
        let mut next_audio = Duration::ZERO;
        let mut saw_video = false;
        let mut saw_audio = false;
        while !saw_video || !saw_audio {
            let elapsed = start.elapsed();
            if elapsed >= next_audio {
                let capture_time = session.pcm_capture_time_now(replacement_pcm).unwrap();
                session
                    .push_pcm_at(replacement_pcm, &replacement_frame, capture_time)
                    .unwrap();
                next_audio = elapsed + Duration::from_millis(10);
            }
            if elapsed >= next_video {
                let capture_us = session.clock_timestamp().unwrap().microseconds().unwrap();
                session
                    .push_raw_video(replacement, &video(200, capture_us))
                    .unwrap();
                next_video = elapsed + Duration::from_millis(33);
            }
            call.drain(&mut session, peers);
            if let Some(frame) = session.try_raw_video_frame(call.video.unwrap()).unwrap() {
                let average = frame.buffer.as_bytes()[..64 * 48]
                    .iter()
                    .map(|&v| u64::from(v))
                    .sum::<u64>()
                    / (64 * 48);
                saw_video |= (195..=205).contains(&average);
            }
            if let Some(frame) = session.try_pcm_frame(call.audio.unwrap()).unwrap() {
                // The 44.1 kHz replacement carries 1500 Hz, unlike the retired
                // 1000 Hz input. Observe native resampling and actual new media,
                // not silence or PLC decay after detaching the old producer.
                let correlation = |lag: usize| {
                    frame
                        .samples
                        .iter()
                        .zip(frame.samples.iter().skip(lag))
                        .fold((0i64, 0i64), |(dot, energy), (&a, &b)| {
                            (
                                dot + i64::from(a) * i64::from(b),
                                energy + i64::from(a) * i64::from(a),
                            )
                        })
                };
                let (period, energy) = correlation(32);
                let (opposite, opposite_energy) = correlation(48);
                saw_audio |=
                    energy > 1_000_000 && period > energy / 2 && opposite < -opposite_energy / 2;
            }
            assert!(
                Instant::now() < deadline,
                "replacement media missing: video={saw_video}, audio={saw_audio}"
            );
            thread::yield_now();
        }
        // Leave queued native work for migrated final shutdown.
        let capture_us = session.clock_timestamp().unwrap().microseconds().unwrap();
        session
            .push_raw_video(replacement, &video(200, capture_us))
            .unwrap();
        (session, image, audio)
    })
    .join()
    .unwrap();
    thread::spawn(move || {
        let mut session = session;
        let (
            _,
            _,
            remote,
            source_events,
            track_events,
            remote_events,
            receive_events,
            (local_sink, remote_sink, track_sink),
        ) = retained_video;
        for sink in [local_sink, remote_sink, track_sink] {
            assert!(session.video_sink_wants(sink).unwrap().is_some());
            session.close_video_sink(sink).unwrap();
            assert_eq!(session.video_sink_wants(sink).unwrap(), None);
            session.release_video_sink(sink);
        }
        session.close_peer(peers[0]).unwrap();
        session.close_peer(peers[1]).unwrap();
        assert_eq!(
            session.video_source_status(remote).unwrap().state,
            VideoSourceState::Ended
        );
        session.close_video_observer(remote_events).unwrap();
        session.release_video_source_view(remote);
        for observer in [source_events, remote_events] {
            assert_eq!(
                session.try_video_observer_event(observer).unwrap(),
                Some(VideoObserverEvent::Changed)
            );
            assert_eq!(session.try_video_observer_event(observer).unwrap(), None);
            session.release_video_observer(observer);
        }
        for observer in [track_events, receive_events] {
            for _ in 0..2 {
                assert_eq!(
                    session.try_video_observer_event(observer).unwrap(),
                    Some(VideoObserverEvent::Changed)
                );
            }
            assert_eq!(session.try_video_observer_event(observer).unwrap(), None);
            session.release_video_observer(observer);
        }
        session.shutdown().unwrap();
        session.shutdown().unwrap();
        drop(session);
        // Caller-owned output has no session, sink or native-buffer borrow.
        assert!((35..=45).contains(&first_video.buffer.as_bytes()[0]));
        assert!(
            first_audio
                .samples
                .iter()
                .any(|sample| sample.unsigned_abs() > 2000)
        );
    })
    .join()
    .unwrap();
}
