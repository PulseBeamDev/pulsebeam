use super::*;
use crate::tests::Pair;

#[test]
fn quic_enqueue_commits_the_receiver_report_token() {
    // Direct receiver observations probe token ownership, not transport input readiness.
    let mut pair = Pair::quic(false);
    pair.connect();
    let arrival = Duration::from_nanos(pair.now.0);
    pair.b
        .receiver
        .observe(
            EPOCH,
            feedback::Receipt {
                space: SPACE,
                packet: feedback::PacketId(0),
                arrived_at: arrival,
                ecn: None,
                tag: 0,
            },
        )
        .unwrap();
    let due = arrival + pair.b.health.feedback_interval;
    assert!(pair.b.prepare_feedback(due).unwrap());
    pair.b
        .receiver
        .observe(
            EPOCH,
            feedback::Receipt {
                space: SPACE,
                packet: feedback::PacketId(1),
                arrived_at: due,
                ecn: None,
                tag: 1,
            },
        )
        .unwrap();
    pair.b
        .poll(
            Time(u64::try_from(due.as_nanos()).unwrap()),
            256,
            |datagram| SendOutcome::Accepted {
                bytes: datagram.payload.len(),
            },
            |_| {},
        )
        .unwrap();
    let next = pair
        .b
        .receiver
        .report(due + pair.b.health.feedback_interval)
        .unwrap();
    assert_eq!(next.base, feedback::PacketId(1));
    assert_eq!(next.count, 1);
}
