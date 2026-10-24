use pulsebeam_webrtc_sys::*;
use std::{
    num::NonZeroUsize,
    time::{Duration, Instant},
};

fn finish(peer: &PeerConnection, id: OperationId) -> Option<SessionDescriptionBytes> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match peer.try_next_event() {
            Some(PeerConnectionEvent::OperationComplete(done)) if done.operation_id == id => {
                return done.result.unwrap().map(Into::into);
            }
            Some(PeerConnectionEvent::OperationCompleteBytes {
                operation_id,
                description,
            }) if operation_id == id => return Some(description),
            Some(PeerConnectionEvent::OperationFailedBytes {
                operation_id,
                error,
            }) if operation_id == id => panic!("native SDP failure: {error:?}"),
            _ => {}
        }
        assert!(Instant::now() < deadline, "SDP callback timeout");
        std::thread::yield_now();
    }
}
fn negotiate(left: &PeerConnection, right: &PeerConnection) {
    let offer = finish(left, left.create_offer().unwrap()).unwrap();
    finish(
        left,
        left.set_local_description_bytes(offer.clone()).unwrap(),
    );
    finish(right, right.set_remote_description_bytes(offer).unwrap());
    let answer = finish(right, right.create_answer().unwrap()).unwrap();
    finish(
        right,
        right.set_local_description_bytes(answer.clone()).unwrap(),
    );
    finish(left, left.set_remote_description_bytes(answer).unwrap());
}

