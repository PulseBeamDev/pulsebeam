//! A bounded control lane on a provisioned authenticated association.

use super::{Fence, LinkTime, Ordering, SendErrorKind, Shared, network};

pub(super) use pulsebeam_transport::CONTROL_CONTEXT as CONTEXT;
pub use pulsebeam_transport::CONTROL_FRAME_BYTES;
const CAPACITY: std::num::NonZeroUsize = const { std::num::NonZeroUsize::new(16).unwrap() };

pub struct PeerControl {
    state: Shared<Fence>,
    outbound: pulsebeam_queue::Sender<Chunk>,
    inbound: pulsebeam_queue::Receiver<Box<[u8]>>,
}

impl PeerControl {
    pub fn is_ready(&self) -> bool {
        self.state.load(Ordering::Acquire) == network::READY
    }

    pub fn is_closed(&self) -> bool {
        self.state.load(Ordering::Acquire) == network::CLOSED
    }

    pub fn try_send(&self, bytes: Box<[u8]>, expires_at: LinkTime) -> Result<(), SendErrorKind> {
        if self.is_closed() {
            return Err(SendErrorKind::Closed);
        }
        if !self.is_ready() {
            return Err(SendErrorKind::NotReady);
        }
        if bytes.len() > CONTROL_FRAME_BYTES {
            return Err(SendErrorKind::TooLarge);
        }
        self.outbound
            .try_send(Chunk { bytes, expires_at })
            .map_err(|error| match error {
                pulsebeam_queue::TrySendError::Full(_) => SendErrorKind::Full,
                pulsebeam_queue::TrySendError::Closed(_) => SendErrorKind::Closed,
            })
    }

    pub fn try_recv(&mut self) -> Option<Box<[u8]>> {
        if self.is_closed() {
            return None;
        }
        let bytes = self.inbound.try_recv().ok()?;
        (!self.is_closed()).then_some(bytes)
    }

    pub async fn recv(&mut self) -> Option<Box<[u8]>> {
        if self.is_closed() {
            return None;
        }
        let bytes = self.inbound.recv().await?;
        (!self.is_closed()).then_some(bytes)
    }
}

pub(super) struct Chunk {
    pub(super) bytes: Box<[u8]>,
    pub(super) expires_at: LinkTime,
}

pub(super) struct HostLane {
    pub(super) outgoing: pulsebeam_queue::Receiver<Chunk>,
    pub(super) incoming: pulsebeam_queue::Sender<Box<[u8]>>,
    pub(super) pending: Option<Chunk>,
}

pub(super) fn pair(state: Shared<Fence>) -> (PeerControl, HostLane) {
    let (outbound, outgoing) = pulsebeam_queue::bounded(CAPACITY);
    let (incoming, inbound) = pulsebeam_queue::bounded(CAPACITY);
    (
        PeerControl {
            state,
            outbound,
            inbound,
        },
        HostLane {
            outgoing,
            incoming,
            pending: None,
        },
    )
}
