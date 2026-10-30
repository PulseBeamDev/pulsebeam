use pulsebeam_rtc_simulation::{
    NativeH264FixtureEncoding, SimulationWorld, stock_h264_fixture,
    stock_h264_fixture_for_encoding, stock_h264_solid_fixture,
};
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
    let solid = stock_h264_solid_fixture(&world).unwrap();
    assert_eq!(solid.len(), 2);
    assert!(
        solid
            .iter()
            .all(|unit| unit.keyframe && unit.width == 320 && unit.height == 180)
    );
    assert_ne!(solid[0].data, solid[1].data);
    for (width, height, bitrate_kbps) in [(320, 180, 100), (640, 360, 400), (1280, 720, 1_000)] {
        let encoding = NativeH264FixtureEncoding {
            width,
            height,
            bitrate_kbps,
        };
        let spatial = stock_h264_fixture_for_encoding(&world, encoding, 3, 48).unwrap();
        assert_eq!(spatial.len(), 48);
        assert!(spatial[0].keyframe);
        assert!(spatial.iter().all(|unit| unit.width == u32::from(width)
            && unit.height == u32::from(height)
            && !unit.data.is_empty()));
        assert_eq!(
            spatial
                .iter()
                .map(|unit| unit.temporal_index)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([Some(0), Some(1), Some(2)])
        );
        let protected: Vec<_> = spatial
            .iter()
            .filter(|unit| unit.temporal_index.is_some_and(|index| index <= 1))
            .collect();
        assert_eq!(
            protected.len(),
            24,
            "the supplied minimum schedule must actually support15fps"
        );
        assert!(
            spatial
                .windows(2)
                .all(|pair| pair[1].rtp_timestamp - pair[0].rtp_timestamp == 3_000)
        );
        let bytes: u64 = spatial
            .iter()
            .map(|unit| u64::try_from(unit.data.len()).unwrap())
            .sum();
        let protected_bytes: u64 = protected
            .iter()
            .map(|unit| u64::try_from(unit.data.len()).unwrap())
            .sum();
        eprintln!(
            "supplied_h264 width={width} height={height} request_kbps={bitrate_kbps} frames={} total_bytes={bytes} average_bps={} protected_frames={} protected_bps={} max_access_unit_bytes={}",
            spatial.len(),
            bytes * 8 * 30 / 48,
            protected.len(),
            protected_bytes * 8 * 30 / 48,
            spatial.iter().map(|unit| unit.data.len()).max().unwrap()
        );
    }
    for (width, height, bitrate_kbps) in [
        (0, 180, 100),
        (319, 180, 100),
        (320, 181, 100),
        (1282, 720, 1_000),
        (1280, 722, 1_000),
        (320, 180, 0),
        (320, 180, 1_001),
    ] {
        assert!(
            stock_h264_fixture_for_encoding(
                &world,
                NativeH264FixtureEncoding {
                    width,
                    height,
                    bitrate_kbps
                },
                3,
                48
            )
            .is_err()
        );
    }
    for (layers, frames) in [(0, 48), (4, 48), (3, 0), (3, 121)] {
        assert!(stock_h264_fixture(&world, layers, frames).is_err());
    }
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
