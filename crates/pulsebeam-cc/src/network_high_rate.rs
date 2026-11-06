use super::{Host, Link};
use std::time::Duration;

#[test]
fn accepted_full_datagrams_grow_past_old_rate_policy() {
    let mut host = Host::new();
    host.source = 2_000_000_000;
    host.demand = 2_000_000_000;
    host.report_interval_ms = 1;
    let link = Link {
        capacity: 4_000_000_000,
        rtt: Duration::from_millis(1),
        ..Link::default()
    };
    for _ in 0..15_000 {
        host.step(link);
        if host.max_target > 100_000_000 {
            break;
        }
    }
    host.run(100, link);
    assert!(
        host.max_target > 100_000_000,
        "uncapped evidence-driven growth: {}",
        host.max_target
    );
    assert_eq!(host.cc.diagnostics().charged_bytes, host.wire);
}
