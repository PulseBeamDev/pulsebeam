//! Bounded simulated I/O adapter. The host owns network effects and steering.
//!
//! An adapter accepts a whole entry atomically. Loss/delay/duplication after
//! that boundary is network behavior, not a failed local submission.
use super::*;
use std::{future::Future, pin::Pin};

pub type Readiness<'a> = Pin<Box<dyn Future<Output = io::Result<()>> + 'a>>;

pub struct Metadata {
    pub source: SocketAddr,
    pub destination: SocketAddr,
    pub len: usize,
    pub stride: usize,
    pub received_at: ReceiveTime,
}

/// Only the simulator implements this adapter. Production callers use the
/// concrete `UdpSocket`, not a generic transport abstraction.
pub trait Io {
    fn local_addr(&self) -> SocketAddr;
    fn capabilities(&self) -> Capabilities;
    fn readable(&self) -> Readiness<'_>;
    fn writable(&self) -> Readiness<'_>;
    fn receive(&mut self, payload: &mut [u8]) -> io::Result<Metadata>;
    fn submit(&mut self, entry: &SendDatagram<'_>) -> io::Result<()>;
}

pub fn from_io(options: BindOptions, io: Box<dyn Io>) -> io::Result<BoundUdpSocket> {
    options.validate()?;
    let caps = io.capabilities();
    if caps.max_batch_messages == 0
        || caps.max_batch_messages > BATCH_SIZE
        || caps.max_gso_segments == 0
        || caps.max_gso_segments > MAX_GSO_SEGMENTS
        || (options.mode == Mode::Scalar
            && (caps.gro || caps.max_gso_segments != 1 || caps.max_batch_messages != 1))
    {
        return Err(invalid("invalid simulated UDP capabilities"));
    }
    Ok(BoundUdpSocket {
        inner: Bound { io },
        options,
    })
}

pub(crate) struct Bound {
    io: Box<dyn Io>,
}
impl Bound {
    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.io.local_addr()
    }
    pub(crate) fn activate(self, _: &BindOptions) -> io::Result<Socket> {
        let caps = self.io.capabilities();
        Ok(Socket { io: self.io, caps })
    }
}

pub(crate) async fn bind(options: &BindOptions) -> io::Result<Bound> {
    if options.reuse_port || options.mode == Mode::Batch {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "install a host simulated batch/reuseport I/O adapter",
        ));
    }
    let socket = turmoil::net::UdpSocket::bind(options.local_addr).await?;
    let local_addr = socket.local_addr()?;
    if local_addr.ip().is_unspecified() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "wildcard simulation requires destination-aware host I/O",
        ));
    }
    Ok(Bound {
        io: Box::new(Turmoil { socket, local_addr }),
    })
}

pub(crate) struct Socket {
    io: Box<dyn Io>,
    caps: Capabilities,
}
impl Socket {
    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.io.local_addr()
    }
    pub(crate) fn capabilities(&self) -> Capabilities {
        self.caps
    }
    pub(crate) async fn readable(&self) -> io::Result<()> {
        self.io.readable().await
    }
    pub(crate) async fn writable(&self) -> io::Result<()> {
        self.io.writable().await
    }
    pub(crate) fn recv(
        &mut self,
        arena: &mut ReceiveArena,
        max_datagram: usize,
        counters: &mut Counters,
    ) -> io::Result<()> {
        let mut received = false;
        for (slot, bytes) in arena
            .entries
            .iter_mut()
            .zip(arena.bytes.chunks_exact_mut(arena.slot_size))
            .take(self.caps.max_batch_messages)
        {
            match self.io.receive(bytes) {
                Ok(meta) => {
                    received = true;
                    if meta.len > bytes.len() {
                        counters.truncated_drops = counters.truncated_drops.saturating_add(1);
                    } else if meta.destination.ip().is_unspecified()
                        || meta.stride == 0
                        || meta.stride > max_datagram
                        || meta.len.max(1).div_ceil(meta.stride) > MAX_GSO_SEGMENTS
                        || (!self.caps.gro && meta.len > meta.stride)
                        || (meta.len > 0 && meta.stride > meta.len)
                    {
                        counters.invalid_metadata_drops =
                            counters.invalid_metadata_drops.saturating_add(1);
                    } else {
                        *slot = Some(Received {
                            source: meta.source,
                            destination: meta.destination,
                            len: meta.len,
                            stride: meta.stride,
                            received_at: meta.received_at,
                        });
                    }
                }
                Err(error) if received && error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
    pub(crate) fn send(&mut self, entries: &[SendDatagram<'_>]) -> SendReport {
        // One adapter call is one atomic input entry, including a GSO aggregate.
        submit_prefix(entries, 1, |group| {
            let Some(entry) = group.first() else {
                return Ok(0);
            };
            self.io.submit(entry)?;
            Ok(1)
        })
    }
}

struct Turmoil {
    socket: turmoil::net::UdpSocket,
    local_addr: SocketAddr,
}
impl Io for Turmoil {
    fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            max_batch_messages: 1,
            gro: false,
            max_gso_segments: 1,
        }
    }
    fn readable(&self) -> Readiness<'_> {
        Box::pin(self.socket.readable())
    }
    fn writable(&self) -> Readiness<'_> {
        Box::pin(self.socket.writable())
    }
    fn receive(&mut self, payload: &mut [u8]) -> io::Result<Metadata> {
        let (len, source) = self.socket.try_recv_from(payload)?;
        Ok(Metadata {
            source,
            destination: self.local_addr,
            len,
            stride: len.max(1),
            received_at: ReceiveTime::now(),
        })
    }
    fn submit(&mut self, entry: &SendDatagram<'_>) -> io::Result<()> {
        self.socket
            .try_send_to(entry.payload, entry.destination)
            .map(|_| ())
    }
}
