//! Opaque packet framing for an installed native binding.

use std::{
    num::{NonZeroU16, NonZeroU32},
    time::{Duration, Instant},
};

use noq_proto::Connection;
use pulsebeam_core::fabric::packet::MAX_PACKET_BYTES;

use crate::records::{PacketFrame, PacketFraming};

use super::{APPLICATION_DATAGRAM_BYTES, DatagramAdmission, PacketScheduler};

const MAX_ACTIVE: usize = 4;
const MAX_SLICES: usize = 16;
const WHOLE_CAPACITY: usize = APPLICATION_DATAGRAM_BYTES - PacketFrame::WHOLE_HEADER_BYTES;
const SLICE_CAPACITY: usize = APPLICATION_DATAGRAM_BYTES - PacketFrame::SLICE_HEADER_BYTES;
const _: () = assert!(MAX_PACKET_BYTES.div_ceil(SLICE_CAPACITY) <= MAX_SLICES);
const LIFETIME: Duration = Duration::from_millis(250);

#[derive(Debug, PartialEq, Eq)]
pub enum SendError {
    Busy,
    Size,
    Unsupported,
    Expired,
    IdExhausted,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SendProgress {
    Idle,
    Deferred,
    Partial,
    /// Every frame has entered the Noq queue. This is not a delivery receipt.
    Admitted,
}

/// One instance per association, installed send binding and generation. Keep
/// it through queue backpressure, and drop it on retirement. Binding IDs are
/// not reused within an association direction.
pub struct Sender {
    framing: PacketFraming,
    binding: NonZeroU16,
    last_transfer: u32,
    pending: Option<Pending>,
}

struct Pending {
    bytes: Box<[u8]>,
    started: Instant,
    sliced: Option<Sliced>,
}

#[derive(Clone, Copy)]
struct Sliced {
    id: NonZeroU32,
    offset: usize,
}

impl Sender {
    pub fn new(framing: PacketFraming, binding: NonZeroU16) -> Self {
        Self {
            framing,
            binding,
            last_transfer: 0,
            pending: None,
        }
    }

    pub fn start(&mut self, now: Instant, bytes: Box<[u8]>) -> Result<(), SendError> {
        if self.pending.is_some() {
            return Err(SendError::Busy);
        }
        if bytes.is_empty() || bytes.len() > MAX_PACKET_BYTES {
            return Err(SendError::Size);
        }
        self.pending = Some(Pending {
            bytes,
            started: now,
            sliced: None,
        });
        Ok(())
    }

    /// Offer at most one DATAGRAM. The scheduler checks Noq admission but
    /// does not influence this binding's fixed Whole/Slice boundaries.
    pub fn offer(
        &mut self,
        scheduler: &mut PacketScheduler,
        connection: &mut Connection,
        now: Instant,
    ) -> Result<SendProgress, SendError> {
        self.offer_with(now, |bytes| scheduler.offer(connection, bytes))
    }

