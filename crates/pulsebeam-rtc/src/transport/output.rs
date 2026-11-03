use std::collections::VecDeque;

use super::{PreparedTransmit, TransportError, secure_output_buffer, srtp::ProtectionLayout};

pub(super) struct SlotReservation {
    expanded: Option<VecDeque<PreparedTransmit>>,
    packet_bytes: usize,
}

pub(super) struct OutputQueue {
    packets: VecDeque<PreparedTransmit>,
    packet_bytes: usize,
    max_bytes: usize,
}

impl OutputQueue {
    pub(super) fn new(max_bytes: usize) -> Self {
        Self {
            packets: VecDeque::new(),
            packet_bytes: 0,
            max_bytes,
        }
    }

    fn fits(&self, packet_bytes: usize, slots: usize) -> bool {
        slots
            .checked_mul(size_of::<PreparedTransmit>())
            .and_then(|backing| backing.checked_add(packet_bytes))
            .is_some_and(|total| total <= self.max_bytes)
    }

    fn preflight(&mut self, capacity: usize) -> Result<usize, TransportError> {
        let packet_bytes = self
            .packet_bytes
            .checked_add(capacity)
            .ok_or(TransportError::QueueFull)?;
        let required = self
            .packets
            .len()
            .checked_add(1)
            .ok_or(TransportError::QueueFull)?;
        if !self.fits(packet_bytes, self.packets.capacity().max(required))
            && self.packets.is_empty()
        {
            self.clear();
        }
        self.fits(packet_bytes, self.packets.capacity().max(required))
            .then_some(packet_bytes)
            .ok_or(TransportError::QueueFull)
    }

    pub(super) fn secure_packet(
        &mut self,
        layout: ProtectionLayout,
        prefix: Option<[u8; 2]>,
    ) -> Result<Vec<u8>, TransportError> {
        let minimum = layout
            .initialized_bytes
            .checked_add(prefix.map_or(0, |bytes| bytes.len()))
            .ok_or(TransportError::QueueFull)?;
        self.preflight(minimum)?;
        secure_output_buffer(layout, prefix)
    }

    pub(super) fn reserve_slot(
        &mut self,
        capacity: usize,
    ) -> Result<SlotReservation, TransportError> {
        let packet_bytes = self.preflight(capacity)?;
        if self.packets.len() < self.packets.capacity() {
            return Ok(SlotReservation {
                expanded: None,
                packet_bytes,
            });
        }
        let required = self
            .packets
            .len()
            .checked_add(1)
            .ok_or(TransportError::QueueFull)?;
        let doubled = self.packets.capacity().checked_mul(2).unwrap_or(required);
        let capacity = doubled.max(required);
        let capacity = if self.fits(packet_bytes, capacity) {
            capacity
        } else {
            required
        };
        let mut expanded = VecDeque::new();
        expanded
            .try_reserve_exact(capacity)
            .map_err(|_| TransportError::QueueFull)?;
        if !self.fits(packet_bytes, expanded.capacity()) {
            return Err(TransportError::QueueFull);
        }
        Ok(SlotReservation {
            expanded: Some(expanded),
            packet_bytes,
        })
    }

    pub(super) fn push_reserved(&mut self, packet: PreparedTransmit, reservation: SlotReservation) {
        debug_assert_eq!(
            self.packet_bytes.checked_add(packet.bytes.capacity()),
            Some(reservation.packet_bytes)
        );
        if let Some(mut expanded) = reservation.expanded {
            expanded.append(&mut self.packets);
            self.packets = expanded;
        }
        debug_assert!(self.packets.len() < self.packets.capacity());
        self.packets.push_back(packet);
        self.packet_bytes = reservation.packet_bytes;
    }

    pub(super) fn pop_front(&mut self) -> Option<PreparedTransmit> {
        let packet = self.packets.pop_front()?;
        debug_assert!(self.packet_bytes >= packet.bytes.capacity());
        self.packet_bytes = self.packet_bytes.saturating_sub(packet.bytes.capacity());
        Some(packet)
    }

    pub(super) fn retain(&mut self, mut keep: impl FnMut(&PreparedTransmit) -> bool) {
        self.packets.retain(|packet| {
            if keep(packet) {
                return true;
            }
            debug_assert!(self.packet_bytes >= packet.bytes.capacity());
            self.packet_bytes = self.packet_bytes.saturating_sub(packet.bytes.capacity());
            false
        });
    }

    pub(super) fn clear(&mut self) {
        self.packets = VecDeque::new();
        self.packet_bytes = 0;
    }

    pub(super) fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.packets.len()
    }

    #[cfg(test)]
    pub(super) fn capacity(&self) -> usize {
        self.packets.capacity()
    }

    #[cfg(test)]
    pub(super) fn front(&self) -> Option<&PreparedTransmit> {
        self.packets.front()
    }

    #[cfg(test)]
    pub(super) fn assert_storage(&self) -> usize {
        let packets: usize = self
            .packets
            .iter()
            .map(|packet| packet.bytes.capacity())
            .sum();
        assert_eq!(self.packet_bytes, packets);
        assert!(self.fits(packets, self.packets.capacity()));
        let backing = self
            .packets
            .capacity()
            .checked_mul(size_of::<PreparedTransmit>())
            .unwrap();
        packets.checked_add(backing).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_packet_capacity_and_staged_backing_fit_before_commit() {
        let mut bytes = Vec::with_capacity(64);
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        let mut queue = OutputQueue::new(usize::MAX);
        let reservation = queue.reserve_slot(bytes.capacity()).unwrap();
        let slots = reservation.expanded.as_ref().unwrap().capacity();
        let cost = bytes.capacity() + slots * size_of::<PreparedTransmit>();
        assert_eq!(reservation.packet_bytes, bytes.capacity());
        assert_eq!(queue.assert_storage(), 0);
        drop(reservation);
        assert_eq!(queue.capacity(), 0);
        assert_eq!(queue.assert_storage(), 0);

        let mut exact = OutputQueue::new(cost);
        let reservation = exact.reserve_slot(bytes.capacity()).unwrap();
        assert_eq!(reservation.packet_bytes, bytes.capacity());
        drop(reservation);
        assert_eq!(exact.assert_storage(), 0);
        let mut short = OutputQueue::new(cost - 1);
        assert!(matches!(
            short.reserve_slot(bytes.capacity()),
            Err(TransportError::QueueFull)
        ));
        assert_eq!(short.assert_storage(), 0);
        assert!(matches!(
            OutputQueue::new(0).reserve_slot(0),
            Err(TransportError::QueueFull)
        ));
        assert!(matches!(
            queue.reserve_slot(usize::MAX),
            Err(TransportError::QueueFull)
        ));
        assert_eq!(queue.assert_storage(), 0);
    }
}
