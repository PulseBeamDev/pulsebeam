use std::num::NonZeroU16;

use pulsebeam_core::dd::{
    DecodeTargetIndication::{NotPresent, Required, Switch},
    DependencyDescriptor, FrameDependencyTemplate, TemplateDependencyStructure,
};

use super::*;
use crate::media::units::{Dependency, Framing, SenderTime, Unit};

#[allow(
    clippy::disallowed_methods,
    reason = "all elapsed time is virtual from a single test origin"
)]
fn origin() -> Instant {
    Instant::now()
}

#[test]
fn freshness_waits_for_video_restart_without_withholding_recording() {
    let binding = Binding {
        generation: crate::executor::tests::dial_effect().generation,
        id: NonZeroU16::MIN,
    };
    let now = origin();
    for codec in [Codec::H264, Codec::Vp8, Codec::Vp9, Codec::Av1] {
        let mut fresh = Track::new(
            binding,
            codec,
            Policy::Freshness {
                reorder: Duration::ZERO,
                max_age: Duration::from_millis(50),
            },
        )
        .unwrap();
        let mut record = Track::new(
            binding,
            codec,
            Policy::Recording {
                reorder: Duration::ZERO,
            },
        )
        .unwrap();
        let unit = |sequence: u64, access| {
            Event::Unit(Unit {
                source: binding,
                codec,
                first_sequence: sequence,
                last_sequence: sequence,
                rtp_time: Some(sequence.saturating_mul(3_000)),
                sender_time: SenderTime::Unknown,
                arrived_at: now,
                access,
                dependency: Dependency::Absent,
                framing: Framing::Whole,
                data: vec![1],
            })
        };
        for seq in [10, 11] {
            fresh.emit(now, unit(seq, Access::DependentVideo));
            fresh.schedule_refresh(now);
            assert!(fresh.next(now).is_none());
            assert_eq!(fresh.refresh_due(now), seq == 10);
            if seq == 10 {
                fresh.refresh_result(now, Ok(()));
            }
            record.emit(now, unit(seq, Access::DependentVideo));
            assert!(
                matches!(record.next(now), Some(Receive::Media(Event::Unit(unit))) if unit.first_sequence == seq)
            );
            assert!(!record.refresh_due(now));
        }
        fresh.emit(now, unit(12, Access::IndependentVideo));
        assert!(
            matches!(fresh.next(now), Some(Receive::Media(Event::Unit(unit))) if unit.first_sequence == 12)
        );
        fresh.emit(now, unit(13, Access::DependentVideo));
        assert!(
            matches!(fresh.next(now), Some(Receive::Media(Event::Unit(unit))) if unit.first_sequence == 13)
        );
        assert_eq!(fresh.stats().withheld_video, 2);
        assert_eq!(fresh.stats().refresh_sent, 1);
    }
}

#[test]
fn freshness_checks_selected_target_ancestry_and_recovers_without_a_new_binding() {
    let binding = Binding {
        generation: crate::executor::tests::dial_effect().generation,
        id: NonZeroU16::MIN,
    };
    let mut fresh = Track::new(
        binding,
        Codec::Vp9,
        Policy::Freshness {
            reorder: Duration::ZERO,
            max_age: Duration::from_millis(50),
        },
    )
    .unwrap();
    let root = FrameDependencyTemplate {
        dtis: [Switch; 2].into_iter().collect(),
        chain_diffs: [0].into_iter().collect(),
        ..Default::default()
    };
    let structure = TemplateDependencyStructure {
        decode_target_count: 2,
        chain_count: 1,
        decode_target_protected_by: [0, 0].into_iter().collect(),
        templates: [root.clone()].into_iter().collect(),
        ..Default::default()
    };
    let descriptor = |frame_number, targets, diff: Option<u16>, chain, attach| {
        let mut deps = root.clone();
        deps.dtis = targets;
        deps.frame_diffs.extend(diff);
        deps.chain_diffs[0] = chain;
        Dependency::Parsed(Box::new(DependencyDescriptor {
            start_of_frame: true,
            end_of_frame: true,
            frame_number,
            frame_dependencies: deps,
            active_decode_targets: if attach { Some(2) } else { None },
            attached_structure: attach.then(|| Box::new(structure.clone())),
            ..Default::default()
        }))
    };
    let switch = || [Switch; 2].into_iter().collect();
    let now = origin();
    let unit = |seq, access, dependency| {
        Event::Unit(Unit {
            source: binding,
            codec: Codec::Vp9,
            first_sequence: seq,
            last_sequence: seq,
            rtp_time: Some(3_000),
            sender_time: SenderTime::Unknown,
            arrived_at: now,
            access,
            dependency,
            framing: Framing::Whole,
            data: vec![1],
        })
    };
    for (seq, access, dd, deliver, refresh) in [
        (
            100,
            Access::IndependentVideo,
            descriptor(u16::MAX - 1, switch(), Some(1), 1, true),
            false,
            true,
        ),
        (
            101,
            Access::IndependentVideo,
            descriptor(u16::MAX, switch(), None, 0, true),
            true,
            false,
        ),
        (
            102,
            Access::DependentVideo,
            descriptor(
                0,
                [Required, NotPresent].into_iter().collect(),
                Some(1),
                1,
                false,
            ),
            false,
            false,
        ),
        (
            103,
            Access::DependentVideo,
            descriptor(1, switch(), Some(2), 2, false),
            true,
            false,
        ),
        (
            104,
            Access::DependentVideo,
            descriptor(2, switch(), Some(1), 2, false),
            false,
            true,
        ),
        (
            105,
            Access::IndependentVideo,
            Dependency::Absent,
            false,
            false,
        ),
        (
            106,
            Access::IndependentVideo,
            descriptor(3, switch(), None, 0, false),
            false,
            false,
        ),
        (
            107,
            Access::IndependentVideo,
            descriptor(4, switch(), None, 0, true),
            true,
            false,
        ),
        (
            108,
            Access::DependentVideo,
            descriptor(5, switch(), Some(1), 1, false),
            true,
            false,
        ),
    ] {
        fresh.emit(now, unit(seq, access, dd));
        fresh.schedule_refresh(now);
        let event = fresh.next(now);
        if deliver {
            assert!(
                matches!(event, Some(Receive::Media(Event::Unit(unit))) if unit.source == binding && unit.first_sequence == seq),
                "sequence {seq}"
            );
        } else {
            assert!(event.is_none(), "sequence {seq}");
        }
        assert_eq!(fresh.refresh_due(now), refresh, "sequence {seq}");
        if refresh {
            fresh.refresh_result(now, Ok(()));
        }
    }
    assert_eq!(fresh.stats().refresh_sent, 2);
}

