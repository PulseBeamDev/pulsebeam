#![cfg_attr(target_arch = "bpf", no_std)]
#![cfg_attr(target_arch = "bpf", no_main)]

mod common;

#[cfg(target_arch = "bpf")]
mod bpf;

#[cfg(not(target_arch = "bpf"))]
fn main() {}

/// Host-target compile checks (per CLAUDE.md's testing guidance, and this
/// crate's own README-equivalent constraint: the eBPF program has no room
/// to host normal unit tests). These do not exercise the BPF verifier or
/// socket steering — that only happens under `make build-ebpf` plus the
/// privileged `ebpf-smoke` CI job. What they do guarantee: the exact
/// `pulsebeam-routing` entry points and argument types the BPF program in
/// `bpf::pulsebeam_client` / `bpf::pulsebeam_node` calls still exist with
/// the signatures this crate assumes, so a breaking change in the shared
/// classifier fails here on stable instead of only at the next nightly
/// eBPF build.
#[cfg(all(not(target_arch = "bpf"), test))]
mod host_tests {
    use pulsebeam_routing::classify::{ClientVerdict, classify_client_for_node, classify_node};
    use pulsebeam_routing::envelope::{Envelope, EnvelopeType};
    use pulsebeam_routing::{RouteId, TransportRoute};

    #[test]
    fn client_classifier_signature_matches_bpf_call_site() {
        let verdict = classify_client_for_node(&[0u8; 4], 0, 0, 0);
        assert_eq!(verdict, ClientVerdict::Established);
    }

    #[test]
    fn node_classifier_signature_matches_bpf_call_site() {
        let env = Envelope {
            ty: EnvelopeType::Media,
            epoch: 1,
            route: RouteId::new(2, 3),
            extension: 0,
        };
        let bytes = env.encode();
        let verdict = classify_node(&bytes, 8);
        assert_eq!(
            verdict,
            pulsebeam_routing::classify::NodeVerdict::Steer { shard: 2 }
        );
    }

    #[test]
    fn counter_index_table_is_dense_and_in_bounds() {
        use crate::common::counters;
        let indices = [
            counters::MALFORMED_STUN,
            counters::MALFORMED_ENVELOPE,
            counters::INVALID_UFRAG,
            counters::INVALID_VERSION,
            counters::INVALID_TYPE,
            counters::WRONG_CLUSTER,
            counters::WRONG_NODE,
            counters::INVALID_SHARD,
            counters::UNKNOWN_FLOW,
            counters::STALE_ROUTE,
            counters::SELECTED,
        ];
        let mut seen = std::collections::HashSet::new();
        for idx in indices {
            assert!(idx < counters::COUNT);
            assert!(seen.insert(idx), "duplicate counter index {idx}");
        }
        assert_eq!(seen.len(), counters::COUNT as usize);
    }

    #[test]
    fn transport_route_shard_stays_within_sockarray_capacity() {
        let route = TransportRoute::new(1, 0);
        assert!(u32::from(route.shard()) < crate::common::MAX_SHARDS);
    }
}
