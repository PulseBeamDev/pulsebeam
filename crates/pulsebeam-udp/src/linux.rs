use super::*;
use nix::{
    cmsg_space,
    errno::Errno,
    libc,
    sys::socket::{
        ControlMessage, ControlMessageOwned, MsgFlags, MultiHeaders, SockaddrStorage, recvmmsg,
        recvmsg, sendmmsg, sendmsg, setsockopt, sockopt,
    },
};
use std::{
    io::{IoSlice, IoSliceMut},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    os::fd::{AsFd, AsRawFd, BorrowedFd},
};

pub(crate) struct Bound {
    socket: socket2::Socket,
    local_addr: SocketAddr,
}

impl AsFd for Bound {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.socket.as_fd()
    }
}

pub(crate) async fn bind(options: &BindOptions) -> io::Result<Bound> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(options.local_addr),
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    if options.local_addr.is_ipv6() {
        socket.set_only_v6(false)?;
    }
    if options.nonfragmenting_max_datagram_bytes.is_some() {
        // A dual-stack socket also transmits IPv4-mapped datagrams.
        set_ip_option(
            &socket,
            libc::IPPROTO_IP,
            libc::IP_MTU_DISCOVER,
            libc::IP_PMTUDISC_DO,
        )?;
        if options.local_addr.is_ipv6() {
            set_ip_option(
                &socket,
                libc::IPPROTO_IPV6,
                libc::IPV6_MTU_DISCOVER,
                libc::IP_PMTUDISC_DO,
            )?;
            set_ip_option(&socket, libc::IPPROTO_IPV6, libc::IPV6_DONTFRAG, 1)?;
        }
    }
    if options.receive_ecn {
        setsockopt(&socket, sockopt::IpRecvTos, &true)?;
        if options.local_addr.is_ipv6() {
            setsockopt(&socket, sockopt::Ipv6RecvTClass, &true)?;
        }
    }
    socket.set_nonblocking(true)?;
    if options.reuse_port {
        socket.set_reuse_port(true)?;
    }
    socket.set_recv_buffer_size(options.recv_buffer_bytes)?;
    socket.set_send_buffer_size(options.send_buffer_bytes)?;
    socket.bind(&options.local_addr.into())?;
    let local_addr = socket
        .local_addr()?
        .as_socket()
        .ok_or_else(|| invalid("non-IP UDP socket"))?;
    match local_addr {
        SocketAddr::V4(_) => setsockopt(&socket, sockopt::Ipv4PacketInfo, &true)?,
        SocketAddr::V6(_) => {
            setsockopt(&socket, sockopt::Ipv6RecvPacketInfo, &true)?;
            setsockopt(&socket, sockopt::Ipv4PacketInfo, &true)?;
        }
    }
    Ok(Bound { socket, local_addr })
}

