//! Portable scalar backend. Wildcard binding requires destination ancillary
//! support and is rejected rather than fabricating receive destinations.
use super::*;

pub(crate) struct Bound {
    socket: socket2::Socket,
    local_addr: SocketAddr,
}
pub(crate) async fn bind(options: &BindOptions) -> io::Result<Bound> {
    if options.mode == Mode::Batch || options.local_addr.ip().is_unspecified() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "portable UDP requires scalar mode and a specific local address",
        ));
    }
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(options.local_addr),
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    // A portable scalar socket has no pktinfo with which to recover the real
    // IPv4 destination of a dual-stack receive. Keep families separate.
    if options.local_addr.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.set_nonblocking(true)?;
    if options.reuse_port {
        #[cfg(unix)]
        socket.set_reuse_port(true)?;
        #[cfg(not(unix))]
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "UDP reuseport unavailable",
        ));
    }
    socket.set_recv_buffer_size(options.recv_buffer_bytes)?;
    socket.set_send_buffer_size(options.send_buffer_bytes)?;
    socket.bind(&options.local_addr.into())?;
    let local_addr = socket
        .local_addr()?
        .as_socket()
        .ok_or_else(|| invalid("non-IP UDP socket"))?;
    Ok(Bound { socket, local_addr })
}
impl Bound {
    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
    pub(crate) fn activate(self, _: &BindOptions) -> io::Result<Socket> {
        Ok(Socket {
            socket: tokio::net::UdpSocket::from_std(self.socket.into())?,
            local_addr: self.local_addr,
        })
    }
}
pub(crate) struct Socket {
    socket: tokio::net::UdpSocket,
    local_addr: SocketAddr,
}
impl Socket {
    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
    pub(crate) fn capabilities(&self) -> Capabilities {
        Capabilities {
            max_batch_messages: 1,
            gro: false,
            max_gso_segments: 1,
        }
    }
    pub(crate) async fn readable(&self) -> io::Result<()> {
        self.socket.readable().await
    }
    pub(crate) async fn writable(&self) -> io::Result<()> {
        self.socket.writable().await
    }
    pub(crate) fn recv(
        &mut self,
        arena: &mut ReceiveArena,
        max_datagram: usize,
        counters: &mut Counters,
    ) -> io::Result<()> {
        let bytes = arena.bytes.get_mut(..arena.slot_size).unwrap_or_default();
        let (len, source) = self.socket.try_recv_from(bytes)?;
        if len > max_datagram {
            counters.truncated_drops = counters.truncated_drops.saturating_add(1);
        } else if let Some(entry) = arena.entries.first_mut() {
            *entry = Some(Received {
                source,
                destination: self.local_addr,
                len,
                stride: len.max(1),
                received_at: ReceiveTime::now(),
            });
        }
        Ok(())
    }
    pub(crate) fn send(&mut self, entries: &[SendDatagram<'_>]) -> SendReport {
        submit_prefix(entries, 1, |group| {
            let Some(entry) = group.first() else {
                return Ok(0);
            };
            self.socket
                .try_send_to(entry.payload, entry.destination)
                .map(|_| 1)
        })
    }
}