    fn offer_with(
        &mut self,
        now: Instant,
        offer: impl FnOnce(&[u8]) -> Result<(), DatagramAdmission>,
    ) -> Result<SendProgress, SendError> {
        let Some(mut pending) = self.pending.take() else {
            return Ok(SendProgress::Idle);
        };
        if now.saturating_duration_since(pending.started) >= LIFETIME {
            return Err(SendError::Expired);
        }
        let bytes = &pending.bytes;
        let (frame, next) = if bytes.len() <= WHOLE_CAPACITY {
            (PacketFrame::Whole(bytes), None)
        } else {
            let sliced = match pending.sliced {
                Some(sliced) => sliced,
                None => Sliced {
                    id: self
                        .last_transfer
                        .checked_add(1)
                        .and_then(NonZeroU32::new)
                        .ok_or(SendError::IdExhausted)?,
                    offset: 0,
                },
            };
            let end = sliced
                .offset
                .saturating_add(SLICE_CAPACITY)
                .min(bytes.len());
            let range = bytes.get(sliced.offset..end).ok_or(SendError::Size)?;
            let frame = PacketFrame::Slice {
                transfer: sliced.id,
                total: u16::try_from(bytes.len()).map_err(|_| SendError::Size)?,
                offset: u16::try_from(sliced.offset).map_err(|_| SendError::Size)?,
                bytes: range,
            };
            (
                frame,
                (end < bytes.len()).then_some(Sliced {
                    offset: end,
                    ..sliced
                }),
            )
        };
        let mut wire = [0; APPLICATION_DATAGRAM_BYTES];
        let len = frame
            .write(self.framing, self.binding, &mut wire)
            .map_err(|_| SendError::Size)?;
        match offer(wire.get(..len).ok_or(SendError::Size)?) {
            Ok(()) => {
                if let PacketFrame::Slice { transfer, .. } = frame {
                    self.last_transfer = transfer.get();
                }
                if let Some(next) = next {
                    pending.sliced = Some(next);
                    self.pending = Some(pending);
                    Ok(SendProgress::Partial)
                } else {
                    Ok(SendProgress::Admitted)
                }
            }
            Err(DatagramAdmission::Size) => Err(SendError::Size),
            Err(DatagramAdmission::Full | DatagramAdmission::ControlTurn) => {
                self.pending = Some(pending);
                Ok(SendProgress::Deferred)
            }
            Err(DatagramAdmission::Unsupported) => Err(SendError::Unsupported),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Received<'a> {
    Whole(&'a [u8]),
    Reassembled(Box<[u8]>),
}

impl AsRef<[u8]> for Received<'_> {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Whole(bytes) => bytes,
            Self::Reassembled(bytes) => bytes,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReceiveError {
    Binding,
    Malformed,
    Conflict,
    Capacity,
}

/// Allocate only for slices of a live, authenticated binding. The association
/// owner must resolve the frame prefix to the installed receive binding and
/// generation before calling `receive`; there is no packet-driven install.
pub struct Receiver {
    framing: PacketFraming,
    binding: NonZeroU16,
    high: u32,
    terminal: u64,
    active: Vec<Assembly>,
}

struct Assembly {
    id: u32,
    deadline: Instant,
    bytes: Box<[u8]>,
    ranges: [Option<(usize, usize)>; MAX_SLICES],
    remaining: usize,
}

impl Receiver {
    pub fn new(framing: PacketFraming, binding: NonZeroU16) -> Self {
        Self {
            framing,
            binding,
            high: 0,
            terminal: 0,
            active: Vec::new(),
        }
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.active.iter().map(|a| a.deadline).min()
    }

    pub fn expire(&mut self, now: Instant) {
        let high = self.high;
        let terminal = &mut self.terminal;
        self.active.retain(|a| {
            if a.deadline > now {
                true
            } else {
                *terminal |= terminal_bit(high, a.id);
                false
            }
        });
    }

    pub fn receive<'a>(
        &mut self,
        now: Instant,
        wire: &'a [u8],
    ) -> Result<Option<Received<'a>>, ReceiveError> {
        if PacketFrame::binding(wire) != Some(self.binding) {
            return Err(ReceiveError::Binding);
        }
        if wire.len() > APPLICATION_DATAGRAM_BYTES {
            return Err(ReceiveError::Malformed);
        }
        self.expire(now);
        let (transfer, total, offset, bytes) =
            match PacketFrame::decode(self.framing, wire).map_err(|_| ReceiveError::Malformed)? {
                PacketFrame::Whole(bytes) => return Ok(Some(Received::Whole(bytes))),
                PacketFrame::Slice {
                    transfer,
                    total,
                    offset,
                    bytes,
                } => (transfer, total, offset, bytes),
            };
        let id = transfer.get();
        if id > self.high {
            self.terminal = self
                .terminal
                .checked_shl(id.saturating_sub(self.high))
                .unwrap_or(0);
            self.high = id;
            self.active.retain(|a| terminal_bit(id, a.id) != 0);
        }
        let bit = terminal_bit(self.high, id);
        if bit == 0 || self.terminal & bit != 0 {
            return Ok(None);
        }
        let index = match self.active.iter().position(|a| a.id == id) {
            Some(index) => index,
            None => {
                if self.active.len() >= MAX_ACTIVE {
                    self.terminal |= bit;
                    return Err(ReceiveError::Capacity);
                }
                let deadline = now.checked_add(LIFETIME).ok_or(ReceiveError::Capacity)?;
                let index = self.active.len();
                self.active.push(Assembly {
                    id,
                    deadline,
                    bytes: vec![0; usize::from(total)].into_boxed_slice(),
                    ranges: [None; MAX_SLICES],
                    remaining: usize::from(total),
                });
                index
            }
        };
        let assembly = self.active.get_mut(index).ok_or(ReceiveError::Capacity)?;
        match assembly.insert(usize::from(total), usize::from(offset), bytes) {
            Ok(false) => Ok(None),
            result => {
                self.terminal |= bit;
                let complete = self.active.swap_remove(index);
                result.map(|_| Some(Received::Reassembled(complete.bytes)))
            }
        }
    }
}

fn terminal_bit(high: u32, id: u32) -> u64 {
    1_u64.checked_shl(high.saturating_sub(id)).unwrap_or(0)
}

impl Assembly {
    fn insert(&mut self, total: usize, offset: usize, data: &[u8]) -> Result<bool, ReceiveError> {
        if self.bytes.len() != total {
            return Err(ReceiveError::Conflict);
        }
        let end = offset.saturating_add(data.len());
        let dest = self
            .bytes
            .get_mut(offset..end)
            .ok_or(ReceiveError::Conflict)?;
        for (lo, hi) in self.ranges.iter().flatten().copied() {
            if (offset, end) == (lo, hi) && dest == data {
                return Ok(false);
            }
            if offset < hi && lo < end {
                return Err(ReceiveError::Conflict);
            }
        }
        let range = self
            .ranges
            .iter_mut()
            .find(|r| r.is_none())
            .ok_or(ReceiveError::Capacity)?;
        *range = Some((offset, end));
        dest.copy_from_slice(data);
        self.remaining = self.remaining.saturating_sub(data.len());
        Ok(self.remaining == 0)
    }
}

/// Pass the prefix and borrowed bytes to the active binding owner. Malformed
/// or unbound input never creates reassembly state.
pub fn read_frame<T>(
    connection: &mut Connection,
    receive: impl FnOnce(NonZeroU16, &[u8]) -> T,
) -> Option<Result<T, ReceiveError>> {
    let wire = connection.datagrams().recv()?;
    Some(
        PacketFrame::binding(&wire)
            .map(|binding| receive(binding, &wire))
            .ok_or(ReceiveError::Malformed),
    )
}

#[cfg(test)]
mod tests;
