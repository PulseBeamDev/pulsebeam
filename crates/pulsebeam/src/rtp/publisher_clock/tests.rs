use std::time::Duration;

use super::*;
use pulsebeam_clock::{
    domain::Provenance,
    time::{DomainId, Generation},
};

fn clock(at: Instant, source: u64, generation: u64) -> PublisherClock {
    PublisherClock::with_verified_members(
        Binding {
            domain: DomainId(source),
            generation: Generation(generation),
        },
        &[(5, ClockRate::VIDEO)],
        at,
        GlobalInstant::from_nanos(2_085_978_494_000_000_000),
    )
    .unwrap()
}

fn report(at: Instant, seconds: u32, tick: u32) -> pulsebeam_rtc::SenderReport {
    pulsebeam_rtc::SenderReport {
        arrival: at.into(),
        ssrc: 5,
        ntp_seconds: seconds,
        ntp_fraction: 0,
        rtp_timestamp: tick,
    }
}

#[test]
fn deferred_report_stays_inside_the_admitted_lifetime_and_unfolds_across_era() {
    let at = Instant::now();
    let mut active = clock(at, 2, 1);
    let mut unrelated = clock(at, 3, 1);
    let early = report(at + Duration::from_secs(1), u32::MAX, 90_000);
    active.report(early, at + Duration::from_secs(1)).unwrap();
    assert_eq!(unrelated.encodings[0].pending_report, None);

    let start = at + Duration::from_secs(2);
    active.media(0, 180_000, start, start).unwrap();
    assert_eq!(
        active.encodings[0].last_report.unwrap().1.as_nanos(),
        2_085_978_495_000_000_000
    );
    active.report(report(start, 0, 180_000), start).unwrap();
    assert_eq!(
        active.encodings[0].last_report.unwrap().1.as_nanos(),
        2_085_978_496_000_000_000
    );

    let mut replacement = clock(start, 2, 2);
    assert_eq!(replacement.report(early, start), Err(Error::Stale));
    assert_eq!(replacement.encodings[0].pending_report, None);
    let projected = unrelated.media(0, 180_000, start, start).unwrap();
    assert_eq!(projected.provenance, Provenance::ArrivalEstimate);
}

#[test]
fn unqualified_arrival_cadence_crosses_rtp_wrap_without_source_evidence() {
    let at = Instant::now();
    let mut clock = clock(at, 2, 1);
    clock.host_reference(at, 0, false).unwrap();
    let first = clock.media(0, u32::MAX - 44_999, at, at).unwrap();
    let now = at + Duration::from_secs(1);
    clock.host_reference(now, 1, false).unwrap();
    let next = clock.media(0, 45_000, now, now).unwrap();
    assert_eq!(next.provenance, Provenance::ArrivalEstimate);
    assert_eq!(next.rtp.0.wrapping_sub(first.rtp.0), 90_000);
    assert_eq!(
        clock.media(0, 2_147_528_648, now, now).err(),
        Some(Error::Ambiguous)
    );
}

#[test]
fn assigned_samples_survive_reorder_and_holdover_without_exporting_sender_ticks() {
    let at = Instant::now();
    let mut clock = clock(at, 2, 1);
    let a = clock.assign_sample(0, 200, at, at);
    let next = at + Duration::from_millis(20);
    let b = clock.assign_sample(0, 2_000, next, next);
    assert_ne!(a.timestamp, 200);
    assert_eq!(b.timestamp.wrapping_sub(a.timestamp), 1_800);
    assert_eq!(b.projection.unwrap().1.rtp.0, b.timestamp);
    clock.host_reference(next, 1, false).unwrap();
    let later = next + Duration::from_millis(15);
    let reordered = clock.assign_sample(0, 200, later, later);
    let duplicate = clock.assign_sample(0, 2_000, later, later);
    assert_eq!(reordered.timestamp, a.timestamp);
    assert_eq!(reordered.projection, a.projection);
    assert_eq!(duplicate.timestamp, b.timestamp);
    assert_eq!(duplicate.projection, b.projection);
    let fallback = clock.assign_sample(0, 0x8000_07d0, later, later);
    assert_eq!(fallback.timestamp, b.timestamp.wrapping_add(1_350));
    assert!(fallback.projection.is_none());
}

#[test]
fn verified_members_share_a_domain_but_keep_independent_rtp_coordinates() {
    let at = Instant::now();
    let binding = Binding {
        domain: DomainId(2),
        generation: Generation(1),
    };
    let mut shared = PublisherClock::with_verified_members(
        binding,
        &[(5, ClockRate::VIDEO), (6, ClockRate::OPUS)],
        at,
        GlobalInstant::from_nanos(2_085_978_494_000_000_000),
    )
    .unwrap();
    let early = report(at, u32::MAX - 1, 90_000);
    shared.report(early, at).unwrap();
    let audio = shared.media(1, 1_000, at, at).unwrap();
    let video = shared.media(0, 90_000, at, at).unwrap();
    assert_eq!(audio.binding, binding);
    assert_eq!(video.binding, binding);
    assert_ne!(audio.rtp, video.rtp);
    assert_eq!(audio.global, video.global);
    assert!(shared.encodings[0].last_report.is_some());
    assert!(shared.encodings[1].last_report.is_none());
    assert_eq!(
        shared.report(pulsebeam_rtc::SenderReport { ssrc: 7, ..early }, at),
        Err(Error::WrongBinding)
    );
    assert_eq!(shared.media(2, 1_000, at, at), Err(Error::WrongBinding));
}
