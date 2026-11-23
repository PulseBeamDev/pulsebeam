use std::time::Duration;

use crate::dd::RenderResolution;

use super::*;

fn structure(offset: u8, count: usize, width: u16) -> TemplateDependencyStructure {
    let mut structure = crate::dd::test_utils::structure_minimal();
    structure.template_id_offset = offset;
    structure.templates = std::iter::repeat_n(structure.templates[0].clone(), count).collect();
    structure.resolutions = [RenderResolution { width, height: 480 }]
        .into_iter()
        .collect();
    structure
}

#[test]
fn wrapped_intervals_share_only_identical_semantics() {
    let now = Instant::now();
    let until = now + Duration::from_millis(120);
    let mut intervals = TemplateIntervals::default();
    let first = structure(60, 8, 640);
    let second = structure(60, 8, 1280);
    let a = intervals.offer(now, until, &first, |_| true).unwrap();
    assert_eq!(a.offset(), 60);
    assert_eq!(a.map(60, 3), Some(3));
    assert_eq!(a.map(60, 4), None);
    let alias = structure(11, 8, 640);
    let shared = intervals.offer(now, until, &alias, |_| true).unwrap();
    assert_eq!(shared, a);
    assert_eq!(shared.map(11, 18), Some(3));
    let different = intervals.offer(now, until, &second, |_| true).unwrap();
    assert_eq!(different.offset(), 4);
    assert_eq!(different.map(60, 3), Some(11));
}

#[test]
fn refused_admissions_neither_reserve_nor_extend_a_lease() {
    let now = Instant::now();
    let until = now + Duration::from_millis(120);
    let later = until + Duration::from_millis(120);
    let mut intervals = TemplateIntervals::default();
    let first = structure(63, 64, 640);
    let second = structure(1, 64, 1280);
    assert!(intervals.offer(now, later, &second, |_| false).is_none());
    let a = intervals.offer(now, until, &first, |_| true).unwrap();
    assert_eq!(a.map(63, 62), Some(62));
    assert!(intervals.offer(now, later, &first, |_| false).is_none());
    assert!(
        intervals
            .offer(now, later, &second, |_| panic!("space is live"))
            .is_none()
    );
    assert_eq!(
        intervals
            .offer(until, later, &second, |_| true)
            .unwrap()
            .offset(),
        1
    );
}
