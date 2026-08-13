//! Bitstream primitives for the AV1 Dependency Descriptor.
//!
//! Overflow is explicit here: `#![deny(clippy::arithmetic_side_effects)]`.
//! This parses attacker-supplied bytes, and `overflow-checks` is off in
//! release, so a wrapped offset or width would not stop — it would read the
//! wrong bits and hand up a descriptor that looks valid.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Truncated;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Overflow;

pub struct BitReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn with_offset(buf: &'a [u8], bit_offset: usize) -> Self {
        Self {
            buf,
            pos: bit_offset.min(buf.len().saturating_mul(8)),
        }
    }

    pub fn bits_read(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        (self.buf.len().saturating_mul(8)).saturating_sub(self.pos)
    }

    pub fn read_bit(&mut self) -> Result<u32, Truncated> {
        self.read_bits(1)
    }

    pub fn read_bits(&mut self, n: u32) -> Result<u32, Truncated> {
        debug_assert!(n <= 32, "read_bits({n}) exceeds u32");
        if n == 0 {
            return Ok(0);
        }
        if self.remaining() < n as usize {
            return Err(Truncated);
        }

        let mut out = 0u32;
        let mut left = n;
        let mut pos = self.pos;
        while left > 0 {
            let byte = *self.buf.get(pos >> 3).ok_or(Truncated)?;
            let bit_in_byte = (pos & 7) as u32;
            let avail = 8u32.saturating_sub(bit_in_byte);
            let take = avail.min(left);
            let shift = avail.saturating_sub(take);
            let mask = if take == 8 {
                0xff
            } else {
                (1u32 << take).saturating_sub(1)
            };
            out = (out << take) | ((byte as u32 >> shift) & mask);
            pos = pos.saturating_add(take as usize);
            left = left.saturating_sub(take);
        }

        self.pos = pos;
        Ok(out)
    }

    /// Read `n` bits as a byte. `n` above 8 is a caller bug, not bad input, so
    /// it is asserted rather than reported; the read is clamped so a release
    /// build cannot silently drop the high bits.
    pub fn read_bits_u8(&mut self, n: u32) -> Result<u8, Truncated> {
        debug_assert!(n <= 8, "read_bits_u8({n}) cannot fit in a u8");
        let v = self.read_bits(n.min(8))?;
        Ok(v as u8)
    }

    /// Read `n` bits as a `u16`, under the same contract as [`Self::read_bits_u8`].
    pub fn read_bits_u16(&mut self, n: u32) -> Result<u16, Truncated> {
        debug_assert!(n <= 16, "read_bits_u16({n}) cannot fit in a u16");
        let v = self.read_bits(n.min(16))?;
        Ok(v as u16)
    }

    /// [`Self::read_ns`] where the caller has bounded `n` to 256, so every
    /// value in `0..n` is a byte.
    pub fn read_ns_u8(&mut self, n: u32) -> Result<u8, Truncated> {
        debug_assert!(n <= 256, "read_ns_u8({n}) can decode above a u8");
        let v = self.read_ns(n.min(256))?;
        Ok(v as u8)
    }

    /// AV1 non-symmetric encoding (`ns(n)`): values in `0..n` in `floor_log2(n)`
    /// or `floor_log2(n)+1` bits.
    pub fn read_ns(&mut self, n: u32) -> Result<u32, Truncated> {
        debug_assert!(n > 0, "ns(0) has no valid encoding");
        if n <= 1 {
            return Ok(0);
        }

        let w = floor_log2(n).saturating_add(1);
        let m = (1u32 << w).saturating_sub(n);
        let v = self.read_bits(w.saturating_sub(1))?;
        let out = if v < m {
            v
        } else {
            (v << 1).saturating_sub(m).saturating_add(self.read_bit()?)
        };

        debug_assert!(out < n, "ns({n}) decoded {out} out of range");
        Ok(out)
    }

    pub fn skip(&mut self, n: usize) -> Result<(), Truncated> {
        if self.remaining() < n {
            return Err(Truncated);
        }
        self.pos = self.pos.saturating_add(n);
        Ok(())
    }
}

pub struct BitWriter<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> BitWriter<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        buf.fill(0);
        Self { buf, pos: 0 }
    }

    pub fn bits_written(&self) -> usize {
        self.pos
    }

    pub fn write_bit(&mut self, b: bool) -> Result<(), Overflow> {
        self.write_bits(u32::from(b), 1)
    }

    pub fn write_bits(&mut self, v: u32, n: u32) -> Result<(), Overflow> {
        debug_assert!(n <= 32, "write_bits({n}) exceeds u32");
        debug_assert!(
            n == 32 || v < (1u32 << n),
            "write_bits({v}, {n}) does not fit"
        );
        if n == 0 {
            return Ok(());
        }
        if (self.buf.len().saturating_mul(8)).saturating_sub(self.pos) < n as usize {
            return Err(Overflow);
        }

        let v = if n == 32 {
            v
        } else {
            v & (1u32 << n).saturating_sub(1)
        };
        let mut left = n;
        let mut pos = self.pos;
        while left > 0 {
            let byte = self.buf.get_mut(pos >> 3).ok_or(Overflow)?;
            let bit_in_byte = (pos & 7) as u32;
            let avail = 8u32.saturating_sub(bit_in_byte);
            let take = avail.min(left);
            let chunk = (v >> left.saturating_sub(take)) & (1u32 << take).saturating_sub(1);
            let chunk = chunk as u8;
            *byte |= chunk << avail.saturating_sub(take);
            pos = pos.saturating_add(take as usize);
            left = left.saturating_sub(take);
        }

        self.pos = pos;
        Ok(())
    }

    pub fn write_ns(&mut self, v: u32, n: u32) -> Result<(), Overflow> {
        debug_assert!(n > 0, "ns(0) has no valid encoding");
        debug_assert!(v < n, "ns({n}) cannot encode {v}");
        if n <= 1 {
            return Ok(());
        }

        let w = floor_log2(n).saturating_add(1);
        let m = (1u32 << w).saturating_sub(n);
        if v < m {
            self.write_bits(v, w.saturating_sub(1))
        } else {
            let x = v.saturating_add(m);
            self.write_bits(x >> 1, w.saturating_sub(1))?;
            self.write_bit(x & 1 == 1)
        }
    }

    /// Byte length written; any trailing bits in the final byte are zero, which
    /// is what the descriptor's `zero_bit` padding requires.
    pub fn finish(self) -> usize {
        self.pos.div_ceil(8)
    }
}

