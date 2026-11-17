//! Bounded H264 RTP assembly for the experimental application decoder.
use pulsebeam_rtc::RtpPacket;

const MAX_ACCESS_UNIT: usize = 2 * 1024 * 1024;

#[derive(Default)]
pub(super) struct Assembly {
    timestamp: Option<u32>,
    next: Option<u16>,
    data: Vec<u8>,
    fragment: bool,
    key: bool,
    broken: bool,
    finished: bool,
    dependency_lost: bool,
    pub(super) incomplete: u64,
}
pub(super) struct AccessUnit {
    pub(super) timestamp: u32,
    pub(super) data: Vec<u8>,
    pub(super) key: bool,
    pub(super) missing_before: bool,
}
impl Assembly {
    pub(super) fn invalidate(&mut self) {
        self.dependency_lost = true;
    }

    pub(super) fn push(&mut self, packet: &RtpPacket) -> Option<AccessUnit> {
        let header = packet.header;
        if let Some(old) = self.timestamp {
            if i32::from_be_bytes(header.timestamp.wrapping_sub(old).to_be_bytes()) < 0 {
                return None;
            }
            if old == header.timestamp && self.finished {
                return None;
            }
            if old != header.timestamp {
                if !self.data.is_empty() {
                    self.incomplete += 1;
                    self.dependency_lost = true;
                }
                self.data.clear();
                self.fragment = false;
                self.key = false;
                self.broken = false;
                self.finished = false;
            }
        }
        self.timestamp = Some(header.timestamp);
        if self.next.is_some_and(|next| next != header.sequence) {
            self.dependency_lost = true;
            if !self.data.is_empty() || self.fragment {
                self.broken = true;
            }
        }
        self.next = Some(header.sequence.wrapping_add(1));
        if !self.broken && self.append(&packet.payload).is_none() {
            self.broken = true;
        }
        if self.data.len() > MAX_ACCESS_UNIT {
            self.broken = true;
        }
        if self.broken {
            self.dependency_lost = true;
            self.data.clear();
        }
        if !header.marker {
            return None;
        }
        self.finished = true;
        let valid = !self.broken && !self.fragment && !self.data.is_empty();
        let data = std::mem::take(&mut self.data);
        if !valid {
            self.incomplete += 1;
            self.dependency_lost = true;
            self.broken = true;
            return None;
        }
        let missing_before = self.dependency_lost;
        if self.key {
            self.dependency_lost = false;
        }
        Some(AccessUnit {
            timestamp: header.timestamp,
            data,
            key: self.key,
            missing_before,
        })
    }

    fn append(&mut self, payload: &[u8]) -> Option<()> {
        let &indicator = payload.first()?;
        match indicator & 31 {
            1..=23 if !self.fragment => {
                self.key |= indicator & 31 == 5;
                self.data.extend_from_slice(&[0, 0, 0, 1]);
                self.data.extend_from_slice(payload);
            }
            24 if !self.fragment => {
                let mut units = payload.get(1..)?;
                while !units.is_empty() {
                    let length = usize::from(u16::from_be_bytes(units.get(..2)?.try_into().ok()?));
                    if length == 0 {
                        return None;
                    }
                    let nal = units.get(2..length.checked_add(2)?)?;
                    if !(1..=23).contains(&(nal.first()? & 31)) {
                        return None;
                    }
                    self.key |= nal[0] & 31 == 5;
                    self.data.extend_from_slice(&[0, 0, 0, 1]);
                    self.data.extend_from_slice(nal);
                    units = units.get(length.checked_add(2)?..)?;
                }
            }
            28 => {
                let &fu = payload.get(1)?;
                let start = fu & 128 != 0;
                let end = fu & 64 != 0;
                if start && end || start == self.fragment || fu & 32 != 0 || fu & 31 == 0 {
                    return None;
                }
                if start {
                    self.data
                        .extend_from_slice(&[0, 0, 0, 1, (indicator & 0xe0) | (fu & 31)]);
                    self.key |= fu & 31 == 5;
                }
                self.data.extend_from_slice(payload.get(2..)?);
                self.fragment = !end;
            }
            _ => return None,
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_rtc::RtpHeader;
    use std::time::Instant;
    fn packet(sequence: u16, timestamp: u32, marker: bool, payload: &[u8]) -> RtpPacket {
        RtpPacket {
            arrival: Instant::now(),
            header: RtpHeader {
                sequence,
                timestamp,
                ssrc: 1,
                payload_type: 96,
                marker,
            },
            payload: payload.to_vec().into(),
            extensions: vec![],
        }
    }
    #[test]
    fn single_stap_fragment_gap_and_reorder_have_explicit_bounded_outcomes() {
        let mut assembly = Assembly::default();
        assert!(
            assembly
                .push(&packet(
                    65535,
                    10,
                    false,
                    &[24, 0, 2, 0x67, 7, 0, 2, 0x68, 8]
                ))
                .is_none()
        );
        assert!(
            assembly
                .push(&packet(0, 10, false, &[0x7c, 0x85, 1, 2]))
                .is_none()
        );
        let au = assembly
            .push(&packet(1, 10, true, &[0x7c, 0x45, 3, 4]))
            .unwrap();
        assert!(au.key);
        assert_eq!(
            au.data,
            [
                0, 0, 0, 1, 0x67, 7, 0, 0, 0, 1, 0x68, 8, 0, 0, 0, 1, 0x65, 1, 2, 3, 4
            ]
        );
        assert!(assembly.push(&packet(1, 10, true, &[0x65, 1])).is_none());
        assert!(
            assembly
                .push(&packet(2, 20, false, &[0x7c, 0x81, 1]))
                .is_none()
        );
        assert!(
            assembly
                .push(&packet(4, 20, true, &[0x7c, 0x41, 3]))
                .is_none()
        );
        let au = assembly.push(&packet(5, 30, true, &[0x61, 9])).unwrap();
        assert!(!au.key);
        assert!(au.missing_before);
        assert_eq!(au.timestamp, 30);
        assert!(assembly.push(&packet(3, 20, true, &[0x61, 2])).is_none());
        assert_eq!(assembly.incomplete, 1);
        assert!(
            assembly
                .push(&packet(6, 40, true, &[24, 0, 10, 0x61]))
                .is_none()
        );
        assert_eq!(assembly.incomplete, 2);
        let recovery = assembly.push(&packet(7, 50, true, &[0x65, 1])).unwrap();
        assert!(recovery.key && recovery.missing_before);
        let delta = assembly.push(&packet(8, 60, true, &[0x61, 2])).unwrap();
        assert!(!delta.missing_before);
        assembly.invalidate();
        assert!(
            assembly
                .push(&packet(9, 70, true, &[0x61, 3]))
                .unwrap()
                .missing_before
        );

        let mut too_large = vec![1; MAX_ACCESS_UNIT];
        too_large[0] = 0x65;
        assert!(assembly.push(&packet(10, 80, true, &too_large)).is_none());
        assert_eq!(assembly.incomplete, 3);
        assert!(assembly.data.is_empty());
    }
}