#[test]
fn slow_reader_overflow_keeps_one_refresh_outstanding_and_does_not_block_another_binding() {
    let binding = Binding {
        generation: crate::executor::tests::dial_effect().generation,
        id: NonZeroU16::MIN,
    };
    let other = Binding {
        id: NonZeroU16::new(2).unwrap(),
        ..binding
    };
    let max_age = Duration::from_millis(50);
    let policy = Policy::Freshness {
        reorder: Duration::ZERO,
        max_age,
    };
    let mut stalled = Track::new(binding, Codec::Vp8, policy).unwrap();
    let mut healthy = Track::new(other, Codec::Vp8, policy).unwrap();
    let now = origin();
    let unit = |source, sequence, arrived_at, access| {
        Event::Unit(Unit {
            source,
            codec: Codec::Vp8,
            first_sequence: sequence,
            last_sequence: sequence,
            rtp_time: None,
            sender_time: SenderTime::Unknown,
            arrived_at,
            access,
            dependency: Dependency::Absent,
            framing: Framing::Whole,
            data: vec![1],
        })
    };
    let full = u64::try_from(EVENTS).unwrap();
    for seq in 0..=full {
        let access = if seq == 0 {
            Access::IndependentVideo
        } else {
            Access::DependentVideo
        };
        stalled.emit(now, unit(binding, seq, now, access));
        healthy.emit(now, unit(other, seq, now, access));
        assert!(
            matches!(healthy.next(now), Some(Receive::Media(Event::Unit(unit))) if unit.source == other && unit.first_sequence == seq)
        );
    }
    stalled.schedule_refresh(now);
    assert!(stalled.refresh_due(now));
    stalled.refresh_result(now, Err(DatagramAdmission::Full));
    assert!(!stalled.refresh_due(now));
    let retry = now.checked_add(REFRESH_RETRY).unwrap();
    let last = full.saturating_add(2);
    for seq in full.saturating_add(1)..=last {
        stalled.emit(now, unit(binding, seq, now, Access::IndependentVideo));
        stalled.schedule_refresh(now);
        assert!(!stalled.refresh_due(now));
    }
    assert!(stalled.refresh_due(retry));
    stalled.refresh_result(retry, Ok(()));
    for seq in 0..full {
        assert!(
            matches!(stalled.next(retry), Some(Receive::Media(Event::Unit(unit))) if unit.first_sequence == seq)
        );
    }
    assert!(
        matches!(stalled.next(retry), Some(Receive::Loss(Loss { first_sequence, last_sequence, overflow: 3, expired: 0, invalidated: 0 })) if first_sequence == full && last_sequence == last)
    );

    stalled.emit(
        retry,
        unit(
            binding,
            last.saturating_add(1),
            retry,
            Access::DependentVideo,
        ),
    );
    assert!(stalled.next(retry).is_none());
    stalled.emit(
        retry,
        unit(
            binding,
            last.saturating_add(2),
            retry,
            Access::IndependentVideo,
        ),
    );
    let later = retry.checked_add(Duration::from_millis(1)).unwrap();
    stalled.emit(
        later,
        unit(
            binding,
            last.saturating_add(3),
            later,
            Access::DependentVideo,
        ),
    );
    let expiry = retry.checked_add(max_age).unwrap();
    assert!(matches!(
        stalled.next(expiry),
        Some(Receive::Loss(Loss {
            overflow: 0,
            expired: 1,
            invalidated: 1,
            ..
        }))
    ));
    assert!(stalled.next(expiry).is_none());
    assert!(stalled.refresh_due(expiry));
    assert_eq!(stalled.stats().refresh_sent, 1);
    assert_eq!(stalled.stats().refresh_backpressure, 1);
    assert_eq!(healthy.stats().handoff_overflow, 0);
    assert!(!healthy.refresh_due(expiry));
}