fn floor_log2(v: u32) -> u32 {
    debug_assert!(v > 0);
    u32::BITS
        .saturating_sub(1)
        .saturating_sub(v.leading_zeros())
}

#[cfg(test)]
mod tests {
    // Tests assert by panicking; the process ending is the mechanism.
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn reads_across_byte_boundaries() {
        let buf = [0b1010_1100, 0b0011_0101];
        let mut r = BitReader::new(&buf);
        assert_eq!(r.read_bits(3).unwrap(), 0b101);
        assert_eq!(r.read_bits(7).unwrap(), 0b0110000);
        assert_eq!(r.read_bits(6).unwrap(), 0b110101);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn bit_reader_rejects_reads_past_end() {
        let buf = [0xff, 0xff];
        let mut r = BitReader::new(&buf);
        r.read_bits(16).unwrap();
        assert_eq!(r.read_bit(), Err(Truncated));

        let mut r = BitReader::new(&buf);
        assert_eq!(r.read_bits(17), Err(Truncated));
    }

    #[test]
    fn failed_read_leaves_cursor_unchanged() {
        let buf = [0xff];
        let mut r = BitReader::new(&buf);
        r.read_bits(6).unwrap();
        assert_eq!(r.read_bits(4), Err(Truncated));
        assert_eq!(r.bits_read(), 6);
        assert_eq!(r.read_bits(2).unwrap(), 0b11);
    }

    #[test]
    fn bit_writer_reports_overflow_instead_of_panicking() {
        let mut buf = [0u8; 1];
        let mut w = BitWriter::new(&mut buf);
        w.write_bits(0xff, 8).unwrap();
        assert_eq!(w.write_bit(true), Err(Overflow));
    }

    #[test]
    fn finish_zero_pads_trailing_bits() {
        let mut buf = [0xffu8; 2];
        let mut w = BitWriter::new(&mut buf);
        w.write_bits(0b101, 3).unwrap();
        assert_eq!(w.finish(), 1);
        assert_eq!(buf[0], 0b1010_0000);
    }

    #[test]
    fn ns_roundtrips_all_values_up_to_64() {
        for n in 1..=64u32 {
            for v in 0..n {
                let mut buf = [0u8; 2];
                let mut w = BitWriter::new(&mut buf);
                w.write_ns(v, n).unwrap();
                let written = w.bits_written();

                let mut r = BitReader::new(&buf);
                assert_eq!(r.read_ns(n).unwrap(), v, "ns({n}) value {v}");
                assert_eq!(r.bits_read(), written, "ns({n}) width mismatch for {v}");
            }
        }
    }

    #[test]
    fn ns_uses_minimal_widths() {
        let mut buf = [0u8; 2];
        let mut w = BitWriter::new(&mut buf);
        w.write_ns(0, 1).unwrap();
        assert_eq!(w.bits_written(), 0);

        let mut w = BitWriter::new(&mut buf);
        w.write_ns(0, 3).unwrap();
        assert_eq!(w.bits_written(), 1);

        let mut w = BitWriter::new(&mut buf);
        w.write_ns(2, 3).unwrap();
        assert_eq!(w.bits_written(), 2);
    }

    proptest! {
        #[test]
        fn bits_roundtrip_arbitrary_widths(chunks in prop::collection::vec((0u32..=32, any::<u32>()), 1..24)) {
            let mut buf = [0u8; 128];
            let total: u32 = chunks.iter().map(|(n, _)| n).sum();
            prop_assume!(total as usize <= buf.len().saturating_mul(8));

            let masked: Vec<(u32, u32)> = chunks
                .iter()
                .map(|&(n, v)| {
                    let masked = if n == 32 {
                        v
                    } else if n == 0 {
                        0
                    } else {
                        v & (1u32 << n).saturating_sub(1)
                    };
                    (n, masked)
                })
                .collect();

            let mut w = BitWriter::new(&mut buf);
            for &(n, v) in &masked {
                w.write_bits(v, n).unwrap();
            }
            let len = w.finish();

            let mut r = BitReader::new(&buf[..len]);
            for &(n, v) in &masked {
                prop_assert_eq!(r.read_bits(n).unwrap(), v);
            }
        }

        #[test]
        fn reader_never_panics_on_arbitrary_input(
            buf in prop::collection::vec(any::<u8>(), 0..64),
            ops in prop::collection::vec((0u32..=32, any::<bool>()), 1..40),
        ) {
            let mut r = BitReader::new(&buf);
            for (n, is_ns) in ops {
                if is_ns {
                    let _ = r.read_ns(n.max(1));
                } else {
                    let _ = r.read_bits(n);
                }
            }
        }
    }
}
