use pulsebeam_webrtc_sys::*;
use std::{
    collections::{HashMap, HashSet},
    num::NonZeroUsize,
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Condvar, Mutex},
    task::{Context, Wake, Waker},
    thread,
    time::{Duration, Instant},
};

#[derive(Default)]
struct Notification(Mutex<bool>, Condvar);
impl Wake for Notification {
    fn wake(self: Arc<Self>) {
        *self.0.lock().unwrap() = true;
        self.1.notify_one();
    }
}
fn wait(session: &mut ProductionSession) {
    let notification = Arc::new(Notification::default());
    let waker = Waker::from(notification.clone());
    if session
        .poll_ready(&mut Context::from_waker(&waker))
        .is_ready()
    {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut notified = notification.0.lock().unwrap();
    while !*notified {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "native connectivity readiness did not wake the actor"
        );
        notified = notification.1.wait_timeout(notified, remaining).unwrap().0;
    }
}
#[derive(Default)]
struct Events {
    complete: HashMap<(SessionPeerId, OperationId), Option<SessionDescription>>,
    gathered: HashSet<SessionPeerId>,
    channels: Vec<(SessionPeerId, SessionChannelId, Vec<u8>)>,
    stats: HashMap<(SessionPeerId, OperationId), PeerStatsSnapshot>,
    pairs: Vec<(SessionPeerId, IceCandidatePairChange)>,
    messages: Vec<(SessionPeerId, PeerDataChannelMessage)>,
    usage: Vec<(SessionPeerId, i32)>,
    renegotiation: HashSet<SessionPeerId>,
    receivers: Vec<(SessionPeerId, SessionReceiverId)>,
}
impl Events {
    fn drain(&mut self, session: &mut ProductionSession, peers: [SessionPeerId; 2]) {
        for peer in peers {
            while let Some(event) = session.try_peer_event(peer).unwrap() {
                match event {
                    SessionEvent::DataChannelMessage(value) => self.messages.push((peer, value)),
                    SessionEvent::InterestingUsage { usage_pattern } => {
                        self.usage.push((peer, usage_pattern))
                    }
                    SessionEvent::RenegotiationNeeded => {
                        self.renegotiation.insert(peer);
                    }
                    SessionEvent::Track { receiver, .. } => self.receivers.push((peer, receiver)),
                    SessionEvent::OperationComplete(done) => {
                        assert!(
                            self.complete
                                .insert((peer, done.operation_id), done.result.unwrap())
                                .is_none()
                        );
                    }
                    SessionEvent::Stats(value) => {
                        assert!(
                            self.stats
                                .insert((peer, value.operation_id), value)
                                .is_none()
                        );
                    }
                    SessionEvent::IceSelectedCandidatePairChanged(value) => {
                        self.pairs.push((peer, value))
                    }
                    SessionEvent::DataChannel { channel, label } => {
                        self.channels.push((peer, channel, label));
                    }
                    SessionEvent::IceGatheringStateChanged(IceGatheringState::Complete) => {
                        self.gathered.insert(peer);
                    }
                    _ => {}
                }
            }
        }
    }
    fn stats(
        &mut self,
        session: &mut ProductionSession,
        peers: [SessionPeerId; 2],
        peer: SessionPeerId,
        id: OperationId,
    ) -> PeerStatsSnapshot {
        loop {
            self.drain(session, peers);
            if let Some(value) = self.stats.remove(&(peer, id)) {
                return value;
            }
            wait(session);
        }
    }
    fn finish(
        &mut self,
        session: &mut ProductionSession,
        peers: [SessionPeerId; 2],
        peer: SessionPeerId,
        id: OperationId,
    ) -> Option<SessionDescription> {
        loop {
            self.drain(session, peers);
            if let Some(result) = self.complete.remove(&(peer, id)) {
                return result;
            }
            wait(session);
        }
    }
    fn gather(
        &mut self,
        session: &mut ProductionSession,
        peers: [SessionPeerId; 2],
        peer: SessionPeerId,
        description: SessionDescription,
    ) -> SessionDescription {
        let id = session.set_local_description(peer, description).unwrap();
        self.finish(session, peers, peer, id);
        while !self.gathered.contains(&peer) {
            self.drain(session, peers);
            if !self.gathered.contains(&peer) {
                wait(session);
            }
        }
        session.descriptions(peer).unwrap().local().unwrap().clone()
    }
}

