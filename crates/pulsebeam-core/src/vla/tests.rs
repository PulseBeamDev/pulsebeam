use proptest::prelude::*;

use super::*;

const SPARSE: &[u8] = &[
    0x60, 0x21, 0x40, 0x54, 1, 2, 3, 4, 5, 6, 0x02, 0x7f, 0x01, 0x67, 30, 0x04, 0xff, 0x02, 0xcf,
    30, 0x07, 0x7f, 0x04, 0x37, 60,
];

fn layer(rates: &[u64], size: Option<(u32, u32, u8)>) -> SpatialLayerAllocation {
    SpatialLayerAllocation {
        temporal_layers: rates
            .iter()
            .map(|rate| TemporalLayerAllocation {
                cumulative_kbps: *rate,
            })
            .collect(),
        resolution_and_framerate: size.map(|(width, height, framerate)| ResolutionAndFramerate {
            width,
            height,
            framerate,
        }),
    }
}

#[test]
fn sparse_spatial_positions_and_optional_dimensions() -> Result<(), VlaReadError> {
    let mut expected = VideoLayersAllocation {
        current_simulcast_stream_index: 1,
        simulcast_streams: vec![
            SimulcastStreamAllocation {
                spatial_layers: vec![layer(&[], None), layer(&[1, 2], Some((640, 360, 30)))],
            },
            SimulcastStreamAllocation {
                spatial_layers: vec![layer(&[3, 4], Some((1280, 720, 30)))],
            },
            SimulcastStreamAllocation {
                spatial_layers: vec![
                    layer(&[], None),
                    layer(&[], None),
                    layer(&[5, 6], Some((1920, 1080, 60))),
                ],
            },
        ],
    };
    assert_eq!(VideoLayersAllocation::parse(SPARSE), Ok(expected.clone()));
    for stream in &mut expected.simulcast_streams {
        for spatial in &mut stream.spatial_layers {
            spatial.resolution_and_framerate = None;
        }
    }
    let no_dimensions = SPARSE.get(..10).ok_or(VlaReadError::Truncated)?;
    assert_eq!(VideoLayersAllocation::parse(no_dimensions), Ok(expected));
    for length in 0..SPARSE.len() {
        if length != 10 {
            let prefix = SPARSE.get(..length).ok_or(VlaReadError::Truncated)?;
            assert!(
                VideoLayersAllocation::parse(prefix).is_err(),
                "prefix {length}"
            );
        }
    }
    Ok(())
}

#[test]
fn inactive_allocations() {
    assert_eq!(
        VideoLayersAllocation::parse(&[0]),
        Ok(VideoLayersAllocation {
            current_simulcast_stream_index: 0,
            simulcast_streams: Vec::new(),
        })
    );
    assert_eq!(
        VideoLayersAllocation::parse(&[0x20, 0, 0]),
        Ok(VideoLayersAllocation {
            current_simulcast_stream_index: 0,
            simulcast_streams: vec![
                SimulcastStreamAllocation {
                    spatial_layers: Vec::new()
                };
                3
            ],
        })
    );
}

#[test]
fn rate_and_dimension_boundaries() {
    let mut wire = vec![1, 0];
    wire.extend_from_slice(&[0xff; 8]);
    wire.push(0x7f);
    wire.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 255]);
    assert_eq!(
        VideoLayersAllocation::parse(&wire),
        Ok(VideoLayersAllocation {
            current_simulcast_stream_index: 0,
            simulcast_streams: vec![SimulcastStreamAllocation {
                spatial_layers: vec![layer(&[u64::MAX >> 1], Some((65536, 65536, 255)))],
            }],
        })
    );
    wire.clear();
    wire.extend_from_slice(&[1, 0]);
    wire.extend_from_slice(&[0x80; 9]);
    wire.push(0);
    assert_eq!(
        VideoLayersAllocation::parse(&wire),
        Err(VlaReadError::Invalid)
    );
}

#[test]
fn rejects_invalid_counts_rates_and_tails() {
    for wire in [&[][..], &[1], &[1, 0], &[1, 0, 0x80]] {
        assert_eq!(
            VideoLayersAllocation::parse(wire),
            Err(VlaReadError::Truncated)
        );
    }
    assert_eq!(
        VideoLayersAllocation::parse(&[0xc1, 0, 1]),
        Err(VlaReadError::Invalid)
    );
    assert_eq!(
        VideoLayersAllocation::parse(&[0, 0, 1]),
        Err(VlaReadError::TrailingBytes)
    );
    assert_eq!(
        VideoLayersAllocation::parse(&[0; MAX_VLA_LEN + 1]),
        Err(VlaReadError::TooLong)
    );
}