fn set_ip_option(
    socket: &socket2::Socket,
    level: libc::c_int,
    option: libc::c_int,
    mode: libc::c_int,
) -> io::Result<()> {
    let len = libc::socklen_t::try_from(std::mem::size_of_val(&mode))
        .map_err(|_| invalid("unsupported path MTU option size"))?;
    // SAFETY: the owned socket stays live for this call; the integer option
    // is initialized and `len` describes exactly that integer.
    let result = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            level,
            option,
            std::ptr::from_ref(&mode).cast(),
            len,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl Bound {
    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
    pub(crate) fn activate(self, options: &BindOptions) -> io::Result<Socket> {
        let batch = options.mode != Mode::Scalar;
        let gro = batch && setsockopt(&self.socket, sockopt::UdpGroSegment, &true).is_ok();
        let gso = if batch && setsockopt(&self.socket, sockopt::UdpGsoSegment, &1200).is_ok() {
            // Leaving the probe enabled would silently segment ordinary sends.
            setsockopt(&self.socket, sockopt::UdpGsoSegment, &0)?;
            true
        } else {
            false
        };
        Ok(Socket {
            local_addr: self.local_addr,
            socket: tokio::io::unix::AsyncFd::try_new(self.socket)
                .map_err(|error| error.into_parts().1)?,
            recv_cmsg: cmsg_space!(i32, libc::in_pktinfo, libc::in6_pktinfo, u8, i32),
            send_headers: SendHeaders(MultiHeaders::preallocate(BATCH_SIZE, None)),
            gso_headers: SendHeaders(MultiHeaders::preallocate(
                BATCH_SIZE,
                Some(cmsg_space!(u16)),
            )),
            caps: Capabilities {
                max_batch_messages: if batch { BATCH_SIZE } else { 1 },
                gro,
                max_gso_segments: if gso { MAX_GSO_SEGMENTS } else { 1 },
            },
        })
    }
}

pub(crate) struct Socket {
    socket: tokio::io::unix::AsyncFd<socket2::Socket>,
    local_addr: SocketAddr,
    caps: Capabilities,
    recv_cmsg: Vec<u8>,
    send_headers: SendHeaders,
    gso_headers: SendHeaders,
}

pub(crate) struct Detached {
    socket: socket2::Socket,
    local_addr: SocketAddr,
    caps: Capabilities,
    recv_cmsg: Vec<u8>,
    send_headers: SendHeaders,
    gso_headers: SendHeaders,
}

impl AsFd for Detached {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.socket.as_fd()
    }
}

impl Detached {
    pub(crate) fn activate(self) -> Result<Socket, (Box<Self>, io::Error)> {
        if tokio::runtime::Handle::try_current().is_err() {
            return Err((Box::new(self), io::Error::other("no current Tokio runtime")));
        }
        let Self {
            socket,
            local_addr,
            caps,
            recv_cmsg,
            send_headers,
            gso_headers,
        } = self;
        match tokio::io::unix::AsyncFd::try_new(socket) {
            Ok(socket) => Ok(Socket {
                socket,
                local_addr,
                caps,
                recv_cmsg,
                send_headers,
                gso_headers,
            }),
            Err(error) => {
                let (socket, error) = error.into_parts();
                Err((
                    Box::new(Self {
                        socket,
                        local_addr,
                        caps,
                        recv_cmsg,
                        send_headers,
                        gso_headers,
                    }),
                    error,
                ))
            }
        }
    }
}

struct SendHeaders(MultiHeaders<SockaddrStorage>);

// SAFETY: nix 0.31 keeps its owned header/address/cmsg arrays in stable Box
// allocations. Moving these arrays does not move their pointees. Transient
// iov/address pointers are overwritten by sendmmsg before use on every call;
// no operation follows them between calls or through a shared reference.
// Mutable syscall access requires the socket's exclusive &mut borrow.
unsafe impl Send for SendHeaders {}
// SAFETY: shared access cannot mutate or dereference any syscall pointers.
unsafe impl Sync for SendHeaders {}

impl Socket {
    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
    pub(crate) fn capabilities(&self) -> Capabilities {
        self.caps
    }
    pub(crate) async fn readable(&self) -> io::Result<()> {
        self.socket.readable().await.map(|_| ())
    }
    pub(crate) fn poll_readable(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.socket
            .poll_read_ready(cx)
            .map(|result| result.map(|_| ()))
    }
    pub(crate) fn poll_writable(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.socket
            .poll_write_ready(cx)
            .map(|result| result.map(|_| ()))
    }

    pub(crate) async fn writable(&self) -> io::Result<()> {
        self.socket.writable().await.map(|_| ())
    }

    pub(crate) fn detach(self) -> Detached {
        Detached {
            socket: self.socket.into_inner(),
            local_addr: self.local_addr,
            caps: self.caps,
            recv_cmsg: self.recv_cmsg,
            send_headers: self.send_headers,
            gso_headers: self.gso_headers,
        }
    }