struct NetworkDrop {
    owner: Arc<Mutex<Option<ProductionSession>>>,
    finished: Arc<AtomicBool>,
    rejected: Arc<Mutex<Option<PeerErrorKind>>>,
    peer: SessionPeerId,
}
impl Wake for NetworkDrop {
    fn wake(self: Arc<Self>) {
        if let Some(mut session) = self.owner.lock().unwrap().take() {
            *self.rejected.lock().unwrap() = session
                .set_data_channel_message_observation(self.peer, false)
                .err()
                .map(|e| e.kind);
            drop(session);
            self.finished.store(true, Ordering::SeqCst);
        }
    }
}
#[test]
fn migrated_actor_preserves_native_channel_configuration_metadata_and_text_bytes() {
    let mut session = ProductionSession::new(Default::default()).unwrap();
    let mut config = RtcConfiguration::default();
    config.options.report_usage_pattern_delay_ms = Some(20);
    let peers = [
        session
            .create_peer_with_configuration(config.clone())
            .unwrap(),
        session.create_peer_with_configuration(config).unwrap(),
    ];
    for peer in peers {
        session
            .set_data_channel_message_observation(peer, true)
            .unwrap();
    }
    let start = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let label = b"actor\xff\0".to_vec();
    let protocol = b"protocol\x80\0".to_vec();
    let outgoing = session
        .create_channel_bytes(
            peers[0],
            &label,
            Some(DataChannelConfiguration {
                reliable: true,
                max_retransmit_time_ms: Some(-99),
                max_retransmits: Some(i32::MAX),
                protocol: protocol.clone(),
                ..Default::default()
            }),
        )
        .unwrap();
    let expected_label = label.clone();
    drop(label);
    let rejected = session
        .create_channel_bytes(
            peers[0],
            b"invalid",
            Some(DataChannelConfiguration {
                max_retransmit_time_ms: Some(1),
                max_retransmits: Some(1),
                ..Default::default()
            }),
        )
        .unwrap_err();
    assert_eq!(rejected.kind, PeerErrorKind::InvalidParameter);
    assert!(!rejected.message.is_empty());
    let mut events = Events::default();
    let id = session.create_offer(peers[0], false).unwrap();
    let offer = events.finish(&mut session, peers, peers[0], id).unwrap();
    let offer = events.gather(&mut session, peers, peers[0], offer);
    let (report, legacy) = thread::spawn(move || {
        let mut session = session;
        let id = session.set_remote_description(peers[1], offer).unwrap();
        events.finish(&mut session, peers, peers[1], id);
        let id = session.create_answer(peers[1]).unwrap();
        let answer = events.finish(&mut session, peers, peers[1], id).unwrap();
        let answer = events.gather(&mut session, peers, peers[1], answer);
        let id = session.set_remote_description(peers[0], answer).unwrap();
        events.finish(&mut session, peers, peers[0], id);
        let incoming = loop {
            events.drain(&mut session, peers);
            if let Some((_, incoming, _)) = events.channels.iter().find(|(peer, _, label)| *peer == peers[1] && label == &expected_label) {
                if session.channel_state(outgoing).unwrap() == DataChannelState::Open && session.channel_state(*incoming).unwrap() == DataChannelState::Open { break *incoming; }
            }
            wait(&mut session);
        };
        for channel in [outgoing, incoming] {
            assert_eq!(session.channel_label(channel).unwrap(), expected_label);
            let config = session.channel_configuration(channel).unwrap();
            assert_eq!(config.protocol, protocol);
            assert_eq!(config.max_retransmit_time_ms, None);
            assert_eq!(config.max_retransmits, Some(65535));
            assert!(!config.reliable);
        }
        let message = DataChannelMessage { kind: DataChannelMessageKind::Text, bytes: vec![0xff, 0, 0x80] };
        let send_id = session.send_async(outgoing, message.clone()).unwrap();
        let (mut completed, mut received) = (false, false);
        while !completed || !received {
            while let Some(event) = session.try_channel_event(outgoing).unwrap() {
                if let DataChannelEvent::SendComplete(value) = event {
                    assert_eq!(value.send_id, send_id); value.result.unwrap(); completed = true;
                }
            }
            while let Some(event) = session.try_channel_event(incoming).unwrap() {
                if let DataChannelEvent::Message(value) = event { assert_eq!(value, message); assert!(!received); received = true; }
            }
            if !completed || !received { wait(&mut session); }
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while events.messages.len() < 2 {
            events.drain(&mut session, peers);
            assert!(Instant::now() < deadline, "missing native send/receive observation");
            if events.messages.len() < 2 { thread::sleep(Duration::from_millis(1)); }
        }
        assert_eq!(events.messages.len(), 2);
        for (peer, logged) in &events.messages {
            assert_eq!(logged.label, expected_label);
            assert_eq!(logged.message, message);
            assert_eq!(logged.datachannel_id, session.channel_configuration(if *peer == peers[0] { outgoing } else { incoming }).unwrap().id.unwrap());
            assert_eq!(logged.direction, if *peer == peers[0] { DataChannelMessageDirection::Send } else { DataChannelMessageDirection::Receive });
            assert!(logged.unix_timestamp_ms >= start);
            assert!(logged.unix_timestamp_ms <= std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while events.usage.len() < 2 {
            events.drain(&mut session, peers);
            assert!(Instant::now() < deadline, "no actual scheduled SDK usage reports");
            thread::sleep(Duration::from_millis(1));
        }
        assert!(events.renegotiation.contains(&peers[0]));
        let operation = session.request_stats(peers[1]).unwrap();
        let snapshot = loop {
            if let Some(SessionEvent::Stats(value)) = session.try_peer_event(peers[1]).unwrap() {
                if value.operation_id == operation { break value; }
            } else { wait(&mut session); }
        };
        assert!(snapshot.records.iter().any(|record| matches!(record, PeerStatsRecord::DataChannel(value)
            if value.label.as_deref() == Some(expected_label.as_slice()) && value.protocol.as_deref() == Some(protocol.as_slice()))));
        assert!(snapshot.report.len() > snapshot.records.len(), "full report includes the other native families");
        let native_channel = snapshot.report.of_class(RtcStatsClass::DataChannelStats).into_iter().find(|v| matches!(v.values(), RtcStatsValues::DataChannelStats(value) if value.label.as_deref() == Some(expected_label.as_slice()))).unwrap();
        let RtcStatsValues::DataChannelStats(values) = native_channel.values() else { panic!() };
        assert_eq!(values.protocol.as_deref(), Some(protocol.as_slice()));
        assert_eq!(values.bytes_received, Some(3));
        let operation = session.request_legacy_stats(peers[1], LegacyStatsOutputLevel::Debug).unwrap();
        assert!(session.request_stats(peers[1]).is_err());
        let legacy = loop {
            match session.try_peer_event(peers[1]).unwrap() {
                Some(SessionEvent::LegacyStats(value)) if value.operation_id == operation => break value,
                Some(SessionEvent::OperationComplete(value)) if value.operation_id == operation => panic!("legacy request failed: {value:?}"),
                _ => wait(&mut session),
            }
        };
        let channel = legacy.reports.iter().find(|r| r.id.kind() == LegacyStatsType::DataChannel).unwrap();
        assert_eq!(channel.find(LegacyStatsName::Label).unwrap().value, LegacyStatsValueData::String(expected_label.clone()));
        assert!(legacy.reports.iter().flat_map(|r| &r.values).any(|v| matches!(&v.value, LegacyStatsValueData::Id(_))), "negotiated native transport links use retained SDK IDs");
        // Both directions/kinds, independent channel delivery and bounded log
        // overflow. Disable fences callbacks but retains already-copied values.
        let reverse = DataChannelMessage::binary([0x80, 0, 0xff, 4]);
        assert_eq!(session.send(incoming, reverse.clone()).unwrap(), DataChannelSendResult::Sent);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            events.drain(&mut session, peers);
            if events.messages.iter().filter(|(_, m)| m.message == reverse).count() == 2 { break; }
            assert!(Instant::now() < deadline); thread::sleep(Duration::from_millis(1));
        }
        let mut reverse_delivered = false;
        while let Some(event) = session.try_channel_event(outgoing).unwrap() { if let DataChannelEvent::Message(m) = event { assert_eq!(m, reverse); reverse_delivered = true; } }
        assert!(reverse_delivered);
        for peer in peers { events.drain(&mut session, peers); assert_eq!(session.peer_event_observation(peer).unwrap().dropped_control_events, 0); }
        let before = session.channel_counters(incoming).unwrap().messages_received;
        for i in 0..200_u16 {
            assert_eq!(session.send(outgoing, DataChannelMessage::binary(i.to_le_bytes())).unwrap(), DataChannelSendResult::Sent);
        }
        while session.channel_counters(incoming).unwrap().messages_received < before + 200 {
            assert!(Instant::now() < deadline); thread::sleep(Duration::from_millis(1));
        }
        for peer in peers { session.set_data_channel_message_observation(peer, false).unwrap(); }
        let copied = events.messages.len();
        events.drain(&mut session, peers);
        assert!(events.messages.len() > copied, "disable discarded queued copies");
        for peer in peers { assert!(session.peer_event_observation(peer).unwrap().dropped_control_events > 0); }
        while session.try_channel_event(incoming).unwrap().is_some() {}
        let before = session.channel_counters(incoming).unwrap().messages_received;
        let disabled = DataChannelMessage::binary(b"disabled observer still delivers".to_vec());
        assert_eq!(session.send(outgoing, disabled.clone()).unwrap(), DataChannelSendResult::Sent);
        while session.channel_counters(incoming).unwrap().messages_received == before {
            assert!(Instant::now() < deadline); thread::sleep(Duration::from_millis(1));
        }
        events.drain(&mut session, peers);
        assert!(!events.messages.iter().any(|(_, m)| m.message == disabled));
        let mut disabled_delivered = false;
        while let Some(event) = session.try_channel_event(incoming).unwrap() { if let DataChannelEvent::Message(m) = event { assert_eq!(m, disabled); disabled_delivered = true; } }
        assert!(disabled_delivered);
        for channel in [outgoing, incoming] { while session.try_channel_event(channel).unwrap().is_some() {} }
        session.set_data_channel_message_observation(peers[0], true).unwrap();
        let owner = Arc::new(Mutex::new(Some(session)));
        let finished = Arc::new(AtomicBool::new(false));
        let rejected = Arc::new(Mutex::new(None));
        let waker = Waker::from(Arc::new(NetworkDrop { owner: owner.clone(), finished: finished.clone(), rejected: rejected.clone(), peer: peers[0] }));
        {
            let mut guard = owner.lock().unwrap();
            let session = guard.as_mut().unwrap();
            while session.poll_ready(&mut Context::from_waker(&waker)).is_ready() {}
            session.send_async(outgoing, DataChannelMessage::binary(b"native-network-final-owner".to_vec())).unwrap();
        }
        while !finished.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "network callback did not retire final actor"); thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(*rejected.lock().unwrap(), Some(PeerErrorKind::UnsupportedOperation), "did not enter the actual SDK network role");
        assert!(owner.lock().unwrap().is_none());
        loop {
            match ControlledWorld::acquire(1337, Duration::from_secs(2)) {
                Ok(world) => { drop(world); break; }
                Err(_) => { assert!(Instant::now() < deadline, "network-role deferred graph did not retire"); thread::sleep(Duration::from_millis(1)); }
            }
        }
        (snapshot.report, legacy)
    }).join().unwrap();
    assert!(
        legacy
            .reports
            .iter()
            .all(|r| !r.id.to_native_string().is_empty())
    );
    assert!(!report.is_empty());
    assert!(!report.of_class(RtcStatsClass::DataChannelStats).is_empty());
}

#[test]
fn migrated_actor_keeps_reused_native_sender_ids_as_distinct_resources() {
    fn negotiate(session: &mut ProductionSession, peers: [SessionPeerId; 2], events: &mut Events) {
        let id = session.create_offer(peers[0], false).unwrap();
        let offer = events.finish(session, peers, peers[0], id).unwrap();
        let id = session
            .set_local_description(peers[0], offer.clone())
            .unwrap();
        events.finish(session, peers, peers[0], id);
        let id = session.set_remote_description(peers[1], offer).unwrap();
        events.finish(session, peers, peers[1], id);
        let id = session.create_answer(peers[1]).unwrap();
        let answer = events.finish(session, peers, peers[1], id).unwrap();
        let id = session
            .set_local_description(peers[1], answer.clone())
            .unwrap();
        events.finish(session, peers, peers[1], id);
        let id = session.set_remote_description(peers[0], answer).unwrap();
        events.finish(session, peers, peers[0], id);
    }
    let mut session = ProductionSession::builder(Default::default())
        .encoded_audio()
        .build()
        .unwrap();
    let peers = [
        session.create_peer(Default::default()).unwrap(),
        session.create_peer(Default::default()).unwrap(),
    ];
    let source = session.create_opus_source(1).unwrap();
    let retired = session
        .publish_opus(
            peers[0],
            source,
            "actor-reused-id",
            RtpTransceiverDirection::SendOnly,
        )
        .unwrap();
    let old_sender = session.sender(retired).unwrap();
    let old_receiver = session.receiver(retired).unwrap();
    session
        .register_sender_packet_observer(old_sender, 2)
        .unwrap();
    let native_id = session.sender_native_id(old_sender).unwrap();
    let session = thread::spawn(move || {
        let mut session = session;
        let mut events = Events::default();
        negotiate(&mut session, peers, &mut events);
        session.stop_standard(retired).unwrap();
        negotiate(&mut session, peers, &mut events);
        assert!(session.transceiver_stopped(retired).unwrap());
        let source = session.create_opus_source(1).unwrap();
        let replacement = session
            .publish_opus(
                peers[0],
                source,
                "actor-reused-id",
                RtpTransceiverDirection::SendOnly,
            )
            .unwrap();
        let new_sender = session.sender(replacement).unwrap();
        let new_receiver = session.receiver(replacement).unwrap();
        assert_eq!(session.sender_native_id(new_sender).unwrap(), native_id);
        assert_ne!(retired, replacement);
        assert_ne!(old_sender, new_sender);
        assert_ne!(old_receiver, new_receiver);
        assert_eq!(session.sender(replacement).unwrap(), new_sender);
        session
            .register_sender_packet_observer(new_sender, 2)
            .unwrap();
        session
            .unregister_sender_packet_observer(old_sender)
            .unwrap();
        assert!(
            session
                .register_sender_packet_observer(new_sender, 2)
                .is_err(),
            "old observer retirement must not unregister the new endpoint"
        );
        session.close_peer(peers[0]).unwrap();
        session.close_peer(peers[1]).unwrap();
        assert_eq!(session.sender_native_id(old_sender).unwrap(), native_id);
        session
    })
    .join()
    .unwrap();
    let mut session = session;
    session.shutdown().unwrap();
}

#[test]
fn migrated_actor_owns_dtmf_transport_aliases_and_revocable_ice() {
    fn assert_send<T: Send>() {}
    assert_send::<DtlsTransportInformation>();
    assert_send::<DtlsTransportEvent>();
    assert_send::<SessionSctpTransportInformation>();
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .encoded_audio()
        .build()
        .unwrap();
    let mut config = RtcConfiguration::default();
    config.options.bundle_policy = BundlePolicy::MaxBundle;
    let peers = [
        session
            .create_peer_with_configuration(config.clone())
            .unwrap(),
        session.create_peer_with_configuration(config).unwrap(),
    ];
    assert!(session.peer_sctp_transport(peers[0]).unwrap().is_none());
    assert!(
        session
            .peer_dtls_transport_by_mid(peers[0], b"absent")
            .unwrap()
            .is_none()
    );
    let source = session.create_opus_source(1).unwrap();
    let transceiver = session
        .publish_opus(
            peers[0],
            source,
            "tones",
            RtpTransceiverDirection::SendReceive,
        )
        .unwrap();
    let sender = session.sender(transceiver).unwrap();
    let receiver = session.receiver(transceiver).unwrap();
    assert!(session.sender_dtls_transport(sender).unwrap().is_none());
    let dtmf = session.sender_dtmf(sender).unwrap().unwrap();
    assert_eq!(session.sender_dtmf(sender).unwrap(), Some(dtmf));
    assert!(!session.dtmf_can_insert_tones(dtmf).unwrap());
    assert!(!session.insert_dtmf_tones(dtmf, "1", 100, 50).unwrap());
    assert_eq!(session.dtmf_duration_ms(dtmf).unwrap(), 100);
    assert_eq!(session.dtmf_inter_tone_gap_ms(dtmf).unwrap(), 50);
    assert_eq!(session.dtmf_comma_delay_ms(dtmf).unwrap(), 2000);
    assert!(matches!(
        session.insert_dtmf_tones(dtmf, "1", 40, i32::MAX),
        Err(SessionDtmfError::Input(DtmfInsertError::DelayOverflow))
    ));
    let channel_config = DataChannelConfiguration {
        negotiated: true,
        id: Some(0),
        ..Default::default()
    };
    let channels = [
        session
            .create_channel(peers[0], "transport", channel_config.clone())
            .unwrap(),
        session
            .create_channel(peers[1], "transport", channel_config)
            .unwrap(),
    ];
    let mut events = Events::default();
    let id = session.create_offer(peers[0], false).unwrap();
    let offer = events.finish(&mut session, peers, peers[0], id).unwrap();
    let offer = events.gather(&mut session, peers, peers[0], offer);
    assert!(offer.sdp.contains("telephone-event/48000"));
    let mid = offer
        .sdp
        .lines()
        .find_map(|line| line.strip_prefix("a=mid:"))
        .unwrap()
        .as_bytes()
        .to_vec();
    let sctp = session.peer_sctp_transport(peers[0]).unwrap().unwrap();
    assert_eq!(session.peer_sctp_transport(peers[0]).unwrap(), Some(sctp));
    let dtls = session.sctp_dtls_transport(sctp).unwrap().unwrap();
    // RTP endpoint transport association may still be absent before answer.
    if let Some(value) = session.sender_dtls_transport(sender).unwrap() {
        assert_eq!(value, dtls);
    }
    if let Some(value) = session.receiver_dtls_transport(receiver).unwrap() {
        assert_eq!(value, dtls);
    }
    assert_eq!(
        session.peer_dtls_transport_by_mid(peers[0], &mid).unwrap(),
        Some(dtls)
    );
    let ice = session.dtls_ice_transport(dtls).unwrap().unwrap();
    assert_eq!(session.dtls_ice_transport(dtls).unwrap(), Some(ice));
    assert_eq!(session.ice_component(ice).unwrap(), 1);
    let original_config = session.ice_config(ice).unwrap();
    let mut changed = original_config.clone();
    changed.receiving_timeout = Some(TimeDelta::from_native_microsecond_bits(7_000_000));
    changed.continual_gathering_policy = ContinualGatheringPolicy::Continually;
    session.set_ice_config(ice, &changed).unwrap();
    let applied = session.ice_config(ice).unwrap();
    assert_eq!(applied.receiving_timeout, changed.receiving_timeout);
    // The SDK refuses to change gathering policy after gathering has started.
    assert_eq!(
        applied.continual_gathering_policy,
        original_config.continual_gathering_policy
    );
    assert_ne!(
        applied.continual_gathering_policy,
        changed.continual_gathering_policy
    );
    let mut invalid = applied.clone();
    invalid.receiving_timeout = Some(TimeDelta::from_native_microsecond_bits(0));
    match session.set_ice_config(ice, &invalid) {
        Err(SessionTransportError::Operation(IceOperationError::Native(error))) => {
            assert_eq!(error.kind(), RtcErrorType::InvalidParameter);
            assert!(!error.message_bytes().is_empty());
        }
        other => panic!("expected native config rejection, got {other:?}"),
    }
    assert_eq!(session.ice_config(ice).unwrap(), applied);
    session.set_ice_config(ice, &original_config).unwrap();
    assert_eq!(
        session.ice_config(ice).unwrap().receiving_timeout,
        original_config.receiving_timeout
    );
    assert!(matches!(
        session.register_ice_packet_observer(ice, 0),
        Err(SessionTransportError::IceObserver(
            IcePacketObserverError::ZeroCapacity
        ))
    ));
    session.register_ice_packet_observer(ice, 64).unwrap();
    assert!(matches!(
        session.register_ice_packet_observer(ice, 1),
        Err(SessionTransportError::IceObserver(
            IcePacketObserverError::AlreadyRegistered
        ))
    ));
    session.register_dtls_observer(dtls, 1).unwrap();
    assert!(matches!(
        session.register_dtls_observer(dtls, 1),
        Err(SessionTransportError::Observer(
            TransportObserverError::AlreadyRegistered
        ))
    ));
    session.register_sctp_observer(sctp, 1).unwrap();
    assert!(matches!(
        session.register_sctp_observer(sctp, 0),
        Err(SessionTransportError::Observer(
            TransportObserverError::ZeroCapacity
        ))
    ));
    assert!(
        session
            .register_dtmf_observer(dtmf, NonZeroUsize::new(16).unwrap())
            .unwrap()
    );
    assert!(
        !session
            .register_dtmf_observer(dtmf, NonZeroUsize::new(16).unwrap())
            .unwrap()
    );

    let (session, info, connected_sctp) = thread::spawn(move || {
        let mut session = session;
        let id = session.set_remote_description(peers[1], offer).unwrap();
        events.finish(&mut session, peers, peers[1], id);
        let id = session.create_answer(peers[1]).unwrap();
        let answer = events.finish(&mut session, peers, peers[1], id).unwrap();
        assert!(answer.sdp.contains("telephone-event/48000"));
        let answer = events.gather(&mut session, peers, peers[1], answer);
        let id = session.set_remote_description(peers[0], answer).unwrap();
        events.finish(&mut session, peers, peers[0], id);
        let mut opened = HashSet::new();
        while opened.len() != channels.len() {
            events.drain(&mut session, peers);
            for channel in channels {
                while let Some(event) = session.try_channel_event(channel).unwrap() {
                    if event == DataChannelEvent::StateChanged(DataChannelState::Open) {
                        opened.insert(channel);
                    }
                }
            }
            if opened.len() != channels.len() {
                wait(&mut session);
            }
        }
        assert_eq!(session.sender_dtls_transport(sender).unwrap(), Some(dtls));
        assert_eq!(
            session.receiver_dtls_transport(receiver).unwrap(),
            Some(dtls)
        );
        let info = session.dtls_information(dtls).unwrap();
        assert_eq!(info.state(), DtlsTransportState::Connected);
        assert!(info.role().is_some());
        assert!(info.tls_version().is_some());
        assert!(info.ssl_cipher_suite().is_some());
        assert!(info.remote_ssl_certificates().unwrap().len() > 0);
        let connected_sctp = session.sctp_information(sctp).unwrap();
        assert_eq!(connected_sctp.state, SctpTransportState::Connected);
        assert_eq!(connected_sctp.dtls_transport, Some(dtls));
        assert!(connected_sctp.max_message_size.unwrap() > 0.0);
        assert!(connected_sctp.max_channels.unwrap() > 0);
        assert_eq!(
            session.ice_gathering_state(ice).unwrap(),
            IceTransportGatheringState::Complete
        );
        assert!(matches!(
            session.ice_role(ice).unwrap(),
            IceTransportRole::Controlling | IceTransportRole::Controlled
        ));
        assert!(matches!(
            session.ice_transport_state(ice).unwrap(),
            IceTransportState::Connected | IceTransportState::Completed
        ));
        let peer_sctp = session.peer_sctp_transport(peers[1]).unwrap().unwrap();
        let peer_dtls = session.sctp_dtls_transport(peer_sctp).unwrap().unwrap();
        let peer_ice = session.dtls_ice_transport(peer_dtls).unwrap().unwrap();
        session.register_ice_packet_observer(peer_ice, 64).unwrap();
        assert!(session.ice_packet_state(ice).unwrap().writable);
        assert!(session.ice_packet_state(peer_ice).unwrap().receiving);
        let pair = session.ice_selected_candidate_pair(ice).unwrap().unwrap();
        let before = session.ice_stats(ice).unwrap().unwrap();
        assert!(before.selected_candidate_pair_changes > 0);
        assert!(
            before
                .connection_infos
                .iter()
                .any(|v| v.best_connection && v.writable)
        );
        assert!(!before.candidate_stats.is_empty());
        let route = session.ice_network_route(ice).unwrap().unwrap();
        assert!(route.connected);
        assert!(route.packet_overhead > 0);
        events.drain(&mut session, peers);
        let pair_event = events
            .pairs
            .iter()
            .find(|(peer, _)| *peer == peers[0])
            .expect("native selected-pair notification");
        assert_eq!(
            pair_event.1.transport_name,
            session.ice_packet_state(ice).unwrap().transport_name
        );
        assert_eq!(
            pair_event
                .1
                .selected_candidate_pair
                .local
                .properties()
                .address,
            pair.local.properties().address
        );
        assert!(!pair_event.1.reason.is_empty());
        let local_sdp = session
            .descriptions(peers[0])
            .unwrap()
            .local()
            .unwrap()
            .sdp
            .clone();
        let credentials = IceParameters {
            username_fragment: local_sdp
                .lines()
                .find_map(|l| l.strip_prefix("a=ice-ufrag:"))
                .unwrap()
                .as_bytes()
                .to_vec(),
            password: local_sdp
                .lines()
                .find_map(|l| l.strip_prefix("a=ice-pwd:"))
                .unwrap()
                .as_bytes()
                .to_vec(),
            renomination: false,
        };
        let opaque = IceParameters {
            username_fragment: b"native\0\xff".to_vec(),
            password: b"secret\0\x80".to_vec(),
            renomination: true,
        };
        session.set_ice_parameters(ice, &opaque).unwrap();
        assert_eq!(
            session
                .ice_stats(ice)
                .unwrap()
                .unwrap()
                .ice_local_username_fragment,
            opaque.username_fragment
        );
        session.set_ice_parameters(ice, &credentials).unwrap();
        let peer_sdp = session
            .descriptions(peers[1])
            .unwrap()
            .local()
            .unwrap()
            .sdp
            .clone();
        let remote = IceParameters {
            username_fragment: peer_sdp
                .lines()
                .find_map(|l| l.strip_prefix("a=ice-ufrag:"))
                .unwrap()
                .as_bytes()
                .to_vec(),
            password: peer_sdp
                .lines()
                .find_map(|l| l.strip_prefix("a=ice-pwd:"))
                .unwrap()
                .as_bytes()
                .to_vec(),
            renomination: false,
        };
        session.set_remote_ice_parameters(ice, &remote).unwrap();
        session.set_remote_ice_mode(ice, IceMode::Full).unwrap();
        let role = session.ice_role(ice).unwrap();
        session.set_ice_role(ice, role).unwrap();
        assert!(matches!(
            session.set_ice_role(ice, IceTransportRole::Other(222)),
            Err(SessionTransportError::Operation(
                IceOperationError::UnknownRole(222)
            ))
        ));
        session.maybe_start_ice_gathering(ice).unwrap();
        assert_eq!(
            session.ice_gathering_state(ice).unwrap(),
            IceTransportGatheringState::Complete
        );
        session.add_remote_ice_candidate(ice, &pair.remote).unwrap();
        session
            .remove_remote_ice_candidate(ice, &pair.remote)
            .unwrap();
        session.add_remote_ice_candidate(ice, &pair.remote).unwrap();
        let option = session
            .set_ice_option(ice, IceSocketOption::Dscp, 0)
            .unwrap();
        assert_eq!(option.return_value, 0);
        assert_eq!(
            session.ice_option(ice, IceSocketOption::Dscp).unwrap(),
            Some(0)
        );
        let mut options = IcePacketOptions::default();
        options.packet_id = 734;
        options.info_signaled_after_sent.packet_type = PacketType::StunMessage;
        let packet = b"\xf1actual-ice\0\xff";
        let rejected = session.send_ice_packet(ice, packet, &options, 1).unwrap();
        assert_eq!(rejected.return_value, -1);
        assert_eq!(rejected.last_error, 22); // native Linux EINVAL, not a binding error
        assert!(matches!(
            session.send_ice_packet(ice, &[], &options, 0),
            Err(SessionTransportError::Packet(
                IcePacketSendError::EmptyPacket
            ))
        ));
        while session.take_ice_packet_event(ice).unwrap().is_some() {}
        while session.take_ice_packet_event(peer_ice).unwrap().is_some() {}
        let sent = session.send_ice_packet(ice, packet, &options, 0).unwrap();
        assert_eq!(sent.return_value, packet.len() as i32);
        let deadline = Instant::now() + Duration::from_secs(10);
        let (mut received, mut sent_metadata) = (false, false);
        while !received || !sent_metadata {
            while let Some(e) = session.take_ice_packet_event(peer_ice).unwrap() {
                if let IcePacketEvent::Received(value) = e {
                    if value.payload == packet {
                        assert_eq!(value.source, pair.local.properties().address);
                        assert_eq!(value.decryption, IcePacketDecryption::NotDecrypted);
                        received = true;
                    }
                }
            }
            while let Some(e) = session.take_ice_packet_event(ice).unwrap() {
                if let IcePacketEvent::Sent(value) = e {
                    if value.packet_id == 734 {
                        assert_eq!(
                            value.info.packet_type,
                            PacketType::Data,
                            "native ICE overwrites caller metadata"
                        );
                        assert!(value.send_time_ms >= 0);
                        sent_metadata = true;
                    }
                }
            }
            assert!(
                Instant::now() < deadline,
                "native raw packet/callback did not arrive"
            );
            if !received || !sent_metadata {
                wait(&mut session);
            }
        }
        let after = session.ice_stats(ice).unwrap().unwrap();
        assert!(after.bytes_sent >= before.bytes_sent + packet.len() as u64);
        assert!(after.packets_sent > before.packets_sent);
        session.unregister_ice_packet_observer(peer_ice).unwrap();
        session.register_ice_packet_observer(peer_ice, 1).unwrap();
        let received_before = session
            .ice_stats(peer_ice)
            .unwrap()
            .unwrap()
            .packets_received;
        for _ in 0..4 {
            assert_eq!(
                session
                    .send_ice_packet(ice, packet, &options, 0)
                    .unwrap()
                    .return_value,
                packet.len() as i32
            );
        }
        loop {
            if session
                .ice_stats(peer_ice)
                .unwrap()
                .unwrap()
                .packets_received
                >= received_before + 4
            {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        assert!(session.take_dropped_ice_packet_events(peer_ice).unwrap() >= 3);
        assert_eq!(session.take_dropped_ice_packet_events(peer_ice).unwrap(), 0);
        session.remove_all_remote_ice_candidates(ice).unwrap();
        assert!(
            session.ice_packet_state(ice).unwrap().writable,
            "native candidate-cache removal is not connection destruction"
        );
        let _native_optional_rtt = session.ice_rtt_estimate_ms(ice).unwrap();
        let _native_internal_state = session.ice_internal_state(ice).unwrap();
        assert!(session.dtmf_can_insert_tones(dtmf).unwrap());
        assert!(
            session
                .insert_dtmf_tones_with_comma_delay(dtmf, "a,x#", 40, 30, 30)
                .unwrap()
        );
        let mut tones = Vec::new();
        while tones.len() < 8 {
            while let Some(event) = session.take_dtmf_tone_change(dtmf).unwrap() {
                tones.push((event.tone, event.remaining_tones));
            }
            if tones.len() < 8 {
                wait(&mut session);
            }
        }
        assert_eq!(
            tones,
            vec![
                ("a".into(), Some(",x#".into())),
                ("a".into(), None),
                (",".into(), Some("x#".into())),
                (",".into(), None),
                ("#".into(), Some("".into())),
                ("#".into(), None),
                ("".into(), Some("".into())),
                ("".into(), None)
            ]
        );
        assert_eq!(session.dtmf_tones(dtmf).unwrap(), "");
        assert_eq!(session.dtmf_duration_ms(dtmf).unwrap(), 40);
        assert_eq!(session.dtmf_inter_tone_gap_ms(dtmf).unwrap(), 30);
        assert_eq!(session.dtmf_comma_delay_ms(dtmf).unwrap(), 30);
        assert_eq!(session.take_dropped_dtmf_tone_changes(dtmf).unwrap(), 0);
        session.unregister_dtmf_observer(dtmf).unwrap();
        assert!(
            session
                .register_dtmf_observer(dtmf, NonZeroUsize::new(1).unwrap())
                .unwrap()
        );
        assert!(session.insert_dtmf_tones(dtmf, "", 40, 30).unwrap());
        loop {
            let dropped = session.take_dropped_dtmf_tone_changes(dtmf).unwrap();
            if dropped != 0 {
                assert_eq!(dropped, 1);
                break;
            }
            wait(&mut session);
        }
        assert_eq!(session.dtmf_comma_delay_ms(dtmf).unwrap(), 2000);
        session.clear_stats_cache(peers[0]).unwrap();
        let stats_id = session.request_sender_stats(sender).unwrap();
        assert!(
            session.request_sender_stats(sender).is_err(),
            "native report result retains admission"
        );
        let sender_stats = events.stats(&mut session, peers, peers[0], stats_id);
        assert!(
            sender_stats
                .report
                .of_class(RtcStatsClass::DataChannelStats)
                .is_empty(),
            "sender selection is not the complete peer report"
        );
        assert!(
            !sender_stats
                .report
                .of_class(RtcStatsClass::OutboundRtpStreamStats)
                .is_empty()
        );
        let stats_id = session.request_receiver_stats(receiver).unwrap();
        let receiver_stats = events.stats(&mut session, peers, peers[0], stats_id);
        assert!(
            receiver_stats
                .report
                .of_class(RtcStatsClass::DataChannelStats)
                .is_empty()
        );
        let id = session
            .request_stats_with_sender_selector(peers[0], None)
            .unwrap();
        assert!(
            events
                .stats(&mut session, peers, peers[0], id)
                .report
                .is_empty()
        );
        let id = session
            .request_stats_with_receiver_selector(peers[0], None)
            .unwrap();
        assert!(
            events
                .stats(&mut session, peers, peers[0], id)
                .report
                .is_empty()
        );
        let id = session
            .request_stats_with_sender_selector(peers[1], Some(sender))
            .unwrap();
        assert!(
            events
                .stats(&mut session, peers, peers[1], id)
                .report
                .is_empty()
        );
        let id = session
            .request_stats_with_receiver_selector(peers[1], Some(receiver))
            .unwrap();
        assert!(
            events
                .stats(&mut session, peers, peers[1], id)
                .report
                .is_empty()
        );
        // Keep this native notification and transport transitions unconsumed
        // across migration and peer close.
        (session, info, connected_sctp)
    })
    .join()
    .unwrap();

    let (info, connected_sctp) = thread::spawn(move || {
        let mut session = session;
        let mut foreign = ProductionSession::new(ProductionSessionConfig::default()).unwrap();
        assert!(matches!(
            foreign.ice_component(ice),
            Err(SessionTransportError::Resource(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                ..
            }))
        ));
        assert_eq!(
            foreign.dtls_information(dtls).err().unwrap().kind,
            PeerErrorKind::InvalidParameter
        );
        assert_eq!(
            foreign.sctp_information(sctp).unwrap_err().kind,
            PeerErrorKind::InvalidParameter
        );
        assert_eq!(
            foreign.dtmf_can_insert_tones(dtmf).unwrap_err().kind,
            PeerErrorKind::InvalidParameter
        );
        drop(foreign);
        session.close_peer(peers[0]).unwrap();
        assert_eq!(
            session.take_dtmf_tone_change(dtmf).unwrap().unwrap(),
            DtmfToneChange {
                tone: "".into(),
                remaining_tones: None
            }
        );
        assert!(!session.dtmf_can_insert_tones(dtmf).unwrap());
        assert!(!session.insert_dtmf_tones(dtmf, "1", 100, 50).unwrap());
        assert!(matches!(
            session.ice_component(ice),
            Err(SessionTransportError::InternalUnavailable(_))
        ));
        assert!(matches!(
            session.ice_internal_state(ice),
            Err(SessionTransportError::InternalUnavailable(_))
        ));
        assert!(matches!(
            session.ice_config(ice),
            Err(SessionTransportError::InternalUnavailable(_))
        ));
        assert!(matches!(
            session.set_ice_config(ice, &IceConfig::default()),
            Err(SessionTransportError::Operation(
                IceOperationError::Unavailable(_)
            ))
        ));
        assert!(matches!(
            session.ice_stats(ice),
            Err(SessionTransportError::InternalUnavailable(_))
        ));
        assert!(matches!(
            session.ice_selected_candidate_pair(ice),
            Err(SessionTransportError::InternalUnavailable(_))
        ));
        assert!(matches!(
            session.send_ice_packet(ice, b"x", &IcePacketOptions::default(), 0),
            Err(SessionTransportError::Packet(
                IcePacketSendError::Unavailable(_)
            ))
        ));
        assert!(matches!(
            session.ice_option(ice, IceSocketOption::Dscp),
            Err(SessionTransportError::InternalUnavailable(_))
        ));
        assert!(matches!(
            session.unregister_ice_packet_observer(ice),
            Err(SessionTransportError::InternalUnavailable(_))
        ));
        assert!(session.take_ice_packet_event(ice).unwrap().is_none());
        assert_eq!(
            session.dtls_information(dtls).unwrap().state(),
            DtlsTransportState::Closed
        );
        let closed_sctp = session.sctp_information(sctp).unwrap();
        assert_eq!(closed_sctp.state, SctpTransportState::Closed);
        assert_eq!(closed_sctp.dtls_transport, None);
        // Pinned SCTP clears its DTLS reference but retains negotiated limits.
        assert_eq!(
            closed_sctp.max_message_size,
            connected_sctp.max_message_size
        );
        assert_eq!(closed_sctp.max_channels, connected_sctp.max_channels);
        assert_eq!(
            session.take_sctp_event(sctp).unwrap().unwrap().state,
            SctpTransportState::Closed
        );
        match session.take_dtls_event(dtls).unwrap().unwrap() {
            DtlsTransportEvent::StateChange(info) => {
                assert_eq!(info.state(), DtlsTransportState::Closed);
                assert!(info.remote_ssl_certificates().is_none());
                assert_eq!(info.role(), None);
            }
            DtlsTransportEvent::Error(error) => panic!("unexpected native DTLS failure: {error}"),
        }
        assert!(session.take_dtls_dropped_events(dtls).unwrap() > 0);
        assert!(session.take_sctp_dropped_events(sctp).unwrap() > 0);
        assert_eq!(session.take_dtls_dropped_events(dtls).unwrap(), 0);
        assert_eq!(session.take_sctp_dropped_events(sctp).unwrap(), 0);
        assert_eq!(session.dtls_ice_transport(dtls).unwrap(), Some(ice));
        session.unregister_dtls_observer(dtls).unwrap();
        session.unregister_sctp_observer(sctp).unwrap();
        session.release_dtmf_sender(dtmf).unwrap();
        session.release_ice_transport(ice).unwrap();
        session.release_sctp_transport(sctp).unwrap();
        session.release_dtls_transport(dtls).unwrap();
        assert!(matches!(
            session.ice_component(ice),
            Err(SessionTransportError::Resource(_))
        ));
        session.shutdown().unwrap();
        drop(session);
        (info, connected_sctp)
    })
    .join()
    .unwrap();
    // These are independent callback/snapshot/certificate values, not actor
    // handles. They remain usable after native role threads have been destroyed.
    assert_eq!(info.state(), DtlsTransportState::Connected);
    assert!(
        info.remote_certificate_data()
            .unwrap()
            .iter()
            .all(|certificate| !certificate.der.is_empty())
    );
    assert!(info.remote_ssl_certificates().unwrap().len() > 0);
    assert_eq!(connected_sctp.state, SctpTransportState::Connected);
    assert!(!connected_sctp.native_string().is_empty());
}

fn received_rtx(
    session: &mut ProductionSession,
    ice: SessionIceTransportId,
    types: &HashSet<u8>,
) -> usize {
    let mut count = 0;
    while let Some(event) = session.take_ice_packet_event(ice).unwrap() {
        if let IcePacketEvent::Received(packet) = event {
            if packet.payload.len() > 12
                && packet.payload[0] >> 6 == 2
                && types.contains(&(packet.payload[1] & 0x7f))
            {
                count += 1;
            }
        }
    }
    count
}

#[test]
fn migrated_actor_reconfigures_real_padding_probes_and_replaces_audio_tracks() {
    let mut session = ProductionSession::builder(ProductionSessionConfig {
        video_format: Some(VideoCodecFormat::new("VP8")),
    })
    .encoded_audio()
    .build()
    .unwrap();
    let peers = [
        session.create_peer(Default::default()).unwrap(),
        session.create_peer(Default::default()).unwrap(),
    ];
    let video = session.create_video_source().unwrap();
    session
        .publish_video(
            peers[0],
            video,
            "probe-without-media",
            RtpTransceiverDirection::SendOnly,
            &[],
        )
        .unwrap();
    let old = session.create_opus_source(1).unwrap();
    let tx = session
        .publish_opus(
            peers[0],
            old,
            "before-audio",
            RtpTransceiverDirection::SendOnly,
        )
        .unwrap();
    let sender = session.sender(tx).unwrap();
    assert_eq!(session.sender(tx).unwrap(), sender);
    session
        .reconfigure_bandwidth_estimation(peers[0], Default::default())
        .unwrap();
    let mut events = Events::default();
    let id = session.create_offer(peers[0], false).unwrap();
    let offer = events.finish(&mut session, peers, peers[0], id).unwrap();
    let rtx: HashSet<u8> = offer
        .sdp
        .lines()
        .filter_map(|line| {
            let line = line.strip_prefix("a=rtpmap:")?;
            let (payload, codec) = line.split_once(' ')?;
            codec.starts_with("rtx/").then(|| payload.parse().unwrap())
        })
        .collect();
    assert!(!rtx.is_empty(), "fixture requires actual negotiated RTX");
    let offer = events.gather(&mut session, peers, peers[0], offer);
    thread::spawn(move || {
        let id = session.set_remote_description(peers[1], offer).unwrap();
        events.finish(&mut session, peers, peers[1], id);
        let id = session.create_answer(peers[1]).unwrap();
        let answer = events.finish(&mut session, peers, peers[1], id).unwrap();
        let answer = events.gather(&mut session, peers, peers[1], answer);
        let id = session.set_remote_description(peers[0], answer).unwrap();
        events.finish(&mut session, peers, peers[0], id);
        let deadline = Instant::now() + Duration::from_secs(15);
        while peers
            .iter()
            .any(|p| session.peer_connection_state(*p).unwrap() != ConnectionState::Connected)
        {
            events.drain(&mut session, peers);
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        events.drain(&mut session, peers);
        let audio = events
            .receivers
            .iter()
            .find(|(p, r)| {
                *p == peers[1] && session.receiver_media_type(*r).unwrap() == MediaType::Audio
            })
            .unwrap()
            .1;
        let video_receiver = events
            .receivers
            .iter()
            .find(|(p, r)| {
                *p == peers[1] && session.receiver_media_type(*r).unwrap() == MediaType::Video
            })
            .unwrap()
            .1;
        let dtls = session
            .receiver_dtls_transport(video_receiver)
            .unwrap()
            .unwrap();
        let ice = session.dtls_ice_transport(dtls).unwrap().unwrap();
        session.register_ice_packet_observer(ice, 2048).unwrap();
        session
            .reconfigure_bandwidth_estimation(peers[0], Default::default())
            .unwrap();
        let quiet = Instant::now() + Duration::from_millis(150);
        while Instant::now() < quiet {
            assert_eq!(
                received_rtx(&mut session, ice, &rtx),
                0,
                "default unexpectedly probed without any media"
            );
            thread::sleep(Duration::from_millis(1));
        }
        session
            .reconfigure_bandwidth_estimation(
                peers[0],
                BandwidthEstimationSettings {
                    allow_probe_without_media: true,
                },
            )
            .unwrap();
        loop {
            if received_rtx(&mut session, ice, &rtx) > 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "native reconfiguration did not emit RTX padding before media"
            );
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(session.take_dropped_ice_packet_events(ice).unwrap(), 0);
        session.attach_encoded_audio(audio).unwrap();
        let first = OpusInputFrame {
            data: include_bytes!("fixtures/simulation-tone-440.opus").to_vec(),
            rtp_timestamp: 0,
            samples_per_channel: 960,
        };
        session.push_opus(old, &first).unwrap();
        loop {
            if let Some(frame) = session.try_audio_frame(audio).unwrap() {
                assert_eq!(frame.data, first.data);
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        let replacement = session.create_opus_source(1).unwrap();
        let track = session
            .create_opus_track(peers[0], replacement, "after-audio")
            .unwrap();
        assert!(session.set_sender_track(sender, Some(track)).unwrap());
        assert_eq!(session.sender_track_reference(sender).unwrap(), Some(track));
        let wrong = session
            .create_video_track(peers[0], video, "wrong-kind")
            .unwrap();
        assert!(
            !session.set_sender_track(sender, Some(wrong)).unwrap(),
            "SDK kind mismatch must remain a false result"
        );
        assert_eq!(session.sender_track_reference(sender).unwrap(), Some(track));
        session.release_media_track_reference(track).unwrap();
        session.close_source(old).unwrap();
        let next = OpusInputFrame {
            data: include_bytes!("fixtures/simulation-tone-880.opus").to_vec(),
            rtp_timestamp: 960,
            ..first.clone()
        };
        assert_ne!(first.data, next.data);
        session.push_opus(replacement, &next).unwrap();
        loop {
            if let Some(frame) = session.try_audio_frame(audio).unwrap() {
                assert_eq!(frame.data, next.data);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "replacement source never reached native transport"
            );
            thread::sleep(Duration::from_millis(1));
        }
        assert!(session.set_sender_track(sender, None).unwrap());
        assert!(session.sender_track_reference(sender).unwrap().is_none());
        let detached = OpusInputFrame {
            rtp_timestamp: 1920,
            ..first
        };
        // The existing producer boundary rejects input once SDK SetTrack
        // detaches its sink. This is native backpressure, not accepted media.
        let rejected = session.push_opus(replacement, &detached).unwrap_err();
        assert_eq!(rejected.message, "Backpressure");
        let quiet = Instant::now() + Duration::from_millis(150);
        while Instant::now() < quiet {
            assert!(
                session.try_audio_frame(audio).unwrap().is_none(),
                "detached native source still delivered media"
            );
            thread::sleep(Duration::from_millis(1));
        }
        session.unregister_ice_packet_observer(ice).unwrap();
        session.close_peer(peers[0]).unwrap();
        assert_eq!(
            session.set_sender_track(sender, None).unwrap_err().kind,
            PeerErrorKind::Closed
        );
        assert_eq!(
            session
                .reconfigure_bandwidth_estimation(peers[0], Default::default())
                .unwrap_err()
                .kind,
            PeerErrorKind::Closed
        );
        session.shutdown().unwrap();
    })
    .join()
    .unwrap();
}
