use super::*;
use pulsebeam_clock::{Error, ntp::Era};

const TABLE: &str = "#@ 4500000000\n2272060800 10 # 1972\n2287785600 11 # July 1972\n";
const BASE_UTC: i64 = 2_208_988_800_000_000_000;

fn sample(
    start: Instant,
    elapsed_secs: u64,
    base: i64,
    step: i64,
    error_ns: Option<u64>,
) -> Sample {
    Sample {
        at: start + Duration::from_secs(elapsed_secs),
        global: GlobalInstant::from_nanos(
            base + i64::try_from(elapsed_secs).unwrap() * NS_PER_SEC + step,
        ),
        error_ns,
    }
}

#[test]
fn linux_pair_is_qualified_only_when_attested_bounded_and_consistent() {
    let policy = Policy::parse(TABLE).unwrap();
    let before = Instant::now();
    let read = || HostClockRead {
        before,
        after: before + Duration::from_micros(8),
        unix_nanos: BASE_UTC,
        tai_nanos: Some(BASE_UTC + 11 * NS_PER_SEC + 7_999),
        kernel_max_error: Some(Duration::from_micros(23)),
    };
    let qualified = Sample::host(read(), &policy, true).unwrap();
    assert_eq!(qualified.at - before, Duration::from_micros(4));
    assert_eq!(qualified.error_ns, Some(31_000));
    assert_eq!(
        qualified.global.as_nanos(),
        BASE_UTC + 11 * NS_PER_SEC - 8_000_082_000
    );
    let ntp = policy.rules().encode(qualified.global).unwrap();
    assert_eq!(ntp.era, Era(1));
    assert_eq!(ntp.timestamp.seconds, 123_010_304);
    assert_eq!(ntp.timestamp.fraction, 0);
    assert!(
        Sample::host(read(), &policy, false)
            .unwrap()
            .error_ns
            .is_none()
    );
    for invalid in [
        HostClockRead {
            tai_nanos: Some(BASE_UTC),
            ..read()
        },
        HostClockRead {
            after: before + Duration::from_millis(3),
            ..read()
        },
        HostClockRead {
            kernel_max_error: None,
            ..read()
        },
    ] {
        let unqualified = Sample::host(invalid, &policy, true).unwrap();
        assert_eq!(unqualified.global, qualified.global);
        assert!(unqualified.error_ns.is_none());
    }
}

#[test]
fn shard_snapshots_agree_and_unknown_cold_start_does_not_take_a_wall_step() {
    let start = Instant::now();
    let base = 1_700_000_000_000_000_000;
    let initial = sample(start, 0, base, 0, None);
    let mut authority = Authority::start(Policy::parse(TABLE).unwrap(), initial);
    let left = authority.snapshot.clone();
    let right = authority.snapshot.clone();
    let later = start + Duration::from_millis(500);
    assert_eq!(left.at(later), right.at(later));
    assert_eq!(left.at(later).unwrap().discipline, Discipline::Unqualified);
    assert_eq!(left.at(later).unwrap().uncertainty_ns, None);

    // A wall step while unlocked cannot replace the initial coordinate.
    let step = sample(start, 1, base, NS_PER_SEC, None);
    authority.refresh(step.at, None, Some(step));
    let estimate = authority.snapshot.at(step.at).unwrap();
    assert_eq!(estimate.discipline, Discipline::Unqualified);
    assert_eq!(estimate.global.as_nanos(), base + NS_PER_SEC);
}

