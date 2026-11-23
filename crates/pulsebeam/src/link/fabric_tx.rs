use pulsebeam_link::{LinkTime, LinkTx};
use pulsebeam_runtime::time::Instant;

use crate::{route::fabric::local::OwnedPacket, shard::worker::ShardFrame};

use super::wan_host::WanTx;

/// A compiled Fabric branch has one carrier for its entire binding lifetime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FabricTx {
    Legacy {
        tx: LinkTx<ShardFrame>,
        clock_origin: Instant,
    },
    Wan(WanTx),
}

impl FabricTx {
    pub(crate) fn legacy(tx: LinkTx<ShardFrame>, clock_origin: Instant) -> Self {
        Self::Legacy { tx, clock_origin }
    }

    pub(crate) fn is_wan(&self) -> bool {
        matches!(self, Self::Wan(_))
    }

    pub(crate) fn is_live(&self) -> bool {
        match self {
            Self::Legacy { tx, .. } => tx.is_live(),
            Self::Wan(tx) => tx.is_live(),
        }
    }

    pub(crate) fn covers(&self, position: u64) -> bool {
        match self {
            Self::Legacy { .. } => true,
            Self::Wan(tx) => tx.covers(position),
        }
    }

    pub(crate) fn try_send(
        &self,
        now: Instant,
        deadline: Instant,
        position: u64,
        packet: OwnedPacket,
    ) -> bool {
        if now >= deadline {
            return false;
        }
        match self {
            Self::Legacy { tx, clock_origin } => {
                let Some(elapsed) = deadline.checked_duration_since(*clock_origin) else {
                    return false;
                };
                let expires =
                    LinkTime::from_nanos(u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX));
                tx.try_send_fabric(ShardFrame::FabricImage(packet), position, expires)
                    .is_ok()
            }
            Self::Wan(tx) => tx.try_send(now, deadline, position, packet),
        }
    }
}
