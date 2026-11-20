//! Complete, windowed images over a single authenticated Link association.
//! A round's deadline is set by the reader before it sends its first pull.

use std::{num::NonZeroU64, time::Duration};

use pulsebeam_link::CONTROL_FRAME_BYTES;
use pulsebeam_runtime::time::Instant;

pub(in crate::control::controller) const MAX_IMAGE_BYTES: usize = 64 * 1024;
pub(super) const RETRY: Duration = Duration::from_millis(150);
const DEADLINE: Duration = Duration::from_secs(10);
const VERSION: u8 = 1;
const PULL: u8 = 1;
const CHUNK: u8 = 2;
const HEADER: usize = 16;
const CHUNK_BYTES: usize = CONTROL_FRAME_BYTES - HEADER;
const WINDOW: usize = 8;

#[derive(Clone, Copy)]
pub(super) struct Pull {
    round: NonZeroU64,
    window: u16,
    wanted: u8,
}

pub(super) enum Frame<'a> {
    Pull(Pull),
    Chunk(Chunk<'a>),
}

pub(super) struct Chunk<'a> {
    round: NonZeroU64,
    total: usize,
    index: usize,
    bytes: &'a [u8],
}

fn read_int<const N: usize>(input: &[u8], at: usize) -> Option<[u8; N]> {
    input.get(at..at.checked_add(N)?)?.try_into().ok()
}

impl<'a> Frame<'a> {
    pub(super) fn decode(input: &'a [u8]) -> Option<Self> {
        if input.first().copied()? != VERSION || input.len() > CONTROL_FRAME_BYTES {
            return None;
        }
        let round = NonZeroU64::new(u64::from_be_bytes(read_int(input, 2)?))?;
        match input.get(1).copied()? {
            PULL if input.len() == 13 => {
                let window = u16::from_be_bytes(read_int(input, 10)?);
                let wanted = *input.get(12)?;
                if wanted == 0
                    || usize::from(window) >= MAX_IMAGE_BYTES.div_ceil(CHUNK_BYTES * WINDOW)
                {
                    return None;
                }
                Some(Self::Pull(Pull {
                    round,
                    window,
                    wanted,
                }))
            }
            CHUNK => {
                let total = usize::try_from(u32::from_be_bytes(read_int(input, 10)?)).ok()?;
                if total == 0 || total > MAX_IMAGE_BYTES {
                    return None;
                }
                let index = usize::from(u16::from_be_bytes(read_int(input, 14)?));
                let offset = index.checked_mul(CHUNK_BYTES)?;
                let length = total.checked_sub(offset)?.min(CHUNK_BYTES);
                let bytes = input.get(HEADER..)?;
                if length == 0 || bytes.len() != length {
                    return None;
                }
                Some(Self::Chunk(Chunk {
                    round,
                    total,
                    index,
                    bytes,
                }))
            }
            _ => None,
        }
    }
}

impl Pull {
    fn encode(self) -> Box<[u8]> {
        let mut out = Vec::with_capacity(13);
        out.extend([VERSION, PULL]);
        out.extend(self.round.get().to_be_bytes());
        out.extend(self.window.to_be_bytes());
        out.push(self.wanted);
        out.into_boxed_slice()
    }
}

#[derive(Default)]
pub(super) struct Sender {
    round: Option<NonZeroU64>,
    image: Option<(Instant, Vec<u8>)>,
}

impl Sender {
    /// For any one round, read at most once, even if the read failed or expired.
    pub(super) fn needs_image(&mut self, pull: Pull) -> bool {
        if self.round.is_some_and(|round| pull.round <= round) || pull.window != 0 {
            return false;
        }
        self.round = Some(pull.round);
        self.image = None;
        true
    }

    pub(super) fn set_image(&mut self, now: Instant, bytes: Vec<u8>) {
        if !bytes.is_empty() && bytes.len() <= MAX_IMAGE_BYTES {
            self.image = now.checked_add(DEADLINE).map(|deadline| (deadline, bytes));
        }
    }

