//! The two `SK_REUSEPORT` programs and the maps they share. Every read off
//! the wire goes through `net::parse_udp` (bounds-checked) and then
//! `pulsebeam_routing::classify` (the same classifier the userspace demuxer
//! and the simulator use) — this module owns socket selection only, not a
//! second copy of STUN/Envelope parsing.

mod net;

use aya_ebpf::{
    macros::{map, sk_reuseport},
    maps::{LruHashMap, PerCpuArray, ReusePortSockArray},
    programs::SkReuseportContext,
};
use pulsebeam_routing::steer::{self, FlowKey, SteerEnv, Verdict};

use crate::common::{MAX_FLOWS, MAX_SHARDS, counters};
use net::parse_udp;

const SK_PASS: u32 = aya_ebpf::bindings::sk_action::SK_PASS;
const SK_DROP: u32 = aya_ebpf::bindings::sk_action::SK_DROP;

#[unsafe(no_mangle)]
static CLUSTER_ID: u16 = 0;
#[unsafe(no_mangle)]
static NODE_ID: u16 = 0;
#[unsafe(no_mangle)]
static SHARD_COUNT: u16 = 0;

#[map(name = "SOCKARRAY")]
static SOCKARRAY: ReusePortSockArray = ReusePortSockArray::with_max_entries(MAX_SHARDS, 0);

#[map(name = "FLOWS")]
static FLOWS: LruHashMap<FlowKey, u32> = LruHashMap::with_max_entries(MAX_FLOWS, 0);

#[map(name = "COUNTERS")]
static COUNTERS: PerCpuArray<u64> = PerCpuArray::with_max_entries(counters::COUNT, 0);

fn bump(index: u32) {
    if let Some(ptr) = COUNTERS.get_ptr_mut(index) {
        unsafe {
            *ptr = (*ptr).wrapping_add(1);
        }
    }
}

struct BpfSteerEnv;

impl SteerEnv for BpfSteerEnv {
    fn flow_lookup(&self, flow: FlowKey) -> Option<u16> {
        let shard = unsafe { FLOWS.get(flow) }?;
        u16::try_from(*shard).ok()
    }

    fn flow_insert(&mut self, flow: FlowKey, shard: u16) {
        let _ = FLOWS.insert(flow, u32::from(shard), 0);
    }

    fn bump(&mut self, counter: u32) {
        bump(counter);
    }
}

fn select_shard(ctx: &SkReuseportContext, shard: u16) -> u32 {
    if u32::from(shard) >= MAX_SHARDS {
        bump(counters::INVALID_SHARD);
        return SK_DROP;
    }
    match SOCKARRAY.select_reuseport(ctx, u32::from(shard)) {
        Ok(()) => {
            bump(counters::SELECTED);
            SK_PASS
        }
        Err(_) => {
            bump(counters::INVALID_SHARD);
            SK_DROP
        }
    }
}

/// Client bootstrap `SK_REUSEPORT`: first STUN of a transport carries the
/// ufrag and steers by `TransportRoute.shard()`. An established (non-STUN)
/// flow is steered by the `FLOWS` affinity map instead — no per-route lookup
/// either way.
#[sk_reuseport]
pub fn pulsebeam_client(ctx: SkReuseportContext) -> u32 {
    let Some(udp) = parse_udp(&ctx) else {
        bump(counters::MALFORMED_STUN);
        return SK_DROP;
    };

    let cluster_id = unsafe { core::ptr::read_volatile(&raw const CLUSTER_ID) };
    let node_id = unsafe { core::ptr::read_volatile(&raw const NODE_ID) };
    let shard_count = unsafe { core::ptr::read_volatile(&raw const SHARD_COUNT) };

    let mut env = BpfSteerEnv;
    match steer::steer_client(
        &mut env,
        udp.payload(),
        udp.flow,
        cluster_id,
        node_id,
        shard_count,
        MAX_SHARDS,
    ) {
        Verdict::Pass { shard } => select_shard(&ctx, shard),
        Verdict::Drop(_) => SK_DROP,
    }
}

/// Inter-node `SK_REUSEPORT`: reads `Envelope.route` at its fixed offset and
/// steers by `RouteId.shard()`. No per-route lookup — the packed route bits
/// are the only thing the kernel needs.
#[sk_reuseport]
pub fn pulsebeam_node(ctx: SkReuseportContext) -> u32 {
    let Some(udp) = parse_udp(&ctx) else {
        bump(counters::MALFORMED_ENVELOPE);
        return SK_DROP;
    };

    let shard_count = unsafe { core::ptr::read_volatile(&raw const SHARD_COUNT) };

    let mut env = BpfSteerEnv;
    match steer::steer_node(&mut env, udp.payload(), shard_count, MAX_SHARDS) {
        Verdict::Pass { shard } => select_shard(&ctx, shard),
        Verdict::Drop(_) => SK_DROP,
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
