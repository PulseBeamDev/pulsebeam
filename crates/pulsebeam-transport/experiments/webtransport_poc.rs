//! Frame-delivery experiment, not decoded-quality or browser compatibility proof.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_methods,
    reason = "bounded deterministic experimental fixture; failed invariants abort the measurement"
)]

use pulsebeam_transport::{
    Error, NetworkClass, PeerRole, QuicCongestion, QuicCredential, QuicPeer, QuicTuning,
    ReceiveTime, SendOutcome, Time, Transport, experiment::QuicOnly,
};
use pulsebeam_webrtc_sys::*;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::Write as _,
    net::{Ipv4Addr, SocketAddr},
    time::{Duration, Instant},
};

mod comparison;
mod decoder;
mod encoder;
mod media_queue;
mod native_profile;
mod path;
mod receiver;
#[cfg(test)]
mod video;
#[cfg(test)]
use native_profile::NativeOptions;
use native_profile::{ControllerLog, NativeController, NativeTrials};

mod corpus;
mod measurement;
mod uplink;

#[cfg(test)]
#[path = "controls.rs"]
mod controls;

const CHUNK: usize = 1000;
#[cfg(test)]
include!("legacy.rs");

#[derive(Clone, Copy, Debug)]
struct Scenario {
    name: &'static str,
    delay_ms: u64,
    loss: u64,
    turn_ms: u64,
}
const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "clean",
        delay_ms: 15,
        loss: 0,
        turn_ms: 1,
    },
    Scenario {
        name: "constrained",
        delay_ms: 15,
        loss: 0,
        turn_ms: 1,
    },
    Scenario {
        name: "capacity_step",
        delay_ms: 15,
        loss: 0,
        turn_ms: 1,
    },
    Scenario {
        name: "loss_1pct",
        delay_ms: 15,
        loss: 100,
        turn_ms: 1,
    },
    Scenario {
        name: "rtt_160ms",
        delay_ms: 80,
        loss: 0,
        turn_ms: 1,
    },
    Scenario {
        name: "host_turn_4ms",
        delay_ms: 15,
        loss: 0,
        turn_ms: 4,
    },
    Scenario {
        name: "host_stall_20ms",
        delay_ms: 15,
        loss: 0,
        turn_ms: 1,
    },
];
impl Scenario {
    fn rate(self, at: u64, reverse: bool, duration: u64) -> u64 {
        if reverse {
            return 256_000;
        }
        match self.name {
            "constrained" => 900_000,
            "capacity_step" if at >= duration / 3 && at < duration * 2 / 3 => 600_000,
            _ => 2_500_000,
        }
    }
    fn turn(self, at: u64) -> bool {
        at.is_multiple_of(self.turn_ms) && !(self.name == "host_stall_20ms" && at % 250 < 20)
    }
}

struct Link<T> {
    scenario: Scenario,
    duration: u64,
    rng: u64,
    finish: [u64; 2],
    pending: VecDeque<(u64, usize, T)>,
    bytes: [u64; 2],
    accepted_bytes: [u64; 2],
    dropped_bytes: [u64; 2],
    dropped: u64,
    peak_queue_ms: u64,
}
impl<T> Link<T> {
    fn new(scenario: Scenario, seed: u64, duration: u64) -> Self {
        Self {
            scenario,
            duration,
            rng: seed + 1,
            finish: [0; 2],
            pending: VecDeque::new(),
            bytes: [0; 2],
            accepted_bytes: [0; 2],
            dropped_bytes: [0; 2],
            dropped: 0,
            peak_queue_ms: 0,
        }
    }
    fn begin_measurement(&mut self, epoch: u64, duration: u64, seed: u64) {
        for finish in &mut self.finish {
            *finish = finish.saturating_sub(epoch * 1_000_000);
        }
        for (due, _, _) in &mut self.pending {
            *due = due.saturating_sub(epoch);
        }
        self.duration = duration;
        self.rng = seed + 1;
        self.bytes = [0; 2];
        self.accepted_bytes = [0; 2];
        self.dropped_bytes = [0; 2];
        self.dropped = 0;
        self.peak_queue_ms = 0;
    }
    fn push(&mut self, now: u64, from: usize, bytes: usize, item: T) -> Result<(), T> {
        self.bytes[from] += bytes as u64;
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        let now_ns = now * 1_000_000;
        let start = self.finish[from].max(now_ns);
        let queue_ms = (start - now_ns) / 1_000_000;
        if queue_ms > 200 {
            self.dropped += 1;
            self.dropped_bytes[from] += bytes as u64;
            return Err(item);
        }
        self.peak_queue_ms = self.peak_queue_ms.max(queue_ms);
        self.finish[from] = self.serialization_finish(start, bytes, from == 1);
        if self.rng % 10_000 < self.scenario.loss {
            self.dropped += 1;
            self.dropped_bytes[from] += bytes as u64;
            return Err(item);
        }
        self.accepted_bytes[from] += bytes as u64;
        let due = self.finish[from].div_ceil(1_000_000) + self.scenario.delay_ms;
        // The two independently serialized directions may interleave.
        let at = self.pending.partition_point(|(ready, _, _)| *ready <= due);
        self.pending.insert(at, (due, 1 - from, item));
        Ok(())
    }
    fn serialization_finish(&self, start: u64, bytes: usize, reverse: bool) -> u64 {
        let mut at = start;
        let mut work = bytes as u128 * 8 * 1_000_000_000;
        loop {
            let rate = u128::from(self.scenario.rate(at / 1_000_000, reverse, self.duration));
            let boundary = if self.scenario.name == "capacity_step" && !reverse {
                [self.duration / 3, self.duration.saturating_mul(2) / 3]
                    .into_iter()
                    .map(|ms| ms.saturating_mul(1_000_000))
                    .find(|boundary| *boundary > at)
                    .unwrap_or(u64::MAX)
            } else {
                u64::MAX
            };
            let available = u128::from(boundary - at) * rate;
            if work <= available {
                return at + u64::try_from(work.div_ceil(rate)).unwrap();
            }
            work -= available;
            at = boundary;
        }
    }

