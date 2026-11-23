use super::*;

fn source(positions: u16, anchors: Anchors) -> MediaSource {
    MediaSource::new(
        Profile::H264,
        anchors,
        Limits {
            positions: NonZeroU16::new(positions).unwrap(),
            max_age: Duration::from_millis(120),
            max_idle: Duration::from_secs(2),
        },
    )
    .unwrap()
}

fn media<'a>(repair: bool, timestamp: u32, extensions: &'a [Extension<'a>]) -> Input<'a> {
    Input::Media(MediaCopy {
        is_repair: repair,
        marker: true,
        timestamp,
        payload: b"opaque",
        extensions,
        extension_width: None,
    })
}

fn dd(value: &[u8]) -> Extension<'_> {
    Extension {
        kind: ExtensionKind::DependencyDescriptor,
        value,
    }
}

#[test]
fn authenticated_primary_resumes_idle_without_reopening_old_positions() {
    let mut identity = source(8, Anchors::default()).into_identity();
    let now = Instant::now();
    let idle = now + Duration::from_secs(3);
    let mut admit = |at, position, input| identity.admit(at, at, 0, position, input, |_| None);
    assert!(matches!(
        admit(
            now,
            Position::AuthenticatedPrimary(65535),
            media(false, 500, &[])
        ),
        Ok(IdentityAdmission::Media {
            sequence: 65535,
            ..
        })
    ));
    assert!(matches!(
        admit(idle, Position::LowSequence(0), media(true, 501, &[])),
        Err(Rejected::Epoch)
    ));
    assert!(matches!(
        admit(
            idle,
            Position::AuthenticatedPrimary(65536),
            Input::PrimaryPadding
        ),
        Err(Rejected::Epoch)
    ));
    assert!(matches!(
        admit(
            idle,
            Position::AuthenticatedPrimary(65534),
            media(false, 499, &[])
        ),
        Err(Rejected::Epoch)
    ));
    assert!(matches!(
        admit(
            idle,
            Position::AuthenticatedPrimary(65537),
            media(false, 502, &[])
        ),
        Ok(IdentityAdmission::Media {
            sequence: 65537,
            ..
        })
    ));
    assert!(matches!(
        admit(idle, Position::LowSequence(0), media(true, 501, &[])),
        Err(Rejected::Stale)
    ));
    assert_eq!(identity.oldest(), 65537);
}

#[test]
fn rtx_first_freezes_image_but_later_primary_fixes_presence() {
    let now = Instant::now();
    let mut source = source(4, Anchors::default());
    let first = source
        .admit(now, now, 10, u16::MAX, media(true, 500, &[]))
        .unwrap();
    let Admission::Media {
        first: true,
        packet,
        sequence: 65535,
        arrival,
    } = first
    else {
        panic!("first RTX should seed cycle zero: {first:?}");
    };
    assert_eq!(arrival, now);
    let frozen = packet.clone();
    let retry = now + Duration::from_millis(1);
    let with_dd = [dd(&[0x80, 0, 1])];
    for (repair, extensions) in [(false, with_dd.as_slice()), (true, &[]), (false, &with_dd)] {
        assert_eq!(
            source.admit(retry, retry, 20, u16::MAX, media(repair, 500, extensions)),
            Ok(Admission::Media {
                first: false,
                sequence: 65535,
                arrival: now,
                packet: &frozen
            })
        );
    }
    assert_eq!(frozen.header().ingress_ntp32, 10);
    assert_eq!(frozen.extensions().count(), 0);
    assert!(matches!(
        source.admit(now, now, 30, 0, media(false, 502, &[])),
        Ok(Admission::Media {
            first: true,
            sequence: 65536,
            ..
        })
    ));
    assert_eq!(
        source.admit(now, now, 31, u16::MAX, media(false, 500, &[])),
        Err(Rejected::Conflict)
    );
    assert_eq!(
        source.admit(now, now, 40, 1, media(false, 503, &[])),
        Err(Rejected::Fenced)
    );
}

