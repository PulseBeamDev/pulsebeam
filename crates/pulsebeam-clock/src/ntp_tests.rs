use crate::{Error, ntp::*, time::*};

const LEAP_2016: LeapTransition = LeapTransition {
    utc_after: UtcSecond(1_483_228_800),
    tai_minus_utc: 37,
};
const UNSMEARED: NtpScale = NtpScale::UnsmearedRepeatLast;

fn stamp(era: i32, seconds: u32, fraction: u32) -> EraTimestamp {
    EraTimestamp {
        era: Era(era),
        timestamp: NtpTimestamp { seconds, fraction },
    }
}

#[test]
fn explicit_era_at_2036_rollover() {
    // A synthetic rule window around 2036, assuming no intervening leap.
    let rules = UtcRules::new(UtcSecond(2_085_978_490), UtcSecond(2_085_978_510), 37, &[]).unwrap();
    let last = GlobalInstant::from_nanos(2_085_978_523_999_918_000);
    let first = GlobalInstant::from_nanos(2_085_978_524_999_918_000);
    for (wire, instant) in [(stamp(0, u32::MAX, 0), last), (stamp(1, 0, 0), first)] {
        assert_eq!(rules.decode(wire, UNSMEARED), Ok(instant));
        assert_eq!(rules.encode(instant), Ok(wire));
    }
    assert_eq!(
        rules.decode(stamp(0, 0, 0), UNSMEARED),
        Err(Error::OutOfWindow)
    );
    assert_eq!(
        rules.decode(stamp(1, 0, 0), NtpScale::Smeared),
        Err(Error::Unresolved)
    );
    assert_eq!(
        rules.decode(stamp(1, 0, 0), NtpScale::Unknown),
        Err(Error::Unresolved)
    );
}

#[test]
fn positive_leap_never_chooses_a_fold() {
    // IERS Bulletin C 52: 36 s before and 37 s from 2017-01-01 UTC.
    let rules = UtcRules::new(
        UtcSecond(1_483_228_790),
        UtcSecond(1_483_228_810),
        36,
        &[LEAP_2016],
    )
    .unwrap();
    for (wire, ns) in [
        (
            stamp(0, 3_692_217_598, 0x8000_0000),
            1_483_228_826_499_918_000,
        ),
        (stamp(0, 3_692_217_600, 0), 1_483_228_828_999_918_000),
    ] {
        let instant = GlobalInstant::from_nanos(ns);
        assert_eq!(rules.decode(wire, UNSMEARED), Ok(instant));
        assert_eq!(rules.encode(instant), Ok(wire));
    }
    assert_eq!(
        rules.decode(stamp(0, 3_692_217_599, 0), UNSMEARED),
        Err(Error::Ambiguous)
    );
    for ns in [
        1_483_228_826_999_918_000,
        1_483_228_827_999_918_000,
        1_483_228_828_999_917_999,
    ] {
        assert_eq!(
            rules.encode(GlobalInstant::from_nanos(ns)),
            Err(Error::Ambiguous)
        );
    }
    let ends_at_leap = UtcRules::new(
        UtcSecond(1_483_228_790),
        LEAP_2016.utc_after,
        36,
        &[LEAP_2016],
    )
    .unwrap();
    assert_eq!(
        ends_at_leap.decode(stamp(0, 3_692_217_599, u32::MAX), UNSMEARED),
        Err(Error::Ambiguous)
    );
    assert_eq!(
        ends_at_leap.decode(stamp(0, 3_692_217_600, 0), UNSMEARED),
        Err(Error::OutOfWindow)
    );
}

#[test]
fn negative_leap_rejects_missing_label_and_preserves_continuity() {
    let transitions = [LeapTransition {
        tai_minus_utc: 35,
        ..LEAP_2016
    }];
    let rules = UtcRules::new(
        UtcSecond(1_483_228_790),
        UtcSecond(1_483_228_810),
        36,
        &transitions,
    )
    .unwrap();
    let before = rules.decode(stamp(0, 3_692_217_598, 0), UNSMEARED).unwrap();
    let after = rules.decode(stamp(0, 3_692_217_600, 0), UNSMEARED).unwrap();
    assert_eq!(
        after.duration_since(before).unwrap().as_nanos(),
        1_000_000_000
    );
    assert_eq!(
        rules.decode(stamp(0, 3_692_217_599, 0), UNSMEARED),
        Err(Error::Unresolved)
    );
    assert_eq!(rules.encode(after), Ok(stamp(0, 3_692_217_600, 0)));
}

#[test]
fn coverage_validation_and_fraction_rounding() {
    let start = UtcSecond(63_072_000);
    let end = UtcSecond(63_072_003);
    let rules = UtcRules::new(start, end, 10, &[]).unwrap();
    assert!(UtcRules::new(UtcSecond(0), end, 10, &[]).is_err());
    assert!(UtcRules::new(start, end, 9, &[]).is_err());
    assert!(UtcRules::new(start, end, 10, &[LEAP_2016]).is_err());
    assert!(UtcRules::new(start, start, 10, &[]).is_err());
    assert_eq!(
        rules.encode(GlobalInstant::from_nanos(0)),
        Err(Error::OutOfWindow)
    );
    assert_eq!(
        rules.encode(GlobalInstant::from_nanos(63_072_001_999_917_999)),
        Err(Error::OutOfWindow)
    );
    assert_eq!(rules.encode(UTC_1972_START), Ok(stamp(0, 2_272_060_800, 0)));
    assert_eq!(
        rules
            .decode(stamp(0, 2_272_060_800, u32::MAX), UNSMEARED)
            .unwrap()
            .as_nanos(),
        63_072_002_999_917_999
    );
    let one_nanosecond = GlobalInstant::from_nanos(63_072_001_999_918_001);
    let encoded = rules.encode(one_nanosecond).unwrap();
    assert_eq!(encoded, stamp(0, 2_272_060_800, 4));
    assert_eq!(rules.decode(encoded, UNSMEARED), Ok(UTC_1972_START));
}

#[test]
fn outgoing_sr_projects_the_same_instant_across_rtp_wrap() {
    let rules = UtcRules::new(UtcSecond(63_072_000), UtcSecond(63_072_003), 10, &[]).unwrap();
    let outgoing = OutgoingRtp {
        rate: ClockRate::VIDEO,
        modular_offset: 1_466_540_320,
    };
    let instant = GlobalInstant::from_nanos(63_072_002_499_918_000);
    assert_eq!(
        outgoing.sender_report_pair(instant, rules),
        Ok(SenderReportPair {
            ntp: stamp(0, 2_272_060_800, 0x8000_0000),
            rtp: RtpTimestamp(0),
        })
    );
    let before = instant
        .checked_add(GlobalDuration::from_nanos(-11_111))
        .unwrap();
    assert_eq!(outgoing.timestamp(before), Ok(RtpTimestamp(u32::MAX)));
}