    pub(crate) fn recv(
        &mut self,
        arena: &mut ReceiveArena,
        max_datagram: usize,
        counters: &mut Counters,
    ) -> io::Result<()> {
        let cmsg = &mut self.recv_cmsg;
        let local_addr = self.local_addr;
        let caps = self.caps;
        self.socket.try_io(tokio::io::Interest::READABLE, |socket| {
            if caps.max_batch_messages == 1 {
                let bytes = arena.bytes.get_mut(..arena.slot_size).unwrap_or_default();
                let mut iovs = [IoSliceMut::new(bytes)];
                let message = recvmsg(
                    socket.as_raw_fd(),
                    &mut iovs,
                    Some(&mut *cmsg),
                    MsgFlags::MSG_DONTWAIT,
                )
                .map_err(io::Error::from)?;
                if let Some(slot) = arena.entries.first_mut() {
                    *slot = parse_received(
                        message,
                        local_addr,
                        false,
                        max_datagram,
                        counters,
                        ReceiveTime::now(),
                    );
                }
                return Ok(());
            }
            // nix 0.31 does not restore msg_controllen/msg_namelen after a
            // receive. Reusing MultiHeaders can truncate the next packet's
            // ancillary metadata (notably v4 followed by v6). Keep payload
            // storage reusable, but rebuild this bounded safe-API scratch.
            let mut headers = MultiHeaders::<SockaddrStorage>::preallocate(
                self.caps.max_batch_messages,
                Some(cmsg_space!(
                    i32,
                    libc::in_pktinfo,
                    libc::in6_pktinfo,
                    u8,
                    i32
                )),
            );
            let mut slots = arena.bytes.chunks_exact_mut(arena.slot_size);
            let mut iovs: [[IoSliceMut<'_>; 1]; BATCH_SIZE] =
                std::array::from_fn(|_| [IoSliceMut::new(slots.next().unwrap_or_default())]);
            let received = recvmmsg(
                socket.as_raw_fd(),
                &mut headers,
                iovs.iter_mut().take(self.caps.max_batch_messages),
                MsgFlags::MSG_DONTWAIT,
                None,
            )
            .map_err(io::Error::from)?;
            let received_at = ReceiveTime::now();
            for (index, message) in received.enumerate() {
                if let Some(slot) = arena.entries.get_mut(index) {
                    *slot = parse_received(
                        message,
                        local_addr,
                        caps.gro,
                        max_datagram,
                        counters,
                        received_at,
                    );
                }
            }
            Ok(())
        })
    }

    pub(crate) fn send_ect0(&mut self, entry: &SendDatagram<'_>) -> SendReport {
        let destination = match (self.local_addr, entry.destination) {
            (SocketAddr::V6(_), SocketAddr::V4(addr)) => {
                SocketAddr::new(IpAddr::V6(addr.ip().to_ipv6_mapped()), addr.port())
            }
            (_, addr) => addr,
        };
        let address = SockaddrStorage::from(destination);
        let cmsg = if entry.destination.is_ipv4() {
            ControlMessage::Ipv4Tos(&2)
        } else {
            ControlMessage::Ipv6TClass(&2)
        };
        submit_prefix(std::slice::from_ref(entry), 1, |_| {
            let transmit = |socket: &socket2::Socket| {
                sendmsg(
                    socket.as_raw_fd(),
                    &[IoSlice::new(entry.payload)],
                    std::slice::from_ref(&cmsg),
                    MsgFlags::MSG_DONTWAIT,
                    Some(&address),
                )
                .map_err(io::Error::from)
                .and_then(|bytes| {
                    if bytes == entry.payload.len() {
                        Ok(1)
                    } else {
                        Err(io::ErrorKind::WriteZero.into())
                    }
                })
            };
            let mut attempted = false;
            let result = self.socket.try_io(tokio::io::Interest::WRITABLE, |socket| {
                attempted = true;
                transmit(socket)
            });
            if attempted {
                result
            } else {
                transmit(self.socket.get_ref())
            }
        })
    }

    pub(crate) fn send(&mut self, entries: &[SendDatagram<'_>]) -> SendReport {
        let max_batch = self.caps.max_batch_messages;
        let ordinary_headers = &mut self.send_headers;
        let gso_headers = &mut self.gso_headers;
        submit_prefix(entries, max_batch, |group| {
            let group = group.get(..group.len().min(max_batch)).unwrap_or_default();
            let mut iovs: [[IoSlice<'_>; 1]; BATCH_SIZE] =
                std::array::from_fn(|_| [IoSlice::new(&[])]);
            let mut addresses: [Option<SockaddrStorage>; BATCH_SIZE] =
                std::array::from_fn(|_| None);
            for ((iov, addr), entry) in iovs.iter_mut().zip(addresses.iter_mut()).zip(group) {
                *iov = [IoSlice::new(entry.payload)];
                let destination = match (self.local_addr, entry.destination) {
                    (SocketAddr::V6(_), SocketAddr::V4(addr)) => {
                        SocketAddr::new(IpAddr::V6(addr.ip().to_ipv6_mapped()), addr.port())
                    }
                    (_, addr) => addr,
                };
                *addr = Some(SockaddrStorage::from(destination));
            }
            let segment = group
                .first()
                .and_then(|entry| entry.gso_segment_size)
                .map(NonZeroU16::get);
            let cmsg = segment.as_ref().map(ControlMessage::UdpGsoSegments);
            let mut transmit = |socket: &socket2::Socket| {
                // nix retains cmsg storage between calls. Separate ordinary
                // headers prevent a previous GSO cmsg bleeding into a send.
                let headers = if segment.is_some() {
                    &mut *gso_headers
                } else {
                    &mut *ordinary_headers
                };
                sendmmsg(
                    socket.as_raw_fd(),
                    &mut headers.0,
                    iovs.get(..group.len()).unwrap_or_default(),
                    addresses.get(..group.len()).unwrap_or_default(),
                    cmsg.as_slice(),
                    MsgFlags::MSG_DONTWAIT,
                )
                .map(Iterator::count)
                .map_err(|error| match error {
                    Errno::EAGAIN => io::Error::from(io::ErrorKind::WouldBlock),
                    other => io::Error::from(other),
                })
            };
            let mut attempted = false;
            let result = self.socket.try_io(tokio::io::Interest::WRITABLE, |socket| {
                attempted = true;
                transmit(socket)
            });
            if attempted {
                result
            } else {
                // A new reactor has no cached readiness yet. Probe once instead
                // of turning that cache miss into a blocked submission.
                transmit(self.socket.get_ref())
            }
        })
    }
}

fn parse_received(
    message: nix::sys::socket::RecvMsg<'_, '_, SockaddrStorage>,
    local_addr: SocketAddr,
    gro: bool,
    max_datagram: usize,
    counters: &mut Counters,
    received_at: ReceiveTime,
) -> Option<Received> {
    if message.flags.intersects(MsgFlags::MSG_TRUNC) {
        counters.truncated_drops = counters.truncated_drops.saturating_add(1);
        return None;
    }
    let source = message.address.as_ref().and_then(sockaddr);
    let metadata = message.cmsgs().ok().and_then(|cmsgs| {
        let mut destination = None;
        let mut stride = None;
        let mut ecn = None;
        let mut malformed = message.flags.contains(MsgFlags::MSG_CTRUNC);
        for cmsg in cmsgs {
            match cmsg {
                ControlMessageOwned::Ipv4PacketInfo(info) => {
                    destination = Some(SocketAddr::new(
                        IpAddr::V4(Ipv4Addr::from(info.ipi_addr.s_addr.to_ne_bytes())),
                        local_addr.port(),
                    ));
                }
                ControlMessageOwned::Ipv6PacketInfo(info) => {
                    let ip = Ipv6Addr::from(info.ipi6_addr.s6_addr);
                    let scope = if ip.is_unicast_link_local() {
                        info.ipi6_ifindex
                    } else {
                        0
                    };
                    destination = Some(normalize(SocketAddr::V6(std::net::SocketAddrV6::new(
                        ip,
                        local_addr.port(),
                        0,
                        scope,
                    ))));
                }
                ControlMessageOwned::Ipv4Tos(tos) => {
                    ecn = Some(Ecn::from_traffic_class(tos));
                }
                ControlMessageOwned::Ipv6TClass(class) => {
                    ecn = u8::try_from(class).ok().map(Ecn::from_traffic_class);
                }
                ControlMessageOwned::UdpGroSegments(segment) => {
                    if !gro || segment <= 0 || stride.is_some() {
                        malformed = true;
                    }
                    stride = usize::try_from(segment).ok();
                }
                _ => {}
            }
        }
        decode_metadata(
            local_addr,
            destination,
            message.bytes,
            stride,
            max_datagram,
            malformed,
        )
        .map(|(destination, stride)| (destination, stride, ecn))
    });
    let Some((source, (destination, stride, ecn))) = source.zip(metadata) else {
        counters.invalid_metadata_drops = counters.invalid_metadata_drops.saturating_add(1);
        return None;
    };

    Some(Received {
        source,
        destination,
        stride,
        len: message.bytes,
        received_at,
        ecn,
    })
}

fn normalize(address: SocketAddr) -> SocketAddr {
    match address {
        SocketAddr::V6(addr) => addr
            .ip()
            .to_ipv4_mapped()
            .map(|ip| SocketAddr::new(IpAddr::V4(ip), addr.port()))
            .unwrap_or(address),
        _ => address,
    }
}

fn sockaddr(address: &SockaddrStorage) -> Option<SocketAddr> {
    if let Some(addr) = address.as_sockaddr_in() {
        Some(SocketAddr::new(IpAddr::V4(addr.ip()), addr.port()))
    } else {
        address.as_sockaddr_in6().map(|addr| {
            normalize(SocketAddr::V6(std::net::SocketAddrV6::new(
                addr.ip(),
                addr.port(),
                addr.flowinfo(),
                addr.scope_id(),
            )))
        })
    }
}

fn decode_metadata(
    bound: SocketAddr,
    destination: Option<SocketAddr>,
    len: usize,
    gro_stride: Option<usize>,
    max_datagram: usize,
    malformed: bool,
) -> Option<(SocketAddr, usize)> {
    if malformed {
        return None;
    }
    let destination =
        destination.or_else(|| (!bound.ip().is_unspecified()).then_some(normalize(bound)))?;
    if destination.ip().is_unspecified() {
        return None;
    }
    let stride = gro_stride.unwrap_or_else(|| len.max(1));
    if stride == 0
        || stride > max_datagram
        || (gro_stride.is_some() && (stride > len || len.div_ceil(stride) > MAX_GSO_SEGMENTS))
    {
        return None;
    }
    Some((destination, stride))
}

#[cfg(test)]
mod metadata_tests {
    use super::*;
    #[test]
    fn wildcard_omission_malformed_gro_and_oversize_never_fabricate_packets() {
        let wildcard = "0.0.0.0:1234".parse().unwrap();
        let specific = "127.0.0.1:1234".parse().unwrap();
        assert!(decode_metadata(wildcard, None, 3, None, 4, false).is_none());
        assert_eq!(
            decode_metadata(specific, None, 3, None, 4, false),
            Some((specific, 3))
        );
        assert_eq!(
            decode_metadata(wildcard, Some(specific), 7, Some(3), 4, false),
            Some((specific, 3))
        );
        for (len, stride, malformed) in [
            (3, Some(0), false),
            (3, Some(4), false),
            (65, Some(1), false),
            (5, None, false),
            (3, None, true),
        ] {
            assert!(decode_metadata(specific, Some(specific), len, stride, 4, malformed).is_none());
        }
        assert_eq!(
            decode_metadata(specific, None, 0, None, 4, false),
            Some((specific, 1))
        );
    }
}
