use std::time::Duration;

use super::*;

fn input(sequence: u64, timestamp: u32) -> Input {
    Input {
        sequence,
        timestamp,
    }
}

#[test]
fn gaps_and_late_primary_keep_the_same_time_offset() {
    let at = Instant::now();
    let mut timeline = Timeline::new(500, 1500, 90_000, 128).unwrap();
    for (index, timestamp, expected) in [
        (100, 1000, (500, 1500)),
        (102, 1600, (502, 2100)),
        (101, 1300, (501, 1800)),
    ] {
        let (out, next) = timeline
            .prepare(
                1,
                input(index, timestamp),
                at,
                Transmitted::default(),
                false,
            )
            .unwrap();
        assert_eq!((out.sequence, out.timestamp), expected);
        assert_eq!((out.anchor_timestamp, out.wallclock), (1500, at));
        timeline = next;
    }
}

#[test]
fn source_cut_skips_a_position_and_reanchors_after_the_sr_floor() {
    let at = Instant::now();
    let timeline = Timeline::new(500, 1500, 90_000, 128).unwrap();
    let (_, timeline) = timeline
        .prepare(1, input(100, 1000), at, Transmitted::default(), false)
        .unwrap();
    let (_, timeline) = timeline
        .prepare(1, input(102, 1600), at, Transmitted::default(), false)
        .unwrap();
    let cut = at.checked_add(Duration::from_millis(250)).unwrap();
    let transmitted = Transmitted {
        primary: Some(502),
        sender_report: Some(24001),
    };
    let (planned, new_timeline) = timeline
        .prepare(2, input(3, u32::MAX), cut, transmitted, true)
        .unwrap();
    assert_eq!(
        (
            planned.sequence,
            planned.timestamp,
            planned.anchor_timestamp,
            planned.wallclock
        ),
        (504, 24001, 24001, cut)
    );
    // Dropping a refused proposal neither allocates 504 nor activates source 2.
    let (retry, _) = timeline
        .prepare(2, input(3, u32::MAX), cut, transmitted, true)
        .unwrap();
    assert_eq!(retry, planned);
    assert!(matches!(
        new_timeline.prepare(1, input(103, 1700), cut, transmitted, false),
        Err(Rejected::Stale)
    ));
    let (next, _) = new_timeline
        .prepare(2, input(5, 3), cut, transmitted, false)
        .unwrap();
    assert_eq!((next.sequence, next.timestamp), (506, 24005));
}

#[test]
fn queued_progress_cannot_extend_the_receiver_window() {
    let at = Instant::now();
    let timeline = Timeline::new(500, 1500, 90_000, 64).unwrap();
    let (_, timeline) = timeline
        .prepare(1, input(0, 1000), at, Transmitted::default(), false)
        .unwrap();
    let (_, timeline) = timeline
        .prepare(1, input(32767, 1000), at, Transmitted::default(), false)
        .unwrap();
    assert!(matches!(
        timeline.prepare(1, input(32768, 1000), at, Transmitted::default(), false),
        Err(Rejected::AmbiguousEpoch)
    ));
    assert!(matches!(
        timeline.prepare(2, input(0, 1000), at, Transmitted::default(), true),
        Err(Rejected::AmbiguousEpoch)
    ));
    let transmitted = Transmitted {
        primary: Some(565),
        sender_report: None,
    };
    assert!(matches!(
        timeline.prepare(1, input(1, 1000), at, transmitted, false),
        Err(Rejected::Stale)
    ));
    assert!(
        timeline
            .prepare(1, input(2, 1000), at, transmitted, false)
            .is_ok()
    );
}