    fn pop(&mut self, now: u64) -> Option<(usize, T)> {
        if self.pending.front().is_some_and(|(due, _, _)| *due <= now) {
            let (_, to, item) = self.pending.pop_front().unwrap();
            Some((to, item))
        } else {
            None
        }
    }
}

struct Packet {
    bytes: Vec<u8>,
    expires: Time,
}
enum Session {
    Bare(Box<QuicOnly>),
    Overlay(Box<Transport>),
}
impl Session {
    fn dtls(peer: pulsebeam_transport::Peer, origin: Instant) -> Self {
        let mut transport = Transport::new(peer).unwrap();
        transport.start(Time(0), origin).unwrap();
        assert!(transport.health().quic.is_none());
        Self::Overlay(Box::new(transport))
    }
    fn new(peer: QuicPeer, overlay: bool, origin: Instant) -> Self {
        if overlay {
            let mut transport = Transport::new(peer).unwrap();
            transport.start(Time(0), origin).unwrap();
            Self::Overlay(Box::new(transport))
        } else {
            Self::Bare(Box::new(QuicOnly::new(peer, origin).unwrap()))
        }
    }
    fn ready(&self) -> bool {
        match self {
            Self::Bare(port) => port.ready(),
            Self::Overlay(port) => port.health().ready,
        }
    }
    fn input_ready(&self) -> bool {
        match self {
            Self::Bare(port) => port.input_ready(),
            Self::Overlay(port) => port.input_ready(),
        }
    }
    fn needs_progress(&self) -> bool {
        match self {
            Self::Bare(port) => !port.input_ready(),
            Self::Overlay(port) => port.needs_progress(),
        }
    }
    fn receive(&mut self, source: SocketAddr, bytes: &[u8], at: u64) {
        match self {
            Self::Bare(port) => port.receive(bytes).unwrap(),
            Self::Overlay(port) => port
                .receive(
                    source,
                    bytes,
                    ReceiveTime {
                        monotonic: Time(at * 1_000_000),
                        wall: None,
                    },
                )
                .unwrap(),
        }
    }
    fn submit(&mut self, packet: &Packet, retain_protected: bool) -> Result<(), Error> {
        let expires = if retain_protected {
            Time(u64::MAX)
        } else {
            packet.expires
        };
        match self {
            Self::Bare(port) => port.submit(&packet.bytes, expires),
            Self::Overlay(port) => port.try_submit([0; 4], &packet.bytes, expires),
        }
    }
    fn encoder_target_bps(&self) -> Option<u64> {
        match self {
            Self::Overlay(port) => Some(port.health().encoder_target_bps),
            Self::Bare(_) => None,
        }
    }
    fn demand(&mut self, value: bool) {
        if let Self::Overlay(port) = self {
            port.set_demand(value);
        }
    }
    fn poll(
        &mut self,
        at: u64,
        output: &mut Vec<Vec<u8>>,
        received: &mut Vec<Vec<u8>>,
    ) -> Result<(), Error> {
        self.poll_budget(at, 256, output, received)
    }
    fn poll_budget(
        &mut self,
        at: u64,
        budget: usize,
        output: &mut Vec<Vec<u8>>,
        received: &mut Vec<Vec<u8>>,
    ) -> Result<(), Error> {
        match self {
            Self::Bare(port) => port.poll(
                Time(at * 1_000_000),
                budget,
                &mut |bytes| output.push(bytes.to_vec()),
                &mut |bytes| received.push(bytes.to_vec()),
            ),
            Self::Overlay(port) => port
                .poll(
                    Time(at * 1_000_000),
                    budget,
                    |datagram| {
                        output.push(datagram.payload.to_vec());
                        SendOutcome::Accepted {
                            bytes: datagram.payload.len(),
                        }
                    },
                    |application| received.push(application.payload.to_vec()),
                )
                .map(|_| ()),
        }
    }
    #[cfg(test)]
    fn losses(&self) -> u64 {
        match self {
            Self::Bare(port) => port.stats().lost_packets,
            Self::Overlay(port) => port.health().quic.unwrap().lost_packets,
        }
    }
    #[cfg(test)]
    fn stalls(&self) -> u64 {
        match self {
            Self::Bare(port) => port.stats().inner_stall_polls,
            Self::Overlay(port) => port.health().quic.unwrap().inner_stall_polls,
        }
    }
}

fn main() {
    comparison::run();
}
