pub mod pulsebeam {
    pub mod v1 {
        tonic::include_proto!("pulsebeam.v1");

        impl Ord for PeerInfo {
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                match self.group_id.cmp(&other.group_id) {
                    std::cmp::Ordering::Equal => match self.peer_id.cmp(&other.peer_id) {
                        std::cmp::Ordering::Equal => self.conn_id.cmp(&other.conn_id),
                        ordering => ordering,
                    },
                    ordering => ordering,
                }
            }
        }

        impl PartialOrd for PeerInfo {
            fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }

        impl std::fmt::Display for PeerInfo {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}:{}:{}", self.group_id, self.peer_id, self.conn_id)
            }
        }

        pub const FILE_DESCRIPTOR_SET: &[u8] = tonic::include_file_descriptor_set!("descriptor");
    }
}
pub use pulsebeam::v1::*;
