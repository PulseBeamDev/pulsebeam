use crate::{MediaPacket, TimePoint};

/// Canonical SFU input for server-component traces, not a client or ingress oracle.
pub(crate) struct ComponentSource {
    ssrc: u32,
    sequence: u16,
    clock_rate: u32,
}

impl ComponentSource {
    pub(crate) fn audio(ssrc: u32) -> Self {
        Self {
            ssrc,
            sequence: 0,
            clock_rate: 48_000,
        }
    }

    pub(crate) fn video(ssrc: u32) -> Self {
        Self {
            ssrc,
            sequence: 0,
            clock_rate: 90_000,
        }
    }

    #[allow(
        clippy::arithmetic_side_effects,
        clippy::cast_possible_truncation,
        clippy::disallowed_types,
        reason = "bounded u64 time scales in u128 and wraps on the RTP wire; this single-threaded fixture uses the v3 immutable MediaPacket payload contract"
    )]
    pub(crate) fn sample(&mut self, at: TimePoint, payload: &[u8]) -> MediaPacket {
        use bytes::Bytes;

        self.sequence = self.sequence.wrapping_add(1);
        let timestamp =
            (u128::from(at.global.as_micros()) * u128::from(self.clock_rate) / 1_000_000) as u32;
        let mut bytes = Vec::with_capacity(12 + payload.len());
        bytes.extend_from_slice(&[
            0x80,
            if self.clock_rate == 90_000 {
                0x80 | 96
            } else {
                111
            },
        ]);
        bytes.extend_from_slice(&self.sequence.to_be_bytes());
        bytes.extend_from_slice(&timestamp.to_be_bytes());
        bytes.extend_from_slice(&self.ssrc.to_be_bytes());
        bytes.extend_from_slice(payload);
        MediaPacket::new(Bytes::from(bytes), at.global, Default::default())
    }
}