#[test]
fn owning_source_reads_and_explicit_notifiers_follow_real_sdk_changes() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let input = factory.create_audio_source().unwrap();
    let audio = factory.create_audio_track("source-audio", &input).unwrap();
    let track = audio.as_media_track();
    let observer = track.observe(NonZeroUsize::new(1).unwrap());
    let alias = track.clone();
    let MediaTrackSource::Audio(source) = track.source().unwrap() else {
        panic!()
    };
    let MediaTrackSource::Audio(source_alias) = alias.source().unwrap() else {
        panic!()
    };
    assert!(source.same_resource(&source_alias));
    assert!(!source.remote());
    assert_eq!(source.state(), MediaSourceState::Live);
    assert_eq!(track.audio_volume(), Some(1.0));
    assert_eq!(track.signal_level(), None);
    assert!(track.audio_processor().is_none());
    assert_eq!(track.should_receive(), None);
    assert_eq!(track.content_hint(), None);
    assert_eq!(source.options().echo_cancellation, None);
    audio
        .set_audio_processing_options(AudioProcessingOptions {
            echo_cancellation: ProcessingChoice::Software,
            noise_suppression: ProcessingChoice::Disabled,
            gain_control: ProcessingChoice::Automatic,
        })
        .unwrap();
    let options = source.options();
    assert_eq!(options.echo_cancellation, Some(true));
    assert_eq!(
        options.echo_cancellation_mode,
        Some(AudioProcessingMode::Software)
    );
    assert_eq!(options.noise_suppression, Some(false));
    assert_eq!(options.auto_gain_control, Some(true));
    assert_eq!(
        options.auto_gain_control_mode,
        Some(AudioProcessingMode::Automatic)
    );
    assert_eq!(options.audio_network_adaptor_config, None);
    assert!(
        observer.take_event().is_some(),
        "SDK audio options must notify the common track observer"
    );
    assert!(observer.take_event().is_none());
    let outcome = alias
        .set_audio_options(&AudioSourceOptions {
            highpass_filter: Some(true),
            highpass_filter_mode: Some(AudioProcessingMode::Software),
            stereo_swapping: Some(true),
            audio_network_adaptor_config: Some(b"unused-\xff\0".to_vec()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(outcome.code, TrackAudioOptionsCode::Stored);
    assert!(outcome.ok());
    assert!(outcome.message.is_empty());
    assert_eq!(source.options().highpass_filter, Some(true));
    assert_eq!(
        source.options().highpass_filter_mode,
        Some(AudioProcessingMode::Software)
    );
    assert_eq!(source.options().echo_cancellation, Some(true)); // Native SetAll merges the consumed subset.
    assert_eq!(source.options().stereo_swapping, None); // Native AudioTrack deliberately ignores unrelated options.
    assert_eq!(source.options().audio_network_adaptor_config, None);
    assert!(observer.take_event().is_some());
    assert!(observer.take_event().is_none());
    alias.set_audio_volume(2.5).unwrap();
    assert_eq!(alias.audio_volume(), Some(1.0)); // This SDK local source inherits the no-op volume setter.
    for enabled in [false, true, false] {
        assert!(alias.set_enabled(enabled));
    }
    assert!(observer.take_event().is_some());
    assert!(observer.take_event().is_none());
    assert_eq!(observer.take_dropped_events(), 2);
    assert_eq!(observer.take_dropped_events(), 0);
    drop(alias);
    drop(track);
    drop(audio);
    drop(input);
    assert_eq!(source.state(), MediaSourceState::Live);
    drop(observer);
    drop(source_alias);

    let mut video_input = factory.create_video_source().unwrap();
    let video = factory
        .create_video_track("source-video", &video_input)
        .unwrap()
        .as_media_track();
    let MediaTrackSource::Video(video_source) = video.source().unwrap() else {
        panic!()
    };
    video.set_should_receive(false).unwrap();
    assert_eq!(video.should_receive(), Some(false));
    video.set_should_receive(true).unwrap();
    video.set_content_hint(VideoContentHint::Text).unwrap();
    assert_eq!(video.content_hint(), Some(VideoContentHint::Text));
    video.set_content_hint(VideoContentHint::None).unwrap();
    let changes = video.observe(NonZeroUsize::new(2).unwrap());
    assert_eq!(video.should_receive(), Some(true));
    assert_eq!(video.content_hint(), Some(VideoContentHint::None));
    assert_eq!(video.audio_volume(), None);
    assert_eq!(video_source.state(), MediaSourceState::Live);
    assert!(!video_source.remote());
    let info = video_source.info();
    assert!(!info.is_screencast);
    assert!(!info.supports_encoded_output);
    assert_eq!(info.needs_denoising, None);
    assert_eq!(info.stats, None); // Built-in PushVideoSource inherits the SDK's unsupported stats read.
    video_input.close().unwrap();
    assert_eq!(video_source.state(), MediaSourceState::Ended);
    assert_eq!(video.state(), MediaTrackState::Ended);
    assert!(
        changes.take_event().is_some(),
        "real source state change did not reach SDK track notifier"
    );
    drop(video);
    drop(video_input);
    drop(factory);
    assert_eq!(video_source.state(), MediaSourceState::Ended);
    assert_eq!(source.state(), MediaSourceState::Live);
    drop(changes);
}

#[test]
fn receiver_streams_preserve_native_bytes_traversal_changes_and_retained_lifetimes() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let mut left = factory.create_peer_connection(Default::default()).unwrap();
    let mut right = factory.create_peer_connection(Default::default()).unwrap();
    let input = factory.create_audio_source().unwrap();
    let audio = factory.create_audio_track("stream-audio", &input).unwrap();
    let tx = left
        .add_audio_transceiver(&audio, RtpTransceiverDirection::SendOnly)
        .unwrap();
    let sender = tx.sender();
    let raw_id = b"stream-\xff".to_vec();
    sender.set_stream_id_bytes(&[raw_id.clone()]);
    negotiate(&left, &right);
    let receiver = right.audio_receivers().unwrap().remove(0);
    assert_eq!(receiver.media_type(), MediaType::Audio);
    assert_eq!(receiver.stream_id_bytes(), vec![raw_id.clone()]);
    let streams = receiver.streams();
    assert_eq!(streams.len(), 1);
    let stream = streams[0].clone();
    assert!(stream.same_resource(&receiver.streams()[0]));
    assert_eq!(stream.id(), raw_id);
    assert!(stream.video_tracks().is_empty());
    let received = receiver.track_reference().unwrap();
    assert!(received.same_resource(&stream.audio_tracks()[0]));
    assert!(received.same_resource(&stream.find_audio_track(&received.id()).unwrap()));
    assert!(stream.find_audio_track(b"absent").is_none());
    assert!(stream.find_video_track(&received.id()).is_none());
    received.set_audio_volume(2.75).unwrap();
    assert_eq!(received.audio_volume(), Some(2.75));
    let rejected = received
        .set_audio_options(&AudioSourceOptions::default())
        .unwrap();
    assert_eq!(rejected.code, TrackAudioOptionsCode::RejectedRemoteTrack);
    assert!(!rejected.ok());
    assert!(!rejected.message.is_empty());
    let source = received.source().unwrap();
    assert!(source.remote());
    assert_eq!(source.state(), MediaSourceState::Live);
    // Install the actual receiver track, not a new local track with its ID.
    // MediaTrack retains native source/factory ownership without a PeerInner
    // edge back to the receiving peer.
    let mut relay = factory.create_peer_connection(Default::default()).unwrap();
    let mut forward_init = RtpTransceiverInit::default();
    forward_init.set_direction(RtpTransceiverDirectionValue::SendOnly);
    let forwarded = relay
        .add_media_transceiver_with_init(&received, &forward_init)
        .unwrap();
    let forwarded_sender = forwarded.sender();
    assert!(
        forwarded_sender
            .track_reference()
            .unwrap()
            .same_resource(&received)
    );
    let notifications = stream.observe(NonZeroUsize::new(2).unwrap());
    sender.set_stream_id_bytes(&[b"replacement-stream".to_vec()]);
    negotiate(&left, &right);
    assert_eq!(
        receiver.stream_id_bytes(),
        vec![b"replacement-stream".to_vec()]
    );
    assert!(
        notifications.take_event().is_some(),
        "SDK stream removal did not notify retained old stream"
    );
    assert!(stream.audio_tracks().is_empty());
    assert_eq!(stream.id(), raw_id);
    assert_eq!(receiver.id_bytes(), receiver.id().into_bytes());
    left.close().unwrap();
    right.close().unwrap();
    assert_eq!(received.state(), MediaTrackState::Ended);
    assert_eq!(source.state(), MediaSourceState::Ended); // AudioRtpReceiver::Stop explicitly ends its source.
    drop(receiver);
    drop(streams);
    drop(sender);
    drop(tx);
    drop(left);
    drop(right);
    drop(audio);
    drop(input);
    drop(factory);
    assert_eq!(stream.id(), raw_id);
    assert!(stream.audio_tracks().is_empty());
    assert!(source.remote());
    assert_eq!(source.state(), MediaSourceState::Ended);
    drop(received);
    let retained = forwarded_sender.track_reference().unwrap();
    assert_eq!(retained.id().as_slice(), b"stream-audio");
    assert_eq!(retained.state(), MediaTrackState::Ended);
    relay.close().unwrap();
    drop(forwarded_sender);
    drop(forwarded);
    drop(relay);
    assert_eq!(retained.state(), MediaTrackState::Ended);
    drop(notifications);
}
