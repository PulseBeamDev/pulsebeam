//! Bounded control delivery on a provisioned peer session.

use std::{
    future::Future,
    pin::pin,
    task::{Context, Poll},
};

use super::{Fence, LinkTime, Ordering, SendErrorKind, Shared, network};

pub(super) use pulsebeam_transport::CONTROL_CONTEXT as CONTEXT;
pub use pulsebeam_transport::CONTROL_FRAME_BYTES;
const CAPACITY: std::num::NonZeroUsize = const { std::num::NonZeroUsize::new(16).unwrap() };

pub struct PeerControl {
    state: Shared<Fence>,
    outbound: pulsebeam_queue::Sender<Chunk>,
    inbound: pulsebeam_queue::Receiver<Box<[u8]>>,
    frame_bytes: usize,
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
        if bytes.len() > self.frame_bytes {
            return Err(SendErrorKind::TooLarge);
        }
        self.outbound
            .try_send(Chunk { bytes, expires_at })
            .map_err(|error| match error {
                pulsebeam_queue::TrySendError::Full(_) => SendErrorKind::Full,
                pulsebeam_queue::TrySendError::Closed(_) => SendErrorKind::Closed,
            })
    }

    /// Register interest in inbound control without consuming a frame.
    pub fn poll_readable(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if !self.is_ready() {
            return Poll::Pending;
        }
        match pin!(self.inbound.readable()).poll(cx) {
            Poll::Ready(true) => Poll::Ready(()),
            _ => Poll::Pending,
        }
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

/// The transport end of one ordered control stream. Construct it only after
/// authenticating the provisioned neighbor. Dropping it fences this incarnation
/// of the stream, even if a replacement stream is already connected.
pub struct StreamControl {
    state: Shared<Fence>,
    outbound: StreamOutbound,
    inbound: StreamInbound,
}

impl StreamControl {
    pub fn new_authenticated(frame_bytes: std::num::NonZeroUsize) -> (PeerControl, Self) {
        let state = Shared::new(Fence::new(network::READY));
        let frame_bytes = frame_bytes.get();
        let (peer, host) = pair_with_limit(state.clone(), frame_bytes);
        (
            peer,
            Self {
                state,
                outbound: StreamOutbound(host.outgoing),
                inbound: StreamInbound {
                    incoming: host.incoming,
                    frame_bytes,
                },
            },
        )
    }

    /// Borrow the two directions independently, so backpressure in one does not
    /// stop the transport from driving the other.
    pub fn directions(&mut self) -> (&mut StreamOutbound, &mut StreamInbound) {
        (&mut self.outbound, &mut self.inbound)
    }
}

impl Drop for StreamControl {
    fn drop(&mut self) {
        self.state.store(network::CLOSED, Ordering::Release);
    }
}

pub struct StreamOutbound(pulsebeam_queue::Receiver<Chunk>);

impl StreamOutbound {
    /// The driver must check the deadline in the lane's local clock domain
    /// before sending the frame. Expiration is not a wire timestamp.
    pub async fn recv(&mut self) -> Option<(Box<[u8]>, LinkTime)> {
        let chunk = self.0.recv().await?;
        Some((chunk.bytes, chunk.expires_at))
    }
}

pub struct StreamInbound {
    incoming: pulsebeam_queue::Sender<Box<[u8]>>,
    frame_bytes: usize,
}

impl StreamInbound {
    pub async fn send(&mut self, frame: Box<[u8]>) -> Result<(), SendErrorKind> {
        if frame.len() > self.frame_bytes {
            return Err(SendErrorKind::TooLarge);
        }
        self.incoming
            .send(frame)
            .await
            .map_err(|_| SendErrorKind::Closed)
    }
}

pub(super) fn pair(state: Shared<Fence>) -> (PeerControl, HostLane) {
    pair_with_limit(state, CONTROL_FRAME_BYTES)
}

fn pair_with_limit(state: Shared<Fence>, frame_bytes: usize) -> (PeerControl, HostLane) {
    let (outbound, outgoing) = pulsebeam_queue::bounded(CAPACITY);
    let (incoming, inbound) = pulsebeam_queue::bounded(CAPACITY);
    (
        PeerControl {
            state,
            outbound,
            inbound,
            frame_bytes,
        },
        HostLane {
            outgoing,
            incoming,
            pending: None,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_lite::future::{block_on, poll_once};

    #[test]
    fn inbound_control_wakes_reader_without_consuming_frame() {
        use std::{
            sync::{Arc, atomic::AtomicBool},
            task::{Wake, Waker},
        };
        struct Signal(AtomicBool);
        impl Wake for Signal {
            fn wake(self: Arc<Self>) {
                self.0.store(true, Ordering::Release);
            }
        }
        let signal = Arc::new(Signal(AtomicBool::new(false)));
        let waker = Waker::from(signal.clone());
        let mut cx = Context::from_waker(&waker);
        let (mut peer, mut stream) = StreamControl::new_authenticated(std::num::NonZeroUsize::MIN);
        assert_eq!(peer.poll_readable(&mut cx), Poll::Pending);
        let (_, inbound) = stream.directions();
        block_on(inbound.send(Box::from([7]))).unwrap();
        assert!(signal.0.load(Ordering::Acquire));
        assert_eq!(peer.poll_readable(&mut cx), Poll::Ready(()));
        assert_eq!(peer.try_recv().as_deref(), Some(&[7][..]));
        assert_eq!(peer.poll_readable(&mut cx), Poll::Pending);
    }

    #[test]
    fn stream_is_bounded_ordered_and_fenced_on_drop() {
        block_on(async {
            let limit = CONTROL_FRAME_BYTES.saturating_mul(2);
            let (mut peer, mut stream) =
                StreamControl::new_authenticated(std::num::NonZeroUsize::new(limit).unwrap());
            assert!(peer.is_ready());
            let (outbound, inbound) = stream.directions();
            let deadline = LinkTime::from_nanos(1);
            let frame = |byte| Box::<[u8]>::from([byte]);
            for index in 0..u8::try_from(CAPACITY.get()).unwrap() {
                assert_eq!(peer.try_send(frame(index), deadline), Ok(()));
                assert_eq!(inbound.send(frame(index)).await, Ok(()));
            }
            assert_eq!(
                peer.try_send(frame(255), deadline),
                Err(SendErrorKind::Full)
            );
            assert!(poll_once(inbound.send(frame(255))).await.is_none());
            assert_eq!(
                inbound
                    .send(vec![0; limit.saturating_add(1)].into_boxed_slice())
                    .await,
                Err(SendErrorKind::TooLarge)
            );
            for index in 0..u8::try_from(CAPACITY.get()).unwrap() {
                assert_eq!(outbound.recv().await, Some((frame(index), deadline)));
                assert_eq!(peer.recv().await, Some(frame(index)));
            }
            assert!(poll_once(outbound.recv()).await.is_none());
            assert_eq!(inbound.send(frame(0)).await, Ok(()));
            drop(stream);
            assert!(peer.is_closed());
            assert_eq!(
                peer.try_send(frame(0), deadline),
                Err(SendErrorKind::Closed)
            );
            assert_eq!(peer.recv().await, None);
        });
    }
}
