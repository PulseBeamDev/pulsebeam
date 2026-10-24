use pulsebeam_webrtc_sys::*;
use std::{
    collections::HashMap,
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

// Retirement crosses the SDK's process-wide production/controlled exclusion.
static TEST_LOCK: Mutex<()> = Mutex::new(());

struct ForwardClock(Clock);
impl ClockProvider for ForwardClock {
    fn current_time(&self) -> FiniteTimestamp {
        FiniteTimestamp::from_microseconds(self.0.time_in_microseconds()).unwrap()
    }
    fn convert_timestamp_to_ntp_time(&self, timestamp: Timestamp) -> NtpTime {
        self.0
            .convert_timestamp_to_ntp_time(timestamp)
            .unwrap_or(NtpTime::from_raw(0))
    }
}

fn pcm(timestamp: i64) -> AudioPcmFrame {
    let samples = (0..160)
        .map(|index| (1000.0 * (std::f64::consts::TAU * index as f64 / 16.0).sin()) as i16)
        .collect();
    AudioPcmFrame::i16_interleaved(16000, 1, timestamp, samples).unwrap()
}

#[test]
fn encoded_selection_is_explicit_and_preserves_the_pcm_default() {
    let _lock = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let mut encoded = ProductionSession::builder(ProductionSessionConfig::default())
        .encoded_audio()
        .build()
        .unwrap();
    assert!(
        !encoded
            .audio_processing_state()
            .unwrap()
            .has_software_module
    );
    assert!(encoded.create_pcm_source().is_err());
    let encoded_source = encoded.create_opus_source(1).unwrap();
    assert!(
        ProductionSession::builder(ProductionSessionConfig::default())
            .encoded_audio()
            .audio_processing(AudioProcessingConfig::default())
            .build()
            .is_err()
    );
    let mut raw = ProductionSession::builder(ProductionSessionConfig::default())
        .audio_processing(AudioProcessingConfig::default())
        .build()
        .unwrap();
    assert!(raw.audio_processing_state().unwrap().has_software_module);
    assert!(raw.create_opus_source(1).is_err());
    assert!(raw.push_pcm(encoded_source, &pcm(0)).is_err());
    let source = raw.create_pcm_source().unwrap();
    let foreign_peer = encoded.create_peer(PeerConfiguration::default()).unwrap();
    let before = raw.audio_processing_state().unwrap();
    assert!(
        raw.publish_pcm(
            foreign_peer,
            source,
            "foreign",
            RtpTransceiverDirection::SendOnly,
            Some(AudioProcessingOptions {
                gain_control: ProcessingChoice::Software,
                ..Default::default()
            })
        )
        .is_err()
    );
    assert_eq!(raw.audio_processing_state().unwrap(), before);
    let closed_peer = raw.create_peer(PeerConfiguration::default()).unwrap();
    raw.close_peer(closed_peer).unwrap();
    assert_eq!(
        raw.publish_pcm(
            closed_peer,
            source,
            "closed",
            RtpTransceiverDirection::SendOnly,
            None
        )
        .unwrap_err()
        .kind,
        PeerErrorKind::Closed
    );
    let mut malformed = pcm(0);
    malformed.samples.pop();
    assert!(raw.push_pcm(source, &malformed).is_err());
    raw.push_pcm(source, &pcm(0)).unwrap();
    raw.close_source(source).unwrap();
    raw.close_source(source).unwrap();
    assert!(raw.push_pcm(source, &pcm(0)).is_err());
    raw.shutdown().unwrap();
    assert!(raw.audio_processing_state().is_err());
    // Explicit factories are not replaced by pcm_audio's absent-choice default.
    assert!(
        ProductionSession::builder(ProductionSessionConfig::default())
            .pcm_audio()
            .audio_encoder_factory(&AudioEncoderFactory::with_opus_frames().unwrap())
            .audio_processing(AudioProcessingConfig::default())
            .build()
            .is_err()
    );
}

#[derive(Default)]
struct Events {
    completed: HashMap<(SessionPeerId, OperationId), Option<SessionDescription>>,
    gathered: [bool; 2],
    connected: [bool; 2],
    receiver: Option<SessionReceiverId>,
}
impl Events {
    fn drain(&mut self, session: &mut ProductionSession, peers: [SessionPeerId; 2]) {
        for (index, peer) in peers.into_iter().enumerate() {
            while let Some(event) = session.try_peer_event(peer).unwrap() {
                match event {
                    SessionEvent::OperationComplete(done) => {
                        self.completed
                            .insert((peer, done.operation_id), done.result.unwrap());
                    }
                    SessionEvent::IceGatheringStateChanged(IceGatheringState::Complete) => {
                        self.gathered[index] = true;
                    }
                    SessionEvent::ConnectionStateChanged(ConnectionState::Connected) => {
                        self.connected[index] = true;
                    }
                    SessionEvent::Track { receiver, .. } if index == 1 => {
                        assert!(self.receiver.replace(receiver).is_none());
                    }
                    _ => {}
                }
            }
        }
    }
    fn until(
        &mut self,
        session: &mut ProductionSession,
        peers: [SessionPeerId; 2],
        ready: impl Fn(&Self) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            self.drain(session, peers);
            if ready(self) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "native call did not reach the requested state"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn finish(
        &mut self,
        session: &mut ProductionSession,
        peers: [SessionPeerId; 2],
        peer: SessionPeerId,
        operation: OperationId,
    ) -> Option<SessionDescription> {
        self.until(session, peers, |events| {
            events.completed.contains_key(&(peer, operation))
        });
        self.completed.remove(&(peer, operation)).unwrap()
    }
    fn local(
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
        self.until(session, peers, |events| events.gathered[index]);
        let descriptions = session.descriptions(peers[index]).unwrap();
        descriptions
            .current_local
            .or(descriptions.pending_local)
            .unwrap()
    }
}

fn measure(
    session: &mut ProductionSession,
    source: SessionSourceId,
    receiver: SessionReceiverId,
    timestamp: &mut i64,
) -> (f64, DecodedAudioFrame) {
    let mut magnitude = 0_u64;
    let mut count = 0_usize;
    let mut last = None;
    // Real native encoding/transport/playout is asynchronous. Pace actual 10-ms
    // capture blocks and allow pipeline history to settle before measurement.
    for block in 0..100 {
        session.push_pcm(source, &pcm(*timestamp)).unwrap();
        *timestamp += 10_000;
        thread::sleep(Duration::from_millis(10));
        if let Some(frame) = session.try_pcm_audio_frame(receiver).unwrap() {
            assert_eq!(frame.channels, 1);
            assert_eq!(frame.samples_per_channel, frame.sample_rate_hz / 100);
            assert_eq!(frame.samples.len(), frame.samples_per_channel as usize);
            if block >= 60 {
                magnitude += frame
                    .samples
                    .iter()
                    .map(|&sample| i64::from(sample).unsigned_abs())
                    .sum::<u64>();
                count += frame.samples.len();
                last = Some(frame);
            }
        }
    }
    assert!(count > 0);
    (magnitude as f64 / count as f64, last.unwrap())
}

#[test]
fn migrated_actor_processes_real_pcm_and_retires_its_selected_audio_graph() {
    let _lock = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    // Faithful current values do not manufacture durable clock evidence. The
    // installed encoder must reject its packet-loss control before native use.
    let clock = Clock::from_provider(ForwardClock(Clock::real_time().unwrap()));
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .clock_handle(&clock)
        .pcm_audio()
        .audio_processing(AudioProcessingConfig {
            echo_cancellation: false,
            noise_suppression: NoiseSuppression::Off,
            gain_control: GainControl::FixedDigital,
        })
        .build()
        .unwrap();
    let failure = session.audio_encoder_failure_observer().unwrap();
    drop(clock);
    let peers = [
        session.create_peer(PeerConfiguration::default()).unwrap(),
        session.create_peer(PeerConfiguration::default()).unwrap(),
    ];
    let (session, source, sender) = thread::spawn(move || {
        let source = session.create_pcm_source().unwrap();
        let transceiver = session
            .publish_pcm(
                peers[0],
                source,
                "pcm",
                RtpTransceiverDirection::SendOnly,
                Some(AudioProcessingOptions::default()),
            )
            .unwrap();
        let sender = session.sender(transceiver).unwrap();
        (session, source, sender)
    })
    .join()
    .unwrap();
    let (session, receiver, baseline, timestamp) = thread::spawn(move || {
        let mut session = session;
        let mut events = Events::default();
        let operation = session.create_offer(peers[0], false).unwrap();
        let offer = events
            .finish(&mut session, peers, peers[0], operation)
            .unwrap();
        let offer = events.local(&mut session, peers, 0, offer);
        let operation = session.set_remote_description(peers[1], offer).unwrap();
        events.finish(&mut session, peers, peers[1], operation);
        let operation = session.create_answer(peers[1]).unwrap();
        let answer = events
            .finish(&mut session, peers, peers[1], operation)
            .unwrap();
        let answer = events.local(&mut session, peers, 1, answer);
        let operation = session.set_remote_description(peers[0], answer).unwrap();
        events.finish(&mut session, peers, peers[0], operation);
        events.until(&mut session, peers, |events| events.connected == [true; 2]);
        let receiver = events.receiver.unwrap();
        session.attach_pcm_audio(receiver).unwrap();
        assert!(session.attach_encoded_audio(receiver).is_err());
        let mut timestamp = 0;
        let (baseline, _) = measure(&mut session, source, receiver, &mut timestamp);
        assert!(baseline > 20.0, "PCM did not traverse the negotiated call");
        (session, receiver, baseline, timestamp)
    })
    .join()
    .unwrap();
    let retained = thread::spawn(move || {
        let mut session = session;
        session
            .set_audio_processing_options(
                sender,
                AudioProcessingOptions {
                    gain_control: ProcessingChoice::Software,
                    ..Default::default()
                },
            )
            .unwrap();
        let mut timestamp = timestamp;
        let (processed, retained) = measure(&mut session, source, receiver, &mut timestamp);
        assert!(
            processed > baseline * 1.5,
            "selected fixed digital processing did not affect real audio: {baseline} -> {processed}"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while failure.failure().is_none() {
            assert!(
                Instant::now() < deadline,
                "installed RTCP packet-loss control was not observed"
            );
            session.push_pcm(source, &pcm(timestamp)).unwrap();
            timestamp += 10_000;
            thread::sleep(Duration::from_millis(10));
            session.try_pcm_audio_frame(receiver).unwrap();
        }
        assert_eq!(
            failure.failure(),
            Some(AudioEncoderSdkFailure {
                operation: AudioEncoderSdkOperation::PACKET_LOSS,
                error: AudioEncoderError::UnsafeClock,
            })
        );
        let state = session.audio_processing_state().unwrap();
        assert_eq!(
            state.gain_control.effective,
            ProcessingImplementation::Software
        );
        assert!(!state.gain_control.platform_available);
        session
            .set_audio_processing_options(
                sender,
                AudioProcessingOptions {
                    echo_cancellation: ProcessingChoice::Software,
                    gain_control: ProcessingChoice::Software,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            session.push_pcm(source, &pcm(timestamp)).unwrap_err().kind,
            PeerErrorKind::UnsupportedOperation
        );
        session.detach_audio(receiver).unwrap();
        session.attach_encoded_audio(receiver).unwrap();
        assert!(session.attach_pcm_audio(receiver).is_err());
        session.detach_audio(receiver).unwrap();
        session.attach_pcm_audio(receiver).unwrap();
        session.close_source(source).unwrap();
        assert!(session.push_pcm(source, &pcm(timestamp)).is_err());
        session.close_peer(peers[0]).unwrap();
        session.close_peer(peers[1]).unwrap();
        assert!(session.try_pcm_audio_frame(receiver).unwrap().is_none());
        session.shutdown().unwrap();
        session.shutdown().unwrap();
        drop(session);
        assert_eq!(
            failure.failure().unwrap().error,
            AudioEncoderError::UnsafeClock
        );
        let world = ControlledWorld::acquire(9307, Duration::from_secs(1)).unwrap();
        assert_eq!(world.pump(0).dispatched, 0);
        retained
    })
    .join()
    .unwrap();
    assert!(retained.samples.iter().any(|&sample| sample != 0));
}
