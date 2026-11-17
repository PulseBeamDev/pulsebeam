//! Retain produced access units, matching native encoded-input ownership.
//! The decode deadline measures delivery; it is not a QUIC-only discard rule.
use super::*;

struct QueuedPicture {
    capture: usize,
    epoch: u64,
    total: usize,
    packets: VecDeque<Packet>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TerminalPicture {
    pub(super) capture: usize,
    pub(super) epoch: u64,
    pub(super) admitted_fragments: usize,
    pub(super) queued_fragments: usize,
}

#[derive(Default)]
pub(super) struct PictureQueue {
    pictures: VecDeque<QueuedPicture>,
    pub(super) admitted_pictures: u64,
}
impl PictureQueue {
    pub(super) fn push_packets(&mut self, id: usize, epoch: u64, packets: Vec<Packet>) {
        assert!(!packets.is_empty());
        let packets: VecDeque<_> = packets
            .into_iter()
            .map(|mut packet| {
                packet.expires = Time(u64::MAX);
                packet
            })
            .collect();
        self.pictures.push_back(QueuedPicture {
            capture: id,
            epoch,
            total: packets.len(),
            packets,
        });
    }
    pub(super) fn is_empty(&self) -> bool {
        self.pictures.is_empty()
    }
    pub(super) fn front(&self) -> Option<&Packet> {
        self.pictures.front()?.packets.front()
    }
    pub(super) fn accepted(&mut self) {
        let picture = self.pictures.front_mut().unwrap();
        picture.packets.pop_front().unwrap();
        if picture.packets.is_empty() {
            self.pictures.pop_front();
            self.admitted_pictures += 1;
        }
    }

    pub(super) fn retire(&mut self) -> Vec<TerminalPicture> {
        self.pictures
            .drain(..)
            .map(|picture| TerminalPicture {
                capture: picture.capture,
                epoch: picture.epoch,
                admitted_fragments: picture.total - picture.packets.len(),
                queued_fragments: picture.packets.len(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn backpressure_retains_the_rest_of_an_encoded_picture_without_caller_expiry() {
        let packets = || {
            (0..3)
                .map(|_| Packet {
                    bytes: vec![1; CHUNK],
                    expires: Time(0),
                })
                .collect()
        };
        let mut queue = PictureQueue::default();
        queue.push_packets(0, 0, packets());
        queue.accepted();
        let retry = queue.front().unwrap().bytes.clone();
        assert_eq!(queue.front().unwrap().expires, Time(u64::MAX));
        assert_eq!(queue.front().unwrap().bytes, retry);
        queue.accepted();
        queue.accepted();
        assert!(queue.is_empty());
        assert_eq!(queue.admitted_pictures, 1);
        assert!(queue.retire().is_empty());

        queue.push_packets(0, 7, packets());
        queue.accepted();
        queue.push_packets(0, 8, packets());
        assert_eq!(
            queue.retire(),
            [
                TerminalPicture {
                    capture: 0,
                    epoch: 7,
                    admitted_fragments: 1,
                    queued_fragments: 2
                },
                TerminalPicture {
                    capture: 0,
                    epoch: 8,
                    admitted_fragments: 0,
                    queued_fragments: 3
                },
            ]
        );
        assert!(queue.is_empty());
        assert!(queue.retire().is_empty());
        assert_eq!(queue.admitted_pictures, 1);
    }
}
