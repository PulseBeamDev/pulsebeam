use crate::{Error, ntp::*, time::*};

struct Replay(u64);

impl Replay {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn replayed_quantization_and_wrap_properties() {
    const SEED: u64 = 0xe6ce_754c_d023_aa39;
    let mut random = Replay(SEED);
    for case in 0..4096 {
        let random_time = i64::from_le_bytes(random.next().to_le_bytes()) / 2;
        let rate = if case % 2 == 0 {
            ClockRate::OPUS
        } else {
            ClockRate::VIDEO
        };
        let instant = GlobalInstant::from_nanos(random_time);
        let tick = rate.ticks(instant).unwrap();
        let (earliest, latest) = rate.instant_interval(tick).unwrap();
        assert!(
            (earliest..=latest).contains(&instant),
            "seed {SEED:#x}, case {case}, ns {random_time}, {rate:?}"
        );
        assert_eq!(rate.ticks(earliest), Ok(tick));
        assert_eq!(rate.ticks(latest), Ok(tick));
        let binding = RtpBinding {
            source: Binding {
                domain: DomainId(1),
                generation: Generation(19),
            },
            track: 2,
            rate,
        };
        let now = MonotonicTime::from_nanos(1);
        let anchor = UnwrapAnchor::new(binding, tick, now);
        let window = UnwrapWindow::new(100, 300, 100).unwrap();
        let offset = i128::from(random.next() % 600) - 200;
        let tick_to_assign = tick + offset;
        let wire = RtpTimestamp::from_ticks(tick_to_assign).unwrap();
        let result = anchor.unwrap(binding, wire, now, window);
        if (-100..=300).contains(&offset) {
            assert_eq!(
                result,
                Ok(tick_to_assign),
                "seed {SEED:#x}, case {case}, tick {tick}, offset {offset}"
            );
        } else {
            assert_eq!(
                result,
                Err(Error::OutOfWindow),
                "seed {SEED:#x}, case {case}, tick {tick}, offset {offset}"
            );
        }
    }
}

#[test]
fn replayed_ntp_fraction_round_trips_never_cross_second() {
    const SEED: u64 = 0xd984_11ce_a79f_fd75;
    let mut random = Replay(SEED);
    let rules = UtcRules::new(UtcSecond(2_085_978_495), UtcSecond(2_085_978_497), 37, &[]).unwrap();
    for case in 0..4096 {
        let seconds = u32::MAX.wrapping_add(case % 2);
        let era = if seconds == u32::MAX { Era(0) } else { Era(1) };
        let fraction = u32::try_from(random.next() & 0xffff_ffff).unwrap();
        let input = EraTimestamp {
            era,
            timestamp: NtpTimestamp { seconds, fraction },
        };
        let decoded = rules.decode(input, NtpScale::UnsmearedRepeatLast).unwrap();
        let encoded = rules.encode(decoded).unwrap();
        assert_eq!(encoded.era, era);
        assert_eq!(encoded.timestamp.seconds, seconds);
        // Flooring an external fraction to a nanosecond and back can
        // discard up to five units of the binary fraction.
        let lost_units = fraction.checked_sub(encoded.timestamp.fraction).unwrap();
        assert!(
            lost_units <= 5,
            "seed {SEED:#x}, case {case}, {input:?}, {encoded:?}"
        );
        let back = rules
            .decode(encoded, NtpScale::UnsmearedRepeatLast)
            .unwrap();
        assert!((0..=1).contains(&decoded.duration_since(back).unwrap().as_nanos()));
    }
}
