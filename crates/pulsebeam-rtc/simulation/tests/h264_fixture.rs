use pulsebeam_rtc_simulation::{SimulationWorld, stock_h264_fixture};
use std::collections::BTreeSet;

#[test]
fn stock_h264_reports_genuine_three_temporal_layer_access_units() {
    let world = SimulationWorld::acquire();
    let units = stock_h264_fixture(&world, 3, 48).unwrap();
    assert_eq!(units.len(), 48);
    assert!(units[0].keyframe);
    assert!(
        units
            .iter()
            .all(|unit| unit.width == 320 && unit.height == 180)
    );
    assert!(units.iter().all(|unit| !unit.data.is_empty()));
    let temporal: BTreeSet<_> = units.iter().map(|unit| unit.temporal_index).collect();
    assert_eq!(temporal, BTreeSet::from([Some(0), Some(1), Some(2)]));
    let timestamps: BTreeSet<_> = units.iter().map(|unit| unit.rtp_timestamp).collect();
    assert_eq!(timestamps.len(), units.len());
    let payloads: BTreeSet<_> = units.iter().map(|unit| unit.data.as_slice()).collect();
    assert!(
        payloads.len() > units.len() / 2,
        "encoder must emit real access units, not duplicate rungs"
    );
    assert!(
        units
            .iter()
            .filter(|unit| unit.temporal_index == Some(0))
            .count()
            >= 8
    );
}
