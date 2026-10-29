#![doc = include_str!("../README.md")]
#[cfg(all(test, feature = "sim"))]
mod simulation_tests;
#[cfg(test)]
mod tests;
use std::{io, net::SocketAddr, num::NonZeroU16, time::SystemTime};
use tokio::time::Instant;

#[cfg(all(target_os = "linux", not(feature = "sim")))]
mod linux;
#[cfg(all(not(target_os = "linux"), not(feature = "sim")))]
#[path = "portable.rs"]
mod linux;
#[cfg(feature = "sim")]
pub mod simulation;
#[cfg(not(feature = "sim"))]
use linux as backend;
#[cfg(feature = "sim")]
use simulation as backend;

pub const BATCH_SIZE: usize = 32;
pub const MAX_GSO_SEGMENTS: usize = 64;
pub const MAX_DATAGRAM_BYTES: usize = 65_507;
const MAX_AGGREGATE_BYTES: usize = u16::MAX as usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    Auto,
    Batch,
    Scalar,
}

#[derive(Clone, Debug)]
pub struct BindOptions {
    pub local_addr: SocketAddr,
    pub reuse_port: bool,
    pub mode: Mode,
    pub recv_buffer_bytes: usize,
    pub send_buffer_bytes: usize,
    pub max_datagram_bytes: usize,
}

impl BindOptions {
    pub fn new(local_addr: SocketAddr) -> Self {
        Self {
            local_addr,
            reuse_port: false,
            mode: Mode::Auto,
            recv_buffer_bytes: 4 * 1024 * 1024,
            send_buffer_bytes: 2 * 1024 * 1024,
            max_datagram_bytes: 1500,
        }
    }

