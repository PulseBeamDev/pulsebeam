#![allow(
    clippy::arithmetic_side_effects,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::string_slice,
    clippy::indexing_slicing
)] // test / simulation support
//! Per-packet cost on the shard-local forwarding path, measured on real types.
//!
//! Not a criterion suite — the point is a repeatable number for the work one
//! forwarded packet does, so claims about it can be checked rather than argued.

//! Exceptions: this measures the cost of the very primitives the crate
//! restricts, and printing the numbers is the entire output.
#![allow(clippy::disallowed_types, clippy::print_stdout)]

use std::hint::black_box;
use std::time::Instant;

use pulsebeam::entity::{ParticipantId, TrackKind};
use pulsebeam::rtp::RtpPacket;

fn time<F: FnMut()>(label: &str, iters: u32, mut f: F) {
    // Warm up so the first allocation batch is not in the sample.
    for _ in 0..(iters / 10).max(1000) {
        f();
    }
    let start = Instant::now();
    for _ in 0..iters {
        f();
    }
    let ns = start.elapsed().as_nanos() as f64 / iters as f64;
    println!("{label:<46} {ns:>7.2} ns/op");
}

fn main() {
    let iters: u32 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(1_000_000);

    route_lookup(iters);
    packet_clone(iters);
}

/// What the local fanout does per packet to find a route the envelope had
/// already identified by index.
fn route_lookup(iters: u32) {
    use ahash::{HashMap, HashMapExt};

    let mut rng = pulsebeam_runtime::rand::seeded_rng(1);
    let mut map: HashMap<pulsebeam::entity::TrackId, u64> = HashMap::new();
    let mut ids = Vec::new();
    for _ in 0..64 {
        let id = ParticipantId::new(&mut rng).derive_track_id(TrackKind::Video, "v");
        map.insert(id, 0);
        ids.push(id);
    }
    let dense: Vec<u64> = vec![0; 64];
    let mut i = 0usize;

    time("route by TrackId hash", iters, || {
        i = i.wrapping_add(1);
        black_box(map.get(&ids[i % ids.len()]));
    });
    time("route by dense index", iters, || {
        i = i.wrapping_add(1);
        black_box(dense.get(i % dense.len()));
    });
}

/// `cache.push` clones the packet for every video packet on every track.
fn packet_clone(iters: u32) {
    let bare = RtpPacket::default();

    let mut with_dd = RtpPacket::default();
    with_dd.ext_vals.user_values.set_arc(std::sync::Arc::new(
        pulsebeam_core::dd::DependencyDescriptor::default(),
    ));

    time("RtpPacket::clone, no header extensions", iters, || {
        black_box(bare.clone());
    });
    time("RtpPacket::clone, one user extension", iters, || {
        black_box(with_dd.clone());
    });
    time("RtpPacket::to_transit, one user extension", iters, || {
        black_box(with_dd.to_transit());
    });

    // What the same packet costs if the header extensions are a plain struct
    // instead of a type-keyed map: the five fields the SFU actually reads, plus
    // the parsed descriptor behind one `Arc`.
    #[derive(Clone)]
    #[allow(dead_code)]
    struct CompactExts {
        audio_level: Option<i8>,
        voice_activity: Option<bool>,
        rid: Option<str0m::media::Rid>,
        abs_capture_time: Option<u64>,
        dd: Option<std::sync::Arc<pulsebeam_core::dd::DependencyDescriptor>>,
    }
    #[derive(Clone)]
    #[allow(dead_code)]
    struct CompactPacket {
        ssrc: u32,
        marker: bool,
        header_len: usize,
        seq_no: u64,
        rtp_ts: u64,
        arrival_ts: tokio::time::Instant,
        playout_time: tokio::time::Instant,
        is_keyframe: bool,
        exts: CompactExts,
        payload: std::sync::Arc<[u8]>,
    }
    let compact = CompactPacket {
        ssrc: 1,
        marker: false,
        header_len: 12,
        seq_no: 1,
        rtp_ts: 1,
        arrival_ts: tokio::time::Instant::now(),
        playout_time: tokio::time::Instant::now(),
        is_keyframe: false,
        exts: CompactExts {
            audio_level: None,
            voice_activity: None,
            rid: None,
            abs_capture_time: None,
            dd: Some(std::sync::Arc::new(
                pulsebeam_core::dd::DependencyDescriptor::default(),
            )),
        },
        payload: std::sync::Arc::from(&[0u8; 1100][..]),
    };
    time("compact packet clone, one extension", iters, || {
        black_box(compact.clone());
    });

    println!();
    println!(
        "size_of DependencyDescriptor = {}",
        std::mem::size_of::<pulsebeam_core::dd::DependencyDescriptor>()
    );
    println!("size_of StreamStateInner     = (private)");
    println!(
        "size_of RtpPacket           = {}",
        std::mem::size_of::<RtpPacket>()
    );
    println!(
        "size_of ExtensionValues     = {}",
        std::mem::size_of::<str0m::rtp::ExtensionValues>()
    );
    println!(
        "size_of CompactPacket       = {}",
        std::mem::size_of::<CompactPacket>()
    );
}
