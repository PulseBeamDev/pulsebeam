use std::collections::VecDeque;

use super::Reservation;
use super::SelectedPath;

type Packet = (SelectedPath, Vec<u8>);

pub(super) struct PendingDtls {
    packets: VecDeque<Packet>,
    packet_bytes: usize,
    max_bytes: usize,
}

impl PendingDtls {
    pub(super) fn new(max_bytes: usize) -> Self {
        Self {
            packets: VecDeque::new(),
            packet_bytes: 0,
            max_bytes,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.packets.len()
    }

    pub(super) fn try_push(&mut self, path: SelectedPath, bytes: &[u8]) -> bool {
        let Some(packet_bytes) = self.packet_bytes.checked_add(bytes.len()) else {
            return false;
        };
        let Some(available) = self.max_bytes.checked_sub(packet_bytes) else {
            return false;
        };
        let Some(required) = self.packets.len().checked_add(1) else {
            return false;
        };
        let Some(backing) = Reservation::prepare(&self.packets, required, available) else {
            return false;
        };
        let mut packet = Vec::new();
        if packet.try_reserve_exact(bytes.len()).is_err() {
            return false;
        }
        let Some(packet_bytes) = self.packet_bytes.checked_add(packet.capacity()) else {
            return false;
        };
        if packet_bytes
            .checked_add(backing.bytes())
            .is_none_or(|total| total > self.max_bytes)
        {
            return false;
        }
        packet.extend_from_slice(bytes);
        backing.commit(&mut self.packets);
        self.packets.push_back((path, packet));
        self.packet_bytes = packet_bytes;
        true
    }

    pub(super) fn take(&mut self) -> VecDeque<Packet> {
        self.packet_bytes = 0;
        std::mem::take(&mut self.packets)
    }

    pub(super) fn clear(&mut self) {
        self.packets = VecDeque::new();
        self.packet_bytes = 0;
    }

    #[cfg(test)]
    pub(super) fn retained_bytes(&self) -> usize {
        self.packets
            .capacity()
            .checked_mul(size_of::<Packet>())
            .and_then(|backing| self.packet_bytes.checked_add(backing))
            .expect("admission guarantees representable retained bytes")
    }
}