#[test]
fn identity_retries_require_the_owners_exact_retained_original() {
    let now = Instant::now();
    let mut source = source(16, Anchors::default());
    let identity = &mut source.identity;
    let IdentityAdmission::Media {
        sequence,
        original: Some(original),
        ..
    } = identity
        .admit(
            now,
            now,
            10,
            Position::LowSequence(5),
            media(true, 30, &[]),
            |_| None,
        )
        .unwrap()
    else {
        panic!("first admission did not transfer an original");
    };
    assert_eq!(sequence, 5);
    let later = now + Duration::from_millis(1);
    assert!(matches!(
        identity.admit(
            later,
            later,
            11,
            Position::LowSequence(5),
            media(false, 30, &[]),
            |_| None
        ),
        Err(Rejected::Stale)
    ));
    assert!(matches!(
        identity.admit(later, later, 12, Position::LowSequence(5), media(false, 30, &[]), |position| {
            assert_eq!(position, sequence);
            Some(&original)
        }),
        Ok(IdentityAdmission::Media { sequence: 5, arrival, original: None }) if arrival == now
    ));
    let changed = media(false, 31, &[]);
    let result = identity.admit(later, later, 12, Position::LowSequence(5), changed, |_| {
        Some(&original)
    });
    assert!(matches!(result, Err(Rejected::Conflict)));
}

#[test]
fn a_value_first_seen_on_repair_must_match_later_primary() {
    let now = Instant::now();
    let mut source = source(4, Anchors::default());
    source.admit(now, now, 10, 5, media(true, 30, &[])).unwrap();
    source
        .admit(now, now, 20, 5, media(true, 30, &[dd(&[0x80, 0, 1])]))
        .unwrap();
    assert_eq!(
        source.admit(now, now, 30, 5, media(false, 30, &[dd(&[0x80, 0, 2])])),
        Err(Rejected::Conflict)
    );
}

#[test]
fn wrap_window_ambiguity_and_idle_do_not_reopen_positions() {
    let now = Instant::now();
    let mut source = source(4, Anchors::default());
    source
        .admit(now, now, 10, 1, media(false, 500, &[]))
        .unwrap();
    assert_eq!(
        source.admit(now, now, 20, u16::MAX, media(true, 500, &[])),
        Err(Rejected::PreOrigin)
    );
    assert_eq!(
        source.admit(now, now, 20, 32769, media(true, 500, &[])),
        Err(Rejected::Ambiguous)
    );
    assert_eq!(
        source.admit(now, now, 20, 4098, media(true, 500, &[])),
        Err(Rejected::Epoch)
    );
    for (seq, input) in [(2, Input::PrimaryPadding), (6, media(false, 503, &[]))] {
        source.admit(now, now, 30, seq, input).unwrap();
    }
    assert_eq!(source.identity.ledger.len(), 1);
    assert_eq!(source.originals.len(), 1);
    assert_eq!(
        source.admit(now, now, 30, 2, media(true, 503, &[])),
        Err(Rejected::Stale)
    );
    let late = now + Duration::from_millis(121);
    assert_eq!(
        source.admit(late, late, 30, 6, media(true, 503, &[])),
        Err(Rejected::Stale)
    );
    let idle = now + Duration::from_secs(3);
    assert_eq!(
        source.admit(idle, idle, 30, 7, media(false, 503, &[])),
        Err(Rejected::Epoch)
    );
}