#[cfg(feature = "str0m")]
fn from_native(value: &str0m::rtp::vla::VideoLayersAllocation) -> VideoLayersAllocation {
    VideoLayersAllocation {
        current_simulcast_stream_index: value.current_simulcast_stream_index,
        simulcast_streams: value
            .simulcast_streams
            .iter()
            .map(|stream| SimulcastStreamAllocation {
                spatial_layers: stream
                    .spatial_layers
                    .iter()
                    .map(|spatial| SpatialLayerAllocation {
                        temporal_layers: spatial
                            .temporal_layers
                            .iter()
                            .map(|temporal| TemporalLayerAllocation {
                                cumulative_kbps: temporal.cumulative_kbps,
                            })
                            .collect(),
                        resolution_and_framerate: spatial.resolution_and_framerate.as_ref().map(
                            |size| ResolutionAndFramerate {
                                width: u32::from(size.width),
                                height: u32::from(size.height),
                                framerate: size.framerate,
                            },
                        ),
                    })
                    .collect(),
            })
            .collect(),
    }
}

#[cfg(feature = "str0m")]
#[test]
fn existing_decoder_agrees_on_sparse_positions() -> Result<(), VlaReadError> {
    use str0m::rtp::{ExtensionSerializer, ExtensionValues, vla as native};
    let mut fixtures = vec![SPARSE.to_vec()];
    for mask in 1u8..16 {
        let mut wire = vec![mask, 0];
        wire.extend(1..=u8::try_from(mask.count_ones()).map_err(|_| VlaReadError::Invalid)?);
        fixtures.push(wire);
    }
    for wire in fixtures {
        let expected = VideoLayersAllocation::parse(&wire)?;
        let mut values = ExtensionValues::default();
        assert!(native::Serializer.parse_value(&wire, &mut values));
        assert_eq!(
            values
                .user_values
                .get::<native::VideoLayersAllocation>()
                .map(from_native),
            Some(expected)
        );
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn hostile_input_stays_bounded(bytes in prop::collection::vec(any::<u8>(), 0..=300)) {
        if let Ok(value) = VideoLayersAllocation::parse(&bytes) {
            prop_assert!(bytes.len() <= MAX_VLA_LEN);
            prop_assert!(value.simulcast_streams.len() <= 4);
            for stream in value.simulcast_streams {
                prop_assert!(stream.spatial_layers.len() <= 4);
                for spatial in stream.spatial_layers {
                    prop_assert!(spatial.temporal_layers.len() <= 4);
                    for temporal in spatial.temporal_layers {
                        prop_assert!(temporal.cumulative_kbps <= u64::MAX >> 1);
                    }
                    if let Some(size) = spatial.resolution_and_framerate {
                        prop_assert!((1..=65536).contains(&size.width));
                        prop_assert!((1..=65536).contains(&size.height));
                    }
                }
            }
        }
    }

    #[cfg(feature = "str0m")]
    #[test]
    fn decodes_existing_client_wire(
        shared_mask in 1u8..16,
        stream_count in 1u8..=4,
        current in 0u8..4,
        temporal_count in 1u8..=4,
        base in 0u16..=4000,
        dimensions in any::<bool>(),
    ) {
        use str0m::rtp::{ExtensionSerializer, ExtensionValues, vla as native};
        let allocation = native::VideoLayersAllocation {
            // The old native writer misencodes explicit masks for 3+ streams;
            // SPARSE above is the independent wire proof for that branch.
            current_simulcast_stream_index: current.min(stream_count - 1),
            simulcast_streams: (0..stream_count).map(|stream| native::SimulcastStreamAllocation {
                spatial_layers: (0..u8::BITS - shared_mask.leading_zeros()).map(|spatial| {
                    let active = shared_mask & 1u8.checked_shl(spatial).unwrap_or(0) != 0;
                    let count = (u32::from(temporal_count) + spatial + u32::from(stream)) % 4 + 1;
                    native::SpatialLayerAllocation {
                        temporal_layers: if active {
                            (1..=count).map(|temporal| native::TemporalLayerAllocation {
                                cumulative_kbps: u64::from(base) * u64::from(temporal),
                            }).collect()
                        } else { Vec::new() },
                        resolution_and_framerate: (active && dimensions).then_some(native::ResolutionAndFramerate {
                            width: 640, height: 360, framerate: 30,
                        }),
                    }
                }).collect(),
            }).collect(),
        };
        let expected = from_native(&allocation);
        let mut values = ExtensionValues::default();
        values.user_values.set(allocation);
        let mut bytes = [0; MAX_VLA_LEN];
        let length = native::Serializer.write_to(&mut bytes, &values);
        let wire = bytes.get(..length).ok_or_else(|| TestCaseError::fail("native serializer exceeded extension bound"))?;
        prop_assert_eq!(VideoLayersAllocation::parse(wire), Ok(expected));
    }
}
