use super::{Counters, Pair};
use pulsebeam_webrtc_sys::{
    PeerErrorKind, VideoFrame, VideoSinkAggregates, VideoSinkFrameSize, VideoSinkWants,
};
use std::sync::Arc;

#[test]
fn actual_sink_wants_preserve_fields_rewrite_track_black_and_reject_atomically() {
    let pair = Pair::new(Arc::new(Counters::default()), false);
    let mut source = pair.alice_factory.create_video_source().unwrap();
    let track = pair
        .alice_factory
        .create_video_track("wants", &source)
        .unwrap();
    let view = source.as_view();
    let defaults = VideoSinkWants::default();
    assert_eq!(defaults.max_pixel_count, i32::MAX);
    assert_eq!(defaults.max_framerate_fps, i32::MAX);
    assert_eq!(defaults.resolution_alignment, 1);
    assert_eq!(defaults.target_pixel_count, None);
    assert_eq!(defaults.requested_resolution, None);
    assert_eq!(defaults.aggregates, None);

    let mut wanted = VideoSinkWants {
        rotation_applied: true,
        black_frames: true,
        max_pixel_count: 9999,
        target_pixel_count: Some(0),
        max_framerate_fps: 17,
        resolution_alignment: 6,
        resolutions: vec![
            VideoSinkFrameSize {
                width: 8,
                height: 6,
            },
            VideoSinkFrameSize {
                width: 4,
                height: 2,
            },
        ],
        requested_resolution: Some(VideoSinkFrameSize {
            width: 12,
            height: 10,
        }),
        is_active: true,
        aggregates: Some(VideoSinkAggregates {
            any_active_without_requested_resolution: true,
        }),
    };
    let sink = track.attach_sink_with_wants(&wanted).unwrap();
    let mut alias = sink.clone();
    let mut effective = wanted.clone();
    effective.black_frames = false; // Actual SDK track override, not our input.
    assert_eq!(sink.wants(), Some(effective.clone()));

    let mut direct_wants = VideoSinkWants {
        resolution_alignment: 10,
        aggregates: Some(VideoSinkAggregates::default()),
        ..defaults.clone()
    };
    let direct = view.attach_sink_with_wants(&direct_wants).unwrap();
    let mut direct_alias = direct.clone();
    assert_eq!(direct.wants(), Some(direct_wants.clone()));
    let frame = VideoFrame::i420(4, 4, vec![53; 24], 123_456, 0x1234).unwrap();
    source.push_frame(&frame).unwrap();
    assert_eq!(alias.try_next_frame(), Some(frame.clone()));
    assert_eq!(direct.try_next_frame(), Some(frame.clone()));

    assert!(track.set_enabled(false));
    assert_eq!(sink.wants(), Some(wanted.clone()));
    assert_eq!(direct.wants(), Some(direct_wants.clone()));
    source.push_frame(&frame).unwrap();
    assert_ne!(
        sink.try_next_frame().unwrap().buffer.as_bytes(),
        frame.buffer.as_bytes()
    );
    assert_eq!(direct.try_next_frame(), Some(frame));

    let before = sink.wants();
    wanted.resolution_alignment = i32::MAX;
    assert_eq!(
        sink.update_wants(&wanted).unwrap_err().kind,
        PeerErrorKind::InvalidParameter
    );
    assert_eq!(alias.wants(), before);
    let bad = VideoSinkWants {
        resolution_alignment: i32::MIN,
        ..defaults.clone()
    };
    assert!(view.attach_sink_with_wants(&bad).is_err());
    assert_eq!(view.rejected_sink_requests(), Some(2));
    assert_eq!(track.source().unwrap().rejected_sink_requests(), Some(2));

    // Zero and non-MIN negative alignments are valid in the reached native
    // arithmetic. Preserve them rather than imposing a power-of-two policy.
    direct_wants.resolution_alignment = -10;
    direct.update_wants(&direct_wants).unwrap();
    assert_eq!(direct.wants(), Some(direct_wants));
    effective.resolution_alignment = 0;
    effective.black_frames = true;
    alias.update_wants(&effective).unwrap();
    assert_eq!(sink.wants(), Some(effective));

    drop(track);
    drop(view);
    source.close().unwrap();
    drop(source);
    drop(sink);
    drop(direct);
    assert!(alias.wants().is_some());
    assert!(direct_alias.wants().is_some());
    alias.close().unwrap();
    alias.close().unwrap();
    assert_eq!(alias.wants(), None);
    assert_eq!(
        alias.update_wants(&defaults).unwrap_err().kind,
        PeerErrorKind::Closed
    );
    direct_alias.close().unwrap();
    assert_eq!(direct_alias.wants(), None);
}