    fn validate(&self) -> io::Result<()> {
        if self.max_datagram_bytes == 0
            || self.max_datagram_bytes > MAX_DATAGRAM_BYTES
            || self.recv_buffer_bytes == 0
            || self.send_buffer_bytes == 0
            || self.recv_buffer_bytes > i32::MAX as usize
            || self.send_buffer_bytes > i32::MAX as usize
        {
            return Err(invalid("invalid UDP resource bounds"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub max_batch_messages: usize,
    pub gro: bool,
    pub max_gso_segments: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiveTime {
    pub monotonic: Instant,
    pub wall: SystemTime,
}

impl ReceiveTime {
    pub fn now() -> Self {
        Self {
            monotonic: Instant::now(),
            wall: SystemTime::now(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ReceivedDatagram<'a> {
    pub source: SocketAddr,
    pub destination: SocketAddr,
    pub payload: &'a [u8],
    pub received_at: ReceiveTime,
}

#[derive(Clone, Copy, Debug)]
struct Received {
    source: SocketAddr,
    destination: SocketAddr,
    len: usize,
    stride: usize,
    received_at: ReceiveTime,
}

struct ReceiveArena {
    bytes: Box<[u8]>,
    slot_size: usize,
    entries: [Option<Received>; BATCH_SIZE],
}

impl ReceiveArena {
    fn new(max_datagram: usize, gro: bool) -> Self {
        let slot_size = if gro {
            MAX_AGGREGATE_BYTES
        } else {
            max_datagram.saturating_add(1)
        };
        Self {
            bytes: vec![0; slot_size.saturating_mul(BATCH_SIZE)].into_boxed_slice(),
            slot_size,
            entries: [None; BATCH_SIZE],
        }
    }
}

pub struct RecvBatch<'a> {
    arena: &'a ReceiveArena,
}

impl RecvBatch<'_> {
    pub fn iter(&self) -> impl Iterator<Item = ReceivedDatagram<'_>> + '_ {
        self.arena
            .entries
            .iter()
            .zip(self.arena.bytes.chunks_exact(self.arena.slot_size))
            .filter_map(|(entry, bytes)| entry.map(|entry| (entry, bytes)))
            .flat_map(|(entry, bytes)| {
                // Zero-length UDP datagrams are valid and still yield one item.
                let stride = entry.stride.max(1);
                let count = entry.len.max(1).div_ceil(stride);
                (0..count).map(move |segment| {
                    let start = segment.saturating_mul(stride);
                    let end = start.saturating_add(stride).min(entry.len);
                    ReceivedDatagram {
                        source: entry.source,
                        destination: entry.destination,
                        payload: bytes.get(start..end).unwrap_or_default(),
                        received_at: entry.received_at,
                    }
                })
            })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SendDatagram<'a> {
    pub destination: SocketAddr,
    pub payload: &'a [u8],
    pub gso_segment_size: Option<NonZeroU16>,
}

#[derive(Debug)]
pub enum SendStop {
    Blocked,
    Error(io::Error),
}

#[derive(Debug)]
pub struct SendReport {
    pub accepted: usize,
    pub stop: Option<SendStop>,
}

/// Socket-local diagnostics. These do not measure network loss or delivery.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub received_datagrams: u64,
    pub received_bytes: u64,
    pub truncated_drops: u64,
    pub invalid_metadata_drops: u64,
    pub submitted_datagrams: u64,
    pub submitted_bytes: u64,
    pub blocked_submissions: u64,
    pub io_errors: u64,
}

pub struct BoundUdpSocket {
    inner: backend::Bound,
    options: BindOptions,
}

pub async fn bind(options: BindOptions) -> io::Result<BoundUdpSocket> {
    options.validate()?;
    let inner = backend::bind(&options).await?;
    Ok(BoundUdpSocket { inner, options })
}

impl BoundUdpSocket {
    pub fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }

    pub fn activate(self) -> io::Result<UdpSocket> {
        let inner = self.inner.activate(&self.options)?;
        let capabilities = inner.capabilities();
        Ok(UdpSocket {
            arena: ReceiveArena::new(self.options.max_datagram_bytes, capabilities.gro),
            max_datagram_bytes: self.options.max_datagram_bytes,
            inner,
            counters: Counters::default(),
        })
    }
}

#[cfg(all(target_os = "linux", not(feature = "sim")))]
impl std::os::fd::AsFd for BoundUdpSocket {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(&self.inner)
    }
}

pub struct UdpSocket {
    inner: backend::Socket,
    arena: ReceiveArena,
    max_datagram_bytes: usize,
    counters: Counters,
}

impl UdpSocket {
    pub fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }
    pub fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    pub fn counters(&self) -> Counters {
        self.counters
    }
    pub async fn readable(&self) -> io::Result<()> {
        self.inner.readable().await
    }
    pub async fn writable(&self) -> io::Result<()> {
        self.inner.writable().await
    }

    pub fn try_recv_batch(&mut self) -> io::Result<RecvBatch<'_>> {
        self.arena.entries.fill(None);
        if let Err(error) =
            self.inner
                .recv(&mut self.arena, self.max_datagram_bytes, &mut self.counters)
        {
            if error.kind() != io::ErrorKind::WouldBlock {
                self.counters.io_errors = self.counters.io_errors.saturating_add(1);
            }
            return Err(error);
        }
        for entry in self.arena.entries.iter().flatten() {
            self.counters.received_datagrams = self
                .counters
                .received_datagrams
                .saturating_add(entry.len.max(1).div_ceil(entry.stride.max(1)) as u64);
            self.counters.received_bytes = self
                .counters
                .received_bytes
                .saturating_add(entry.len as u64);
        }
        Ok(RecvBatch { arena: &self.arena })
    }

    /// Validate the entire input first, then submit a bounded contiguous prefix.
    pub fn try_send_batch(&mut self, datagrams: &[SendDatagram<'_>]) -> SendReport {
        let report = match validate_send(datagrams, self.capabilities()) {
            Ok(()) => self.inner.send(datagrams),
            Err(error) => SendReport {
                accepted: 0,
                stop: Some(SendStop::Error(error)),
            },
        };
        for entry in datagrams.iter().take(report.accepted) {
            let segments = entry.gso_segment_size.map_or(1, |size| {
                entry.payload.len().div_ceil(usize::from(size.get()))
            });
            self.counters.submitted_datagrams = self
                .counters
                .submitted_datagrams
                .saturating_add(segments as u64);
            self.counters.submitted_bytes = self
                .counters
                .submitted_bytes
                .saturating_add(entry.payload.len() as u64);
        }
        match report.stop {
            Some(SendStop::Blocked) => {
                self.counters.blocked_submissions =
                    self.counters.blocked_submissions.saturating_add(1);
            }
            Some(SendStop::Error(_)) => {
                self.counters.io_errors = self.counters.io_errors.saturating_add(1);
            }
            None => {}
        }
        report
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn validate_send(datagrams: &[SendDatagram<'_>], caps: Capabilities) -> io::Result<()> {
    if datagrams.len() > BATCH_SIZE {
        return Err(invalid("UDP send batch exceeds the entry budget"));
    }
    for entry in datagrams {
        if let Some(size) = entry.gso_segment_size {
            let size = usize::from(size.get());
            if caps.max_gso_segments == 1 {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "UDP GSO unavailable",
                ));
            }
            if entry.payload.len() <= size
                || entry.payload.len() > MAX_AGGREGATE_BYTES
                || entry.payload.len().div_ceil(size) > caps.max_gso_segments
                || size > MAX_DATAGRAM_BYTES
            {
                return Err(invalid("invalid UDP GSO aggregate"));
            }
        } else if entry.payload.len() > MAX_DATAGRAM_BYTES {
            return Err(invalid("UDP datagram exceeds legal payload size"));
        }
    }
    Ok(())
}

/// Shared progress algorithm: short success and EAGAIN stop immediately; a
/// later fatal error retains all earlier accepted entries.
fn submit_prefix(
    entries: &[SendDatagram<'_>],
    max_group: usize,
    mut submit: impl FnMut(&[SendDatagram<'_>]) -> io::Result<usize>,
) -> SendReport {
    let mut accepted = 0usize;
    // A single call never submits more than BATCH_SIZE kernel messages.
    let limit = entries.len().min(BATCH_SIZE);
    while accepted < limit {
        let Some(first) = entries.get(accepted) else {
            break;
        };
        let run = entries.get(accepted..limit).unwrap_or_default();
        let len = run
            .iter()
            .take(max_group)
            .take_while(|entry| entry.gso_segment_size == first.gso_segment_size)
            .count();
        let group = run.get(..len).unwrap_or_default();
        match submit(group) {
            Ok(count) if count <= len => {
                accepted = accepted.saturating_add(count);
                if count < len {
                    return SendReport {
                        accepted,
                        stop: Some(SendStop::Blocked),
                    };
                }
            }
            Ok(_) => {
                return SendReport {
                    accepted,
                    stop: Some(SendStop::Error(io::Error::other(
                        "invalid backend send progress",
                    ))),
                };
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return SendReport {
                    accepted,
                    stop: Some(SendStop::Blocked),
                };
            }
            Err(error) => {
                return SendReport {
                    accepted,
                    stop: Some(SendStop::Error(error)),
                };
            }
        }
    }
    SendReport {
        accepted,
        stop: (accepted < entries.len()).then_some(SendStop::Blocked),
    }
}
