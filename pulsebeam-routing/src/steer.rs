use crate::classify::{self, ClientVerdict, DropReason, NodeVerdict};

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

const _: () = assert!(core::mem::size_of::<FlowKey>() == 40);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
    Pass { shard: u16 },
    Drop(DropReason),
}

pub trait SteerEnv {
    fn flow_lookup(&self, flow: FlowKey) -> Option<u16>;
    fn flow_insert(&mut self, flow: FlowKey, shard: u16);
    fn bump(&mut self, counter: u32);
}

pub fn steer_client<E: SteerEnv>(
    env: &mut E,
    payload: &[u8],
    flow: FlowKey,
    cluster_id: u16,
    node_id: u16,
    shard_count: u16,
    max_shards: u32,
) -> Verdict {
    match classify::classify_client_for_node(payload, cluster_id, node_id, shard_count) {
        ClientVerdict::Bootstrap { handle, .. } => {
            let shard = handle.route.shard();
            if u32::from(shard) >= max_shards {
                env.bump(counters::INVALID_SHARD);
                return Verdict::Drop(DropReason::InvalidShard);
            }
            env.flow_insert(flow, shard);
            Verdict::Pass { shard }
        }
        ClientVerdict::Established => match env.flow_lookup(flow) {
            Some(shard) if u32::from(shard) >= max_shards => {
                env.bump(counters::INVALID_SHARD);
                Verdict::Drop(DropReason::InvalidShard)
            }
            Some(shard) if shard >= shard_count => {
                env.bump(counters::STALE_ROUTE);
                Verdict::Drop(DropReason::StaleRoute)
            }
            Some(shard) => Verdict::Pass { shard },
            None => {
                env.bump(counters::UNKNOWN_FLOW);
                Verdict::Drop(DropReason::UnknownFlow)
            }
        },
        ClientVerdict::Drop(reason) => {
            env.bump(drop_counter(reason));
            Verdict::Drop(reason)
        }
    }
}

pub fn steer_node<E: SteerEnv>(
    env: &mut E,
    payload: &[u8],
    shard_count: u16,
    max_shards: u32,
) -> Verdict {
    match classify::classify_node(payload, shard_count) {
        NodeVerdict::Steer { shard } if u32::from(shard) < max_shards => Verdict::Pass { shard },
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
    use crate::envelope::{Envelope, EnvelopeType};
    use crate::ufrag::IceUfrag;
    use crate::{RouteId, TransportRoute};
    use std::collections::HashMap;
    use std::vec::Vec;

    #[derive(Default)]
    struct Env {
        flows: HashMap<FlowKey, u16>,
        counters: [u32; counters::COUNT as usize],
    }

    impl SteerEnv for Env {
        fn flow_lookup(&self, flow: FlowKey) -> Option<u16> {
            self.flows.get(&flow).copied()
        }

        fn flow_insert(&mut self, flow: FlowKey, shard: u16) {
            self.flows.insert(flow, shard);
        }

        fn bump(&mut self, counter: u32) {
            let Some(value) = self.counters.get_mut(counter as usize) else {
                panic!("counter index out of bounds");
            };
            *value += 1;
        }
    }

    struct KernelAdapter(Env);

    struct SimulatorAdapter {
        flows: Vec<(FlowKey, u16)>,
        counters: [u32; counters::COUNT as usize],
    }

    impl SteerEnv for KernelAdapter {
        fn flow_lookup(&self, flow: FlowKey) -> Option<u16> {
            self.0.flow_lookup(flow)
        }

        fn flow_insert(&mut self, flow: FlowKey, shard: u16) {
            self.0.flow_insert(flow, shard);
        }

        fn bump(&mut self, counter: u32) {
            self.0.bump(counter);
        }
    }

    impl SteerEnv for SimulatorAdapter {
        fn flow_lookup(&self, flow: FlowKey) -> Option<u16> {
            self.flows
                .iter()
                .find_map(|(known, shard)| (*known == flow).then_some(*shard))
        }

        fn flow_insert(&mut self, flow: FlowKey, shard: u16) {
            if let Some((_, known_shard)) = self.flows.iter_mut().find(|(known, _)| *known == flow)
            {
                *known_shard = shard;
            } else {
                self.flows.push((flow, shard));
            }
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

    fn stun_for(cluster: u16, node: u16, shard: u16) -> Vec<u8> {
        let ufrag = IceUfrag::new(cluster, node, TransportRoute::new(shard, 9), 1);
        let mut username = Vec::from(ufrag.encode_ascii());
        username.extend_from_slice(b":peer");
        let padded = (username.len() + 3) & !3;
        let mut packet = Vec::with_capacity(20 + 4 + padded);
        packet.extend_from_slice(&1u16.to_be_bytes());
        packet.extend_from_slice(&u16::try_from(4 + padded).unwrap().to_be_bytes());
        packet.extend_from_slice(&crate::stun::MAGIC_COOKIE.to_be_bytes());
        packet.extend_from_slice(&[0; 12]);
        packet.extend_from_slice(&6u16.to_be_bytes());
        packet.extend_from_slice(&u16::try_from(username.len()).unwrap().to_be_bytes());
        packet.extend_from_slice(&username);
        packet.resize(20 + 4 + padded, 0);
        packet
    }

    fn stun(shard: u16) -> Vec<u8> {
        stun_for(3, 5, shard)
    }

    #[test]
    fn client_reclassifies_bootstrap_before_flow_lookup() {
        let mut env = Env::default();
        let flow = flow(1);
        assert_eq!(
            steer_client(&mut env, &stun(1), flow, 3, 5, 4, MAX_SHARDS),
            Verdict::Pass { shard: 1 }
        );
        assert_eq!(
            steer_client(&mut env, &stun(3), flow, 3, 5, 4, MAX_SHARDS),
            Verdict::Pass { shard: 3 }
        );
        assert_eq!(env.flow_lookup(flow), Some(3));
    }

    #[test]
    fn established_unknown_flow_is_counted_and_dropped() {
        let mut env = Env::default();
        let verdict = steer_client(&mut env, b"media", flow(2), 3, 5, 4, MAX_SHARDS);
        assert!(matches!(verdict, Verdict::Drop(_)));
        assert_eq!(env.counters[counters::UNKNOWN_FLOW as usize], 1);
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
            Verdict::Drop(crate::classify::DropReason::InvalidShard)
        );
        assert_eq!(env.counters[counters::INVALID_SHARD as usize], 1);
    }

    #[test]
    fn kernel_and_simulator_adapters_make_identical_decisions() {
        let mut kernel = KernelAdapter(Env::default());
        let mut simulator = SimulatorAdapter {
            flows: Vec::new(),
            counters: [0; counters::COUNT as usize],
        };
        let sequence = [
            (stun(1), flow(1)),
            (b"media".to_vec(), flow(1)),
            (stun_for(9, 5, 1), flow(2)),
            (stun_for(3, 9, 1), flow(3)),
            (stun(99), flow(4)),
            (std::vec![0; 20], flow(5)),
            (stun(2), flow(1)),
            (b"media".to_vec(), flow(99)),
        ];
        for (payload, flow) in sequence {
            let kernel_verdict = steer_client(&mut kernel, &payload, flow, 3, 5, 4, MAX_SHARDS);
            let simulator_verdict =
                steer_client(&mut simulator, &payload, flow, 3, 5, 4, MAX_SHARDS);
            assert_eq!(kernel_verdict, simulator_verdict);
            assert_eq!(kernel.0.counters, simulator.counters);
        }
    }
}
