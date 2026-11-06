use super::*;

#[test]
fn pressure_and_terminal_failure_are_counted_once_with_bounded_labels() {
    let recorder = crate::shard::recorder::ShardRecorder::new();
    metrics::with_local_recorder(&recorder, || {
        let (sender, _ready) = mailbox::bounded(NonZeroUsize::MIN);
        let (controller, endpoint) = pair(ConnectionId::new(), sender);
        let at = Instant::now();
        let output = |sequence| SignalingOutput {
            sequence,
            cid: ChannelId::from_value(0),
            bytes: vec![1],
            deadline: at,
        };
        assert!(controller.output(output(1)).is_ok());
        assert!(controller.output(output(2)).is_err());
        for _ in 0..INPUT_FRAMES {
            assert!(endpoint.input([1]));
        }
        assert!(!endpoint.input([2]));
        assert!(!endpoint.input([3]));
    });
    let report = recorder.snapshot(crate::id::ShardId::new(0));
    let schema = report.schema.unwrap();
    let value = |name: &str, lane: &str| {
        schema
            .counters
            .iter()
            .zip(&report.counters)
            .find_map(|(series, value)| {
                (series.name == name && series.labels == [("lane".to_owned(), lane.to_owned())])
                    .then_some(*value)
            })
    };
    assert_eq!(
        value("native_signaling_handoff_pressure", "output"),
        Some(1)
    );
    assert_eq!(value("native_signaling_handoff_failure", "input"), Some(1));
    assert_eq!(schema.counters.len(), 2);
}

#[test]
fn payload_pressure_preserves_fifo_independent_progress_and_connection_local_failure() {
    let (ready, mut notices) = mailbox::bounded(const { NonZeroUsize::new(2).unwrap() });
    let connection = ConnectionId::new();
    let (mut controller, mut endpoint) = pair(connection, ready.clone());
    let at = Instant::now();
    let channel = ChannelId::from_value(0);
    assert!(endpoint.opened(Opened { channel, at }));
    assert!(endpoint.opened(Opened {
        channel,
        at: at + std::time::Duration::from_secs(10)
    }));
    for index in 0..INPUT_FRAMES {
        assert!(endpoint.input(vec![
            u8::try_from(index).unwrap();
            pulsebeam_proto::state::codec::MAX_FRAME_SIZE
        ]));
    }
    assert!(endpoint.fact(Fact::Resource(ResourceObservation::Activity {
        binding: 1,
        active: false
    })));
    let progress = SignalingProgress {
        sequence: 1,
        cid: channel,
        at,
    };
    assert!(endpoint.progress(progress));
    assert_eq!(notices.try_recv().unwrap(), connection);
    assert!(matches!(
        notices.try_recv(),
        Err(mailbox::TryRecvError::Empty)
    ));
    assert_eq!(controller.opened().unwrap().at, at);
    assert_eq!(controller.progress().unwrap().at, at);
    assert!(matches!(
        controller.fact(),
        Some(Fact::Resource(ResourceObservation::Activity {
            binding: 1,
            active: false
        }))
    ));
    assert!(controller.rearm_for(false));
    assert!(matches!(
        notices.try_recv(),
        Err(mailbox::TryRecvError::Empty)
    ));
    let mut model = super::super::NativeSession::default();
    assert!(
        controller
            .command(model.retirement_command(1).unwrap())
            .is_ok()
    );
    assert!(endpoint.command().is_some());
    assert_eq!(notices.try_recv().unwrap(), connection);
    assert!(controller.rearm_for(false));
    assert!(matches!(
        notices.try_recv(),
        Err(mailbox::TryRecvError::Empty)
    ));
    assert!(controller.rearm());
    assert_eq!(notices.try_recv().unwrap(), connection);
    for index in 0..INPUT_FRAMES {
        assert_eq!(
            controller.input().unwrap(),
            vec![u8::try_from(index).unwrap(); pulsebeam_proto::state::codec::MAX_FRAME_SIZE]
        );
    }
    assert!(controller.input().is_none());
    assert!(controller.rearm());
    assert!(matches!(
        notices.try_recv(),
        Err(mailbox::TryRecvError::Empty)
    ));

    let first = SignalingOutput {
        sequence: 1,
        cid: channel,
        bytes: vec![1],
        deadline: at,
    };
    assert!(controller.output(first).is_ok());
    let second = SignalingOutput {
        sequence: 2,
        cid: channel,
        bytes: vec![2],
        deadline: at,
    };
    let pointer = second.bytes.as_ptr();
    let second = controller.output(second).err().expect("one queued output");
    assert_eq!(second.bytes.as_ptr(), pointer);
    assert_eq!(endpoint.output().unwrap().sequence, 1);
    assert!(controller.output(second).is_ok());
    assert_eq!(endpoint.output().unwrap().sequence, 2);

    for index in 0..INPUT_FRAMES {
        assert!(endpoint.input(vec![u8::try_from(index).unwrap()]));
    }
    assert!(!endpoint.input(vec![255]));
    assert!(controller.failed() && endpoint.failed());
    let other = ConnectionId::new();
    let (mut independent, peer) = pair(other, ready);
    assert!(peer.input(vec![42]));
    assert!(!independent.failed());
    assert_eq!(notices.try_recv().unwrap(), connection);
    assert_eq!(notices.try_recv().unwrap(), other);
    assert_eq!(independent.input().unwrap(), vec![42]);
    assert!(independent.rearm());
    for index in 0..INPUT_FRAMES {
        assert_eq!(
            controller.input().unwrap(),
            vec![u8::try_from(index).unwrap()]
        );
    }
    assert!(!controller.rearm());
}
