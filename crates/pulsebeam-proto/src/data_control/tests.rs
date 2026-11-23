use super::*;

const SOURCE: NonZeroU64 = NonZeroU64::MIN;
const BRANCH: NonZeroU64 = NonZeroU64::MIN.saturating_add(1);
const SUBSCRIBER: SubscriberBinding = SubscriberBinding {
    source: SOURCE,
    branch: BRANCH,
};

fn unit() -> BranchUnit {
    BranchUnit {
        source: Some(SourceUnit {
            source_incarnation: SOURCE.get(),
            unit_seq: Some(0),
        }),
        branch_incarnation: BRANCH.get(),
    }
}

#[test]
fn literal_expectation_preserves_zero_identity_and_matches_exact_lifetime() {
    let wire = [
        0x0a, 0x21, 0x0a, 0x1d, 0x0a, 0x12, // expectation, branch, source
        0x09, 1, 0, 0, 0, 0, 0, 0, 0, // source incarnation
        0x11, 0, 0, 0, 0, 0, 0, 0, 0, // present zero unit sequence
        0x11, 2, 0, 0, 0, 0, 0, 0, 0, // branch incarnation
        0x10, 64, // fragment count
    ];
    let value = SubscriberObservation {
        record: Some(subscriber_observation::Record::Expectation(Expectation {
            unit: Some(unit()),
            fragment_count: 64,
        })),
    };
    assert_eq!(encode(SUBSCRIBER, &value).as_deref(), Ok(&wire[..]));
    assert_eq!(decode(SUBSCRIBER, &wire), Ok(value));
    for changed in [
        SubscriberBinding {
            source: BRANCH,
            ..SUBSCRIBER
        },
        SubscriberBinding {
            branch: SOURCE,
            ..SUBSCRIBER
        },
    ] {
        assert_eq!(
            decode::<SubscriberObservation>(changed, &wire),
            Err(Error::Binding)
        );
    }
    assert!(decode::<SubscriberControl>(SUBSCRIBER, &wire).is_err());

    let mut extra = wire.to_vec();
    extra.extend([0x20, 1]);
    assert_eq!(
        decode::<SubscriberObservation>(SUBSCRIBER, &extra),
        Err(Error::Encoding)
    );
    let mut duplicate = wire.to_vec();
    duplicate.extend(wire);
    assert_eq!(
        decode::<SubscriberObservation>(SUBSCRIBER, &duplicate),
        Err(Error::Encoding)
    );
    assert_eq!(
        decode::<SubscriberObservation>(SUBSCRIBER, &[0; MAX_CONTROL_FRAME_BYTES + 1]),
        Err(Error::FrameSize)
    );
}

#[test]
fn repair_identity_and_publisher_admission_are_not_delivery_receipts() {
    let mut demand = RepairDemand {
        unit: Some(unit()),
        fragment_index: Some(0),
        request_id: u64::MAX,
    };
    let cancel = |demand| SubscriberControl {
        record: Some(subscriber_control::Record::CancelRepair(demand)),
    };
    assert!(encode(SUBSCRIBER, &cancel(demand)).is_ok());
    demand.fragment_index = Some(64);
    assert_eq!(encode(SUBSCRIBER, &cancel(demand)), Err(Error::Structure));
    demand.fragment_index = None;
    assert_eq!(encode(SUBSCRIBER, &cancel(demand)), Err(Error::Structure));
    demand.fragment_index = Some(63);
    demand.request_id = 0;
    assert_eq!(encode(SUBSCRIBER, &cancel(demand)), Err(Error::Structure));

    let publication = PublisherBinding { source: SOURCE };
    let admission = |unit_seq| PublisherObservation {
        record: Some(publisher_observation::Record::Admission(SourceUnit {
            source_incarnation: SOURCE.get(),
            unit_seq,
        })),
    };
    let wire = [
        0x0a, 0x12, 0x09, 1, 0, 0, 0, 0, 0, 0, 0, 0x11, 0, 0, 0, 0, 0, 0, 0, 0,
    ];
    assert_eq!(
        encode(publication, &admission(Some(0))).as_deref(),
        Ok(&wire[..])
    );
    assert_eq!(encode(publication, &admission(None)), Err(Error::Structure));
    assert!(decode::<SubscriberControl>(SUBSCRIBER, &wire).is_err());
}
