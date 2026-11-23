use std::time::Duration;

use crate::dd::{DependencyDescriptorReader, evidence::SourceContext};

use super::*;

const HF: [&[u8]; 2] = [&[0x80, 0, 0, 0x80, 0, 0x3b, 0x40, 0], &[0, 0, 0]];
const AF: [&[u8]; 2] = [&[0x80, 0, 42, 0x80, 0, 0xe0], &[0x40, 0, 42]];

fn packet(source: u8, sequence: u64, mode: Mode, entry: bool, q: &Qualified) -> Packet<'_, u8> {
    Packet {
        source,
        output_sequence: sequence,
        mode,
        entry,
        dd: Some(Evidence {
            qualified: q,
            frame: u64::from(q.descriptor.frame_number),
        }),
    }
}

#[test]
fn appendix_filtered_h264_and_av1_rewrite_masks_without_changing_dependencies() {
    for pair in [HF, AF] {
        let mut context = SourceContext::default();
        let mut receiver = DependencyDescriptorReader::new();
        let mut output = OutputMapper::new(77);
        let now = Instant::now();
        let until = now + Duration::from_millis(120);
        for (sequence, raw) in (100..).zip(pair) {
            let q = context.observe(sequence, Some(raw)).unwrap().unwrap();
            let target = u8::try_from(q.active_targets.trailing_zeros()).unwrap();
            output
                .offer(
                    now,
                    until,
                    packet(1, sequence, Mode::Filtered(target), sequence == 100, &q),
                    |bytes| {
                        let bytes = bytes.unwrap();
                        let dd = receiver.read(bytes).unwrap();
                        assert_eq!(dd.frame_number, 77);
                        assert_eq!(receiver.active_decode_targets(), 1 << target);
                        if sequence == 100 {
                            assert_eq!(dd.active_decode_targets, Some(1 << target));
                        } else {
                            assert_eq!(dd.active_decode_targets, None);
                            assert_eq!(bytes.len(), raw.len());
                        }
                        assert_eq!(dd.start_of_frame, q.descriptor.start_of_frame);
                        assert_eq!(dd.end_of_frame, q.descriptor.end_of_frame);
                        assert_eq!(dd.frame_dependencies, q.descriptor.frame_dependencies);
                        true
                    },
                )
                .unwrap();
        }
    }
}

#[test]
fn old_structure_reservations_cannot_cross_a_new_mapping() {
    let now = Instant::now();
    let until = now + Duration::from_millis(120);
    let mut context = SourceContext::default();
    let initial = context.observe(10, Some(AF[0])).unwrap().unwrap();
    let prior = context.observe(11, Some(AF[1])).unwrap().unwrap();
    let replacement = context.observe(12, Some(AF[0])).unwrap().unwrap();
    let current = context.observe(13, Some(AF[1])).unwrap().unwrap();
    assert_eq!(
        context
            .qualify(11, Some(AF[1]))
            .unwrap()
            .unwrap()
            .structure_sequence,
        10
    );
    assert_eq!(current.structure_sequence, 12);
    let mut mapper = OutputMapper::new(u16::MAX);
    let mut receiver = DependencyDescriptorReader::new();
    let mut check = |bytes: Option<&[u8]>, expected| {
        assert_eq!(
            receiver.read(bytes.unwrap()).unwrap().frame_number,
            expected
        );
        true
    };
    mapper
        .offer(
            now,
            until,
            packet(1, 100, Mode::Whole, false, &initial),
            |b| check(b, u16::MAX),
        )
        .unwrap();
    mapper
        .offer(
            now,
            until,
            packet(1, 102, Mode::Whole, false, &replacement),
            |b| check(b, u16::MAX),
        )
        .unwrap();
    assert_eq!(
        mapper.offer(
            now,
            until,
            packet(1, 101, Mode::Whole, false, &prior),
            |_| unreachable!()
        ),
        Err(Rejected::Stale)
    );
    mapper
        .offer(
            now,
            until,
            packet(1, 103, Mode::Whole, false, &current),
            |b| check(b, u16::MAX),
        )
        .unwrap();
    mapper
        .offer(
            now,
            until,
            packet(2, 105, Mode::Whole, true, &initial),
            |b| check(b, 0),
        )
        .unwrap();
}

#[test]
fn refused_bootstrap_and_context_loss_do_not_publish_references() {
    let mut context = SourceContext::default();
    let seed = context.observe(1, Some(AF[0])).unwrap().unwrap();
    let delta = context.observe(2, Some(AF[1])).unwrap().unwrap();
    let now = Instant::now();
    let until = now + Duration::from_millis(120);
    let mut output = OutputMapper::new(10);
    let mode = Mode::Filtered(0);
    assert_eq!(
        output.offer(now, until, packet(1, 100, mode, true, &seed), |_| false),
        Err(Rejected::Admission)
    );
    assert_eq!(
        output.offer(
            now,
            until,
            packet(1, 101, mode, false, &delta),
            |_| unreachable!()
        ),
        Err(Rejected::Bootstrap)
    );
    assert_eq!(
        output.offer(now, until, packet(1, 100, mode, true, &seed), |_| true),
        Ok(())
    );
    output.lose_context();
    assert_eq!(
        output.offer(
            now,
            until,
            packet(1, 101, mode, false, &delta),
            |_| unreachable!()
        ),
        Err(Rejected::Bootstrap)
    );
    assert_eq!(
        output.offer(
            now,
            until,
            packet(1, 101, Mode::Whole, false, &delta),
            |bytes| bytes.is_none()
        ),
        Ok(())
    );
    assert_eq!(
        output.offer(
            now,
            until,
            packet(1, 100, mode, true, &seed),
            |_| unreachable!()
        ),
        Err(Rejected::Stale)
    );
}
