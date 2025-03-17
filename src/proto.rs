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
    }
}
pub use pulsebeam::v1::*;