    pub(super) fn respond(&self, now: Instant, pull: Pull) -> impl Iterator<Item = Box<[u8]>> + '_ {
        let image = self
            .image
            .as_ref()
            .filter(|(deadline, _)| Some(pull.round) == self.round && now < *deadline)
            .map(|(_, image)| image.as_slice());
        (0..WINDOW).filter_map(move |slot| {
            let image = image?;
            if pull.wanted & (1 << slot) == 0 {
                return None;
            }
            let index = usize::from(pull.window)
                .checked_mul(WINDOW)?
                .checked_add(slot)?;
            let offset = index.checked_mul(CHUNK_BYTES)?;
            let bytes = image.get(offset..offset.checked_add(CHUNK_BYTES)?.min(image.len()))?;
            if bytes.is_empty() {
                return None;
            }
            let mut out = Vec::with_capacity(HEADER.checked_add(bytes.len())?);
            out.extend([VERSION, CHUNK]);
            out.extend(pull.round.get().to_be_bytes());
            out.extend(u32::try_from(image.len()).ok()?.to_be_bytes());
            out.extend(u16::try_from(index).ok()?.to_be_bytes());
            out.extend_from_slice(bytes);
            Some(out.into_boxed_slice())
        })
    }
}

#[derive(Default)]
pub(super) struct Reader {
    last_round: u64,
    active: Option<Assembly>,
}

struct Assembly {
    round: NonZeroU64,
    deadline: Instant,
    window: u16,
    received: u8,
    image: Option<Vec<u8>>,
}

impl Reader {
    pub(super) fn start(&mut self, now: Instant) -> bool {
        let Some(round) = self.last_round.checked_add(1).and_then(NonZeroU64::new) else {
            return false;
        };
        let Some(deadline) = now.checked_add(DEADLINE) else {
            return false;
        };
        self.last_round = round.get();
        self.active = Some(Assembly {
            round,
            deadline,
            window: 0,
            received: 0,
            image: None,
        });
        true
    }

    pub(super) fn pull(&self, now: Instant) -> Option<Box<[u8]>> {
        let active = self
            .active
            .as_ref()
            .filter(|active| now < active.deadline)?;
        Some(
            Pull {
                round: active.round,
                window: active.window,
                wanted: !active.received,
            }
            .encode(),
        )
    }

    pub(super) fn receive(&mut self, now: Instant, chunk: Chunk<'_>) -> Option<(Vec<u8>, Instant)> {
        let active = self.active.as_mut().filter(|active| {
            chunk.round == active.round
                && now < active.deadline
                && chunk.index / WINDOW == usize::from(active.window)
        })?;
        let image = active.image.get_or_insert_with(|| vec![0; chunk.total]);
        if image.len() != chunk.total {
            self.active = None;
            return None;
        }
        let offset = chunk.index.checked_mul(CHUNK_BYTES)?;
        let mask = 1 << (chunk.index % WINDOW);
        let dest = image.get_mut(offset..offset.checked_add(chunk.bytes.len())?)?;
        if active.received & mask != 0 && dest != chunk.bytes {
            self.active = None;
            return None;
        }
        dest.copy_from_slice(chunk.bytes);
        active.received |= mask;
        let remaining = chunk
            .total
            .div_ceil(CHUNK_BYTES)
            .checked_sub(usize::from(active.window).checked_mul(WINDOW)?)?;
        let expected = u8::MAX >> WINDOW.saturating_sub(remaining);
        if active.received != expected {
            return None;
        }
        if remaining > WINDOW {
            active.window = active.window.checked_add(1)?;
            active.received = 0;
            return None;
        }
        let result = (active.image.take()?, active.deadline);
        self.active = None;
        Some(result)
    }
}

#[cfg(test)]
mod tests;
