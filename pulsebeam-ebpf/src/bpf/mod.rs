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
use pulsebeam_routing::{
    classify::{classify_client_for_node, classify_node, ClientVerdict, DropReason, NodeVerdict},
    envelope::peek_shard,
};

use crate::common::{counters, MAX_FLOWS, MAX_SHARDS};
use net::{parse_udp, FlowKey};

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

fn drop_reason_counter(reason: DropReason) -> u32 {
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

    match classify_client_for_node(udp.payload(), cluster_id, node_id, shard_count) {
        ClientVerdict::Bootstrap { handle, .. } => {
            let shard = handle.route.shard();
            let selected = select_shard(&ctx, shard);
            if selected == SK_PASS {
                let _ = FLOWS.insert(udp.flow, u32::from(shard), 0);
            }
            selected
        }
        ClientVerdict::Established => match unsafe { FLOWS.get(udp.flow) } {
            Some(&shard) => {
                if shard >= u32::from(shard_count) {
                    bump(counters::STALE_ROUTE);
                    SK_DROP
                } else {
                    select_shard(&ctx, shard as u16)
                }
            }
            None => {
                bump(counters::UNKNOWN_FLOW);
                SK_DROP
            }
        },
        ClientVerdict::Drop(reason) => {
            bump(drop_reason_counter(reason));
            SK_DROP
        }
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

    let Some(shard) = peek_shard(udp.payload()) else {
        bump(counters::MALFORMED_ENVELOPE);
        return SK_DROP;
    };

    match classify_node(udp.payload(), shard_count) {
        NodeVerdict::Steer { shard: verdict_shard } => {
            debug_assert_eq!(shard, verdict_shard, "peek_shard must agree with classify_node");
            select_shard(&ctx, verdict_shard)
        }
        NodeVerdict::Drop(reason) => {
            bump(drop_reason_counter(reason));
            SK_DROP
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
