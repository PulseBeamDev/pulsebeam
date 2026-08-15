use crate::classify::{self, DropReason, NodeVerdict};

pub const MAX_SHARDS: u32 = 1024;
pub const MAX_FLOWS: u32 = 131_072;
pub const SIM_FLOW_CAPACITY: usize = 4096;

const _: () = assert!(MAX_SHARDS > 0);
const _: () = assert!(MAX_FLOWS as usize >= SIM_FLOW_CAPACITY);
const _: () = assert!((MAX_FLOWS as usize).is_multiple_of(SIM_FLOW_CAPACITY));

pub mod counters {
    pub const MALFORMED_STUN: u32 = 0;
    pub const MALFORMED_ENVELOPE: u32 = 1;
    pub const INVALID_UFRAG: u32 = 2;
    pub const INVALID_VERSION: u32 = 3;
    pub const INVALID_TYPE: u32 = 4;
    pub const WRONG_CLUSTER: u32 = 5;
    pub const WRONG_NODE: u32 = 6;
    pub const INVALID_SHARD: u32 = 7;
    pub const UNKNOWN_FLOW: u32 = 8;
    pub const STALE_ROUTE: u32 = 9;
    pub const SELECTED: u32 = 10;
    pub const COUNT: u32 = 11;
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FlowKey {
    pub src_addr: [u8; 16],
    pub dst_addr: [u8; 16],
    pub src_port: u16,
    pub dst_port: u16,
    pub is_ipv6: u8,
    pub _pad: [u8; 3],
}

/// The wire length of a [`FlowKey`]. Named so the copy helper and the layout
/// assertion cannot drift apart.
pub const FLOW_KEY_LEN: usize = 40;

const _: () = assert!(core::mem::size_of::<FlowKey>() == FLOW_KEY_LEN);

impl FlowKey {
    /// The key as the kernel map sees it.
    ///
    /// Written with checked slice ranges rather than indexing: this crate
    /// compiles into a BPF program, where an out-of-range write is not a panic
    /// but a verifier rejection at load time. The offsets are fixed and pinned
    /// by `flow_key_layout_is_stable`; writing them this way means a slip is a
    /// failed test rather than an unloadable program.
    pub fn to_ne_bytes(self) -> [u8; FLOW_KEY_LEN] {
        let mut bytes = [0u8; FLOW_KEY_LEN];
        let mut write = |offset: usize, source: &[u8]| {
            if let Some(target) = bytes.get_mut(offset..offset.saturating_add(source.len())) {
                target.copy_from_slice(source);
            } else {
                debug_assert!(
                    false,
                    "FlowKey field at {offset} lies outside its own layout"
                );
            }
        };
        write(0, &self.src_addr);
        write(16, &self.dst_addr);
        write(32, &self.src_port.to_ne_bytes());
        write(34, &self.dst_port.to_ne_bytes());
        write(36, &[self.is_ipv6]);
        bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
    Select { shard: u16 },
    Pass,
    Drop(DropReason),
}

pub trait SteerEnv {
    fn flow_lookup(&self, flow: FlowKey) -> Option<u16>;
    fn bump(&mut self, counter: u32);
}

pub fn steer_client<E: SteerEnv>(
    env: &mut E,
    flow: FlowKey,
    shard_count: u16,
    max_shards: u32,
) -> Verdict {
    let Some(shard) = env.flow_lookup(flow) else {
        env.bump(counters::UNKNOWN_FLOW);
        return Verdict::Pass;
    };
    if u32::from(shard) >= max_shards {
        env.bump(counters::INVALID_SHARD);
        return Verdict::Pass;
    }
    if shard >= shard_count {
        env.bump(counters::STALE_ROUTE);
        return Verdict::Pass;
    }
    Verdict::Select { shard }
}

pub fn steer_node<E: SteerEnv>(
    env: &mut E,
    payload: &[u8],
    shard_count: u16,
    max_shards: u32,
) -> Verdict {
    match classify::classify_node(payload, shard_count) {
        NodeVerdict::Steer { shard } if u32::from(shard) < max_shards => Verdict::Select { shard },
        NodeVerdict::Steer { .. } => {
            env.bump(counters::INVALID_SHARD);
            Verdict::Drop(DropReason::InvalidShard)
        }
        NodeVerdict::Drop(reason) => {
            env.bump(drop_counter(reason));
            Verdict::Drop(reason)
        }
    }
}

pub const fn drop_counter(reason: DropReason) -> u32 {
    match reason {
        DropReason::NotStun | DropReason::MalformedStun | DropReason::NoUsername => {
            counters::MALFORMED_STUN
        }
        DropReason::BadUfragLen | DropReason::BadUfragEncoding => counters::INVALID_UFRAG,
        DropReason::BadVersion => counters::INVALID_VERSION,
        DropReason::WrongCluster => counters::WRONG_CLUSTER,
        DropReason::WrongNode => counters::WRONG_NODE,
        DropReason::MalformedEnvelope => counters::MALFORMED_ENVELOPE,
        DropReason::UnknownEnvelopeType => counters::INVALID_TYPE,
        DropReason::InvalidShard => counters::INVALID_SHARD,
        DropReason::UnknownFlow => counters::UNKNOWN_FLOW,
        DropReason::StaleRoute => counters::STALE_ROUTE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        RouteId,
        envelope::{Envelope, EnvelopeType},
    };
    use std::collections::HashMap;

    #[derive(Default)]
    struct Env {
        flows: HashMap<FlowKey, u16>,
        counters: [u32; counters::COUNT as usize],
    }

    impl SteerEnv for Env {
        fn flow_lookup(&self, flow: FlowKey) -> Option<u16> {
            self.flows.get(&flow).copied()
        }

        fn bump(&mut self, counter: u32) {
            let Some(value) = self.counters.get_mut(counter as usize) else {
                panic!("counter index out of bounds");
            };
            *value += 1;
        }
    }

    fn flow(value: u8) -> FlowKey {
        FlowKey {
            src_addr: [value; 16],
            dst_addr: [value.wrapping_add(1); 16],
            src_port: 1000,
            dst_port: 3478,
            is_ipv6: 0,
            _pad: [0; 3],
        }
    }

    #[test]
    fn client_miss_falls_through_without_selecting_or_dropping() {
        let mut env = Env::default();
        assert_eq!(
            steer_client(&mut env, flow(1), 4, MAX_SHARDS),
            Verdict::Pass
        );
        assert_eq!(env.counters[counters::UNKNOWN_FLOW as usize], 1);
    }

    #[test]
    fn client_hit_selects_only_a_current_shard() {
        let mut env = Env::default();
        env.flows.insert(flow(1), 2);
        assert_eq!(
            steer_client(&mut env, flow(1), 4, MAX_SHARDS),
            Verdict::Select { shard: 2 }
        );
    }

    #[test]
    fn stale_and_invalid_hits_also_fall_through() {
        let mut env = Env::default();
        env.flows.insert(flow(1), 4);
        assert_eq!(
            steer_client(&mut env, flow(1), 4, MAX_SHARDS),
            Verdict::Pass
        );
        env.flows
            .insert(flow(1), u16::try_from(MAX_SHARDS).unwrap());
        assert_eq!(
            steer_client(&mut env, flow(1), 4, MAX_SHARDS),
            Verdict::Pass
        );
        assert_eq!(env.counters[counters::STALE_ROUTE as usize], 1);
        assert_eq!(env.counters[counters::INVALID_SHARD as usize], 1);
    }

    #[test]
    fn node_steering_checks_runtime_and_map_bounds() {
        let mut env = Env::default();
        let envelope = Envelope {
            ty: EnvelopeType::Media,
            epoch: 1,
            route: RouteId::new(3, 1),
            extension: 0,
        }
        .encode();
        assert_eq!(
            steer_node(&mut env, &envelope, 4, 3),
            Verdict::Drop(DropReason::InvalidShard)
        );
        assert_eq!(env.counters[counters::INVALID_SHARD as usize], 1);
    }
}
