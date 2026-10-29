//! Pure affinity decisions over caller-owned flow mappings.

use crate::{DropReason, ROUTE_SHARD_BITS};

pub const MAX_SHARDS: u32 = 1 << ROUTE_SHARD_BITS;

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FlowKey {
    pub src_addr: [u8; 16],
    pub dst_addr: [u8; 16],
    pub src_port: u16,
    pub dst_port: u16,
    pub is_ipv6: u8,
    pub _pad: [u8; 3],
}

/// The wire length of a [`FlowKey`]. Named so the copy helper and the layout
/// assertion cannot drift apart.
pub const FLOW_KEY_LEN: usize = 40;

const _: () = assert!(core::mem::size_of::<FlowKey>() == FLOW_KEY_LEN);

impl FlowKey {
    /// Explicit native-endian map representation, with zeroed padding.
    pub fn to_ne_bytes(self) -> [u8; FLOW_KEY_LEN] {
        let mut bytes = [0u8; FLOW_KEY_LEN];
        let mut write = |offset: usize, source: &[u8]| {
            if let Some(target) = bytes.get_mut(offset..offset.saturating_add(source.len())) {
                target.copy_from_slice(source);
            } else {
                debug_assert!(
                    false,
                    "FlowKey field at {offset} lies outside its own layout"
                );
            }
        };
        write(0, &self.src_addr);
        write(16, &self.dst_addr);
        write(32, &self.src_port.to_ne_bytes());
        write(34, &self.dst_port.to_ne_bytes());
        write(36, &[self.is_ipv6]);
        bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
    Select { shard: u16 },
    Pass,
    Drop(DropReason),
}

pub trait FlowLookup {
    fn get(&self, flow: &FlowKey) -> Option<u16>;
}

/// Missing, out-of-range and stale affinity all fall back to userspace.
pub fn steer_flow(lookup: &impl FlowLookup, key: &FlowKey, shard_count: u16) -> Verdict {
    match lookup.get(key) {
        Some(shard) if shard < shard_count && u32::from(shard) < MAX_SHARDS => {
            Verdict::Select { shard }
        }
        _ => Verdict::Pass,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Lookup(Option<u16>);
    impl FlowLookup for Lookup {
        fn get(&self, _: &FlowKey) -> Option<u16> {
            self.0
        }
    }

    fn key() -> FlowKey {
        FlowKey {
            src_addr: [1; 16],
            dst_addr: [2; 16],
            src_port: 1000,
            dst_port: 3478,
            is_ipv6: 0,
            _pad: [0; 3],
        }
    }

    #[test]
    fn affinity_hit_miss_stale_and_invalid_use_safe_fallback() {
        assert_eq!(
            steer_flow(&Lookup(Some(2)), &key(), 4),
            Verdict::Select { shard: 2 }
        );
        for (value, count) in [
            (None, 4),
            (Some(4), 4),
            (Some(0), 0),
            (Some(4096), u16::MAX),
            (Some(u16::MAX), u16::MAX),
        ] {
            assert_eq!(steer_flow(&Lookup(value), &key(), count), Verdict::Pass);
        }
    }

    #[test]
    fn flow_key_layout_and_native_bytes_are_stable() {
        assert_eq!(core::mem::size_of::<FlowKey>(), 40);
        assert_eq!(core::mem::offset_of!(FlowKey, src_addr), 0);
        assert_eq!(core::mem::offset_of!(FlowKey, dst_addr), 16);
        assert_eq!(core::mem::offset_of!(FlowKey, src_port), 32);
        assert_eq!(core::mem::offset_of!(FlowKey, dst_port), 34);
        assert_eq!(core::mem::offset_of!(FlowKey, is_ipv6), 36);
        assert_eq!(core::mem::offset_of!(FlowKey, _pad), 37);
        let mut key = key();
        key._pad = [0xFF; 3];
        let bytes = key.to_ne_bytes();
        assert_eq!(&bytes[..16], &[1; 16]);
        assert_eq!(&bytes[16..32], &[2; 16]);
        assert_eq!(&bytes[32..34], &1000u16.to_ne_bytes());
        assert_eq!(&bytes[34..36], &3478u16.to_ne_bytes());
        assert_eq!(&bytes[36..], &[0; 4]);
    }
}
