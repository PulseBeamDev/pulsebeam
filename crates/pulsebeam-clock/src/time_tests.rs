use crate::{Error, time::*};

#[test]
fn historical_epoch_vectors() {
    // BIH 1970 annual report, printed pp. 63–64, Table 11; BIPM 1993 annual
    // report: TAI−UTC = 4.2131700 + (40587−39126) × 0.002592 s.
    // Independent ERFA v2.0.1 eraUtctai(2440587.5, 0) returns status 0 and
    // (2440587.5, 9.2593541666666652e-05). Fractional-day comparison against
    // 8.000082 / 86400 used 0.001 ns tolerance; residual < 0.000002 ns.
    assert_eq!(UNIX_EPOCH_TAI_NS, 8_000_082_000);
    assert_eq!(
        GlobalInstant::from_tai_1970_nanos(8_000_082_000)
            .unwrap()
            .as_nanos(),
        0
    );
    assert_eq!(
        GlobalInstant::from_tai_1970_nanos(0).unwrap().as_nanos(),
        -8_000_082_000
    );
    assert_eq!(UTC_1972_START.as_nanos(), 63_072_001_999_918_000);
    assert_eq!(
        UTC_1972_START.to_tai_1970_nanos(),
        Ok(63_072_010_000_000_000)
    );
}

#[test]
fn exact_rates_negative_floor_and_quantization_interval() {
    assert_eq!(ClockRate::new(0), None);
    for (rate, expected) in [
        (ClockRate::OPUS, 72_000),
        (ClockRate::VIDEO, 135_000),
        (ClockRate::DATA, 135_000),
    ] {
        assert_eq!(
            rate.ticks(GlobalInstant::from_nanos(1_500_000_000)),
            Ok(expected)
        );
        assert_eq!(
            rate.ticks(GlobalInstant::from_nanos(-1_500_000_000)),
            Ok(-expected)
        );
        assert_eq!(
            rate.timestamp(GlobalInstant::from_nanos(-1)),
            Ok(RtpTimestamp(u32::MAX))
        );
    }
    let (first, last) = ClockRate::OPUS.instant_interval(-1).unwrap();
    assert_eq!((first.as_nanos(), last.as_nanos()), (-20_833, -1));
    assert_eq!(ClockRate::VIDEO.ticks(first), Ok(-2));
    assert_eq!(ClockRate::VIDEO.ticks(last), Ok(-1));
    assert_eq!(
        ClockRate::new(2_000_000_000).unwrap().instant_interval(1),
        Err(Error::Unresolved)
    );
}

#[test]
fn literal_wraps_at_both_rates() {
    for (rate, at, extended) in [
        (ClockRate::OPUS, 89_478_485_333_334, 4_294_967_296),
        (ClockRate::OPUS, 178_956_970_666_667, 8_589_934_592),
        (ClockRate::OPUS, 447_392_426_666_667, 21_474_836_480),
        (ClockRate::VIDEO, 47_721_858_844_445, 4_294_967_296),
        (ClockRate::VIDEO, 95_443_717_688_889, 8_589_934_592),
        (ClockRate::VIDEO, 238_609_294_222_223, 21_474_836_480),
    ] {
        let at = GlobalInstant::from_nanos(at);
        let before = at.checked_add(GlobalDuration::from_nanos(-1)).unwrap();
        assert_eq!(rate.ticks(at), Ok(extended));
        assert_eq!(rate.timestamp(at), Ok(RtpTimestamp(0)));
        assert_eq!(rate.timestamp(before), Ok(RtpTimestamp(u32::MAX)));
    }
}

#[test]
fn unwrap_fences_and_ties() {
    let binding = RtpBinding {
        source: Binding {
            domain: DomainId(4),
            generation: Generation(1),
        },
        track: 12,
        rate: ClockRate::OPUS,
    };
    let now = MonotonicTime::from_nanos(2_000);
    let anchor = UnwrapAnchor::new(binding, 12_884_901_888, now);
    let window = UnwrapWindow::new(8, 40, 1_000).unwrap();
    let resolve =
        |binding, residue, now| anchor.unwrap(binding, RtpTimestamp(residue), now, window);
    assert_eq!(resolve(binding, u32::MAX, now), Ok(12_884_901_887));
    assert_eq!(resolve(binding, 20, now), Ok(12_884_901_908));
    assert_eq!(resolve(binding, 2_147_483_648, now), Err(Error::Ambiguous));
    assert_eq!(resolve(binding, 41, now), Err(Error::OutOfWindow));
    assert_eq!(
        resolve(binding, 4_294_967_287, now),
        Err(Error::OutOfWindow)
    );
    assert_eq!(
        resolve(binding, 0, MonotonicTime::from_nanos(3_001)),
        Err(Error::Stale)
    );
    assert_eq!(
        resolve(binding, 0, MonotonicTime::from_nanos(1_999)),
        Err(Error::Stale)
    );
    assert_eq!(UnwrapWindow::new(2_147_483_648, 0, 1), Err(Error::Invalid));
    let old = Binding {
        generation: Generation(0),
        ..binding.source
    };
    for wrong in [
        RtpBinding {
            source: old,
            ..binding
        },
        RtpBinding {
            rate: ClockRate::VIDEO,
            ..binding
        },
        RtpBinding {
            track: 13,
            ..binding
        },
    ] {
        assert_eq!(resolve(wrong, 0, now), Err(Error::WrongBinding));
    }
    let anchor = UnwrapAnchor::new(binding, i128::MAX, now);
    assert_eq!(
        anchor.unwrap(binding, RtpTimestamp(0), now, window),
        Err(Error::Overflow)
    );
}

#[test]
fn checked_extremes() {
    let min = GlobalInstant::from_nanos(i64::MIN);
    let max = GlobalInstant::from_nanos(i64::MAX);
    let fast = ClockRate::new(u32::MAX).unwrap();
    assert_eq!(fast.ticks(min), Ok(-39_614_081_247_908_796_760));
    assert_eq!(fast.ticks(max), Ok(39_614_081_247_908_796_755));
    assert_eq!(max.duration_since(min), Err(Error::Overflow));
    assert_eq!(
        max.checked_add(GlobalDuration::from_nanos(1)),
        Err(Error::Overflow)
    );
    assert_eq!(max.to_tai_1970_nanos(), Err(Error::Overflow));
    assert_eq!(
        GlobalInstant::from_tai_1970_nanos(i64::MIN),
        Err(Error::Overflow)
    );
    assert_eq!(fast.instant_interval(i128::MAX), Err(Error::Overflow));
}