#[test]
fn step_invalidates_lock_and_recovery_slews_in_both_directions() {
    let start = Instant::now();
    let base = 1_700_000_000_000_000_000;
    for offset in [-1_200_000, 1_200_000] {
        let mut authority = Authority::start(
            Policy::parse(TABLE).unwrap(),
            sample(start, 0, base, 0, Some(100)),
        );
        let s = sample(start, 1, base, offset, Some(100));
        authority.refresh(s.at, None, Some(s));
        let estimate = authority.snapshot.at(s.at).unwrap();
        assert_eq!(estimate.discipline, Discipline::Holdover);
        assert_eq!(estimate.evidence_generation, 1);
        assert_eq!(estimate.global.as_nanos(), base + NS_PER_SEC);

        for second in 2..=5 {
            let s = sample(start, second, base, offset, Some(100));
            let expected = authority.snapshot.at(s.at).unwrap().global;
            authority.refresh(s.at, None, Some(s));
            assert_eq!(authority.snapshot.at(s.at).unwrap().global, expected);
            if second == 2 {
                let next = authority
                    .snapshot
                    .at(s.at + Duration::from_secs(1))
                    .unwrap();
                assert_eq!(next.discipline, Discipline::Slewing);
                assert_eq!(
                    next.global.as_nanos(),
                    base + 3 * NS_PER_SEC + offset.signum() * 500_000
                );
                assert!(next.uncertainty_ns.is_none());
            }
        }
        let estimate = authority
            .snapshot
            .at(start + Duration::from_secs(5))
            .unwrap();
        assert_eq!(estimate.discipline, Discipline::Locked);
        assert_eq!(estimate.global.as_nanos(), base + 5 * NS_PER_SEC + offset);
        assert_eq!(estimate.evidence_generation, 1);
        let stale = authority
            .snapshot
            .at(start + Duration::from_secs(8))
            .unwrap();
        assert_eq!(stale.discipline, Discipline::Holdover);
        assert_eq!(stale.evidence_generation, 2);
        assert_eq!(stale.uncertainty_ns, None);
    }
}

#[test]
fn delayed_negative_slew_never_regresses_a_shard_execution_clock() {
    let start = Instant::now();
    let base = 1_700_000_000_000_000_000;
    let mut authority = Authority::start(
        Policy::parse(TABLE).unwrap(),
        sample(start, 0, base, 0, Some(100)),
    );
    let mut late = ShardClock::new(authority.snapshot.clone());
    for second in 1..=2 {
        let s = sample(start, second, base, -2_000_000, Some(100));
        authority.refresh(s.at, None, Some(s));
    }
    let at = start + Duration::from_millis(2_900);
    let previous = late.now(at).unwrap();
    late.update(authority.snapshot.clone());
    let clamped = late.now(at + Duration::from_micros(20)).unwrap();
    assert_eq!(clamped.global, previous.global);
    assert_eq!(clamped.uncertainty_ns, None);
    assert_eq!(clamped.discipline, Discipline::Slewing);
    let later = at + Duration::from_millis(1);
    assert_eq!(late.now(later), authority.snapshot.at(later));
}

#[test]
fn leap_labels_and_policy_expiry_do_not_stop_continuous_time() {
    let policy = Policy::parse(TABLE).unwrap();
    let transition = policy.transitions[0].utc_after.0;
    assert_eq!(
        policy
            .rules()
            .decode_unix_nanos((transition - 1) * NS_PER_SEC),
        Err(Error::Ambiguous)
    );
    let after_leap = policy
        .rules()
        .decode_unix_nanos(transition * NS_PER_SEC)
        .unwrap();
    let inserted = after_leap
        .checked_add(GlobalDuration::from_nanos(-500_000_000))
        .unwrap();
    assert_eq!(policy.rules().encode(inserted), Err(Error::Ambiguous));

    let start = Instant::now();
    let base = policy.expires.as_nanos() - NS_PER_SEC;
    let mut authority = Authority::start(policy, sample(start, 0, base, 0, Some(0)));
    let at_expiry = authority
        .snapshot
        .at(start + Duration::from_secs(1))
        .unwrap();
    assert_eq!(at_expiry.global.as_nanos(), base + NS_PER_SEC);
    assert_eq!(at_expiry.uncertainty_ns, None);
    assert_eq!(at_expiry.evidence_generation, 1);
    assert_eq!(
        authority.snapshot.policy.rules().encode(at_expiry.global),
        Err(Error::OutOfWindow)
    );

    let replacement = Policy::parse(&TABLE.replace("4500000000", "4500100000")).unwrap();
    let s = sample(start, 1, base, 0, Some(0));
    authority.refresh(s.at, Some(replacement), Some(s));
    let s = sample(start, 2, base, 0, Some(0));
    authority.refresh(s.at, None, Some(s));
    let recovered = authority.snapshot.at(s.at).unwrap();
    assert_eq!(recovered.discipline, Discipline::Locked);
    assert_eq!(recovered.policy_generation, 1);
    assert_eq!(recovered.evidence_generation, 1);
    assert_eq!(recovered.global, s.global);
}