#[test]
fn padding_identity_and_current_wire_validation_precede_mutation() {
    let now = Instant::now();
    let mut source = source(4, Anchors::default());
    let empty = Input::Media(MediaCopy {
        is_repair: false,
        marker: false,
        timestamp: 25,
        payload: &[],
        extensions: &[],
        extension_width: None,
    });
    assert_eq!(
        source.admit(now, now, 5, u16::MAX, empty),
        Err(Rejected::Wire(WireError::PayloadLength))
    );
    assert_eq!(source.identity.high, None);
    assert!(source.identity.ledger.is_empty());
    assert!(source.originals.is_empty());
    assert_eq!(
        source.admit(now, now, 8, 0, Input::PrimaryPadding),
        Ok(Admission::Padding)
    );
    assert_eq!(
        source.admit(now, now, 9, 0, Input::PrimaryPadding),
        Ok(Admission::Padding)
    );
    assert!(source.identity.covers_padding(0..1));
    assert!(!source.identity.covers_padding(0..2));
    assert_eq!(
        source.admit(now, now, 10, 0, media(false, 500, &[])),
        Err(Rejected::Conflict)
    );
    assert!(!source.identity.covers_padding(0..1));
}

#[test]
fn primary_first_across_wrap_preserves_padding_hole_and_frozen_repair() {
    let now = Instant::now();
    let descriptor = [dd(&[0x80, 0, 1])];
    for order in [[u16::MAX, 0], [0, u16::MAX]] {
        let mut source = source(4, Anchors::default());
        let first = source
            .admit(now, now, 11, u16::MAX - 1, media(false, 500, &descriptor))
            .unwrap();
        let Admission::Media { packet, .. } = first else {
            panic!("primary media expected");
        };
        let frozen = packet.clone();
        for position in order {
            let input = match position {
                0 => media(false, 502, &[]),
                _ => Input::PrimaryPadding,
            };
            let admitted = source.admit(now, now, 22, position, input).unwrap();
            match position {
                0 => assert!(matches!(
                    admitted,
                    Admission::Media {
                        sequence: 65536,
                        first: true,
                        ..
                    }
                )),
                _ => assert_eq!(admitted, Admission::Padding),
            }
        }
        assert_eq!(
            source.admit(
                now,
                now,
                33,
                u16::MAX - 1,
                media(true, 500, &[dd(&[0x80, 0])])
            ),
            Err(Rejected::Wire(WireError::ExtensionValue))
        );
        assert_eq!(
            source.admit(now, now, 44, u16::MAX - 1, media(true, 500, &[])),
            Ok(Admission::Media {
                sequence: 65534,
                first: false,
                arrival: now,
                packet: &frozen
            })
        );
        assert_eq!(frozen.header().ingress_ntp32, 11);
        assert_eq!(frozen.extensions().next(), descriptor.first().copied());
        assert!(source.identity.covers_padding(65535..65536));
        assert!(!source.identity.covers_padding(65534..65536));
        assert!(!source.identity.covers_padding(65535..65537));
        assert_eq!(
            source.admit(now, now, 55, u16::MAX, media(true, 501, &[])),
            Err(Rejected::Conflict)
        );
    }
}

#[test]
fn authenticated_anchors_and_clock_ambiguity_leave_raw_timestamp_intact() {
    let now = Instant::now();
    let mut source = source(
        4,
        Anchors {
            sequence: Some(0xffff_ffff),
            timestamp: Some(0xffff_ffff),
        },
    );
    let first = source.admit(now, now, 1, 1, media(false, 0, &[])).unwrap();
    assert!(
        matches!(first, Admission::Media { sequence: 0x1_0000_0001, packet, .. } if packet.header().sequence == 1)
    );
    assert_eq!(source.extended_timestamp(0), Some(0x1_0000_0000));
    let half = 1 << 31;
    let tied = source
        .admit(now, now, 2, 2, media(false, half, &[]))
        .unwrap();
    assert!(matches!(tied, Admission::Media { packet, .. } if packet.header().timestamp == half));
    assert_eq!(source.extended_timestamp(0), None);
    assert!(source.admit(now, now, 3, 3, media(false, 7, &[])).is_ok());
    assert_eq!(source.extended_timestamp(7), None);
}
