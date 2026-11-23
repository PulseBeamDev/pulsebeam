use super::*;

#[test]
fn a_refused_enqueue_does_not_consume_either_branch() {
    let mut left = LiveAdmissions::default();
    let mut right = LiveAdmissions::default();
    assert_eq!(left.offer(70, 70, || false), LiveOffer::Refused);
    assert_eq!(right.offer(70, 70, || true), LiveOffer::Enqueued);
    assert_eq!(left.offer(71, 70, || true), LiveOffer::Enqueued);
    for admits in [&mut left, &mut right] {
        assert_eq!(
            admits.offer(71, 70, || panic!("duplicate queued")),
            LiveOffer::Duplicate
        );
    }
}

#[test]
fn source_progress_retires_holes_and_recycles_only_entering_slots() {
    let mut admits = LiveAdmissions::default();
    assert_eq!(admits.offer(4095, 0, || true), LiveOffer::Enqueued);
    assert_eq!(admits.offer(4095, 64, || true), LiveOffer::Enqueued);
    assert_eq!(admits.offer(4095, 65, || true), LiveOffer::Enqueued);
    assert_eq!(admits.offer(4160, 4160, || false), LiveOffer::Refused);
    assert_eq!(
        admits.offer(4095, 64, || panic!("retired packet queued")),
        LiveOffer::Stale
    );
    assert_eq!(
        admits.offer(4160, 65, || panic!("reserved bit cleared")),
        LiveOffer::Duplicate
    );
    assert_eq!(admits.offer(4160, 4160, || true), LiveOffer::Enqueued);
    assert_eq!(
        admits.offer(4160, 4161, || panic!("unvalidated packet queued")),
        LiveOffer::AheadOfSource
    );
    assert_eq!(admits.offer(8193, 8193, || true), LiveOffer::Enqueued);
    assert_eq!(admits.offer(8193, 8192, || true), LiveOffer::Enqueued);
}

#[test]
fn rollover_and_epoch_sized_jump_do_not_alias_admitted_positions() {
    let mut admits = LiveAdmissions::default();
    for sequence in [0xffff_ffff, 0x1_0000_0000, u64::MAX] {
        assert_eq!(
            admits.offer(sequence, sequence, || true),
            LiveOffer::Enqueued
        );
        assert_eq!(
            admits.offer(sequence, sequence, || panic!("duplicate queued")),
            LiveOffer::Duplicate
        );
    }
    assert_eq!(
        admits.offer(u64::MAX, u64::MAX - 4095, || true),
        LiveOffer::Enqueued
    );
    assert_eq!(
        admits.offer(u64::MAX, u64::MAX - 4096, || panic!(
            "retired packet queued"
        )),
        LiveOffer::Stale
    );
}
