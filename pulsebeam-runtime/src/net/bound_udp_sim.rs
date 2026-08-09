use super::{UdpMode, UnifiedSocket, udp_scalar};
use crate::sync::Arc;
use pulsebeam_core::net::UdpSocket;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Weak;
use std::{io, net::SocketAddr};

pub struct BoundUdpSocket {
    socket: udp_scalar::UdpTransport,
    local_addr: SocketAddr,
}

impl BoundUdpSocket {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
    pub fn into_unified_socket(self) -> io::Result<UnifiedSocket> {
        Ok(UnifiedSocket::UdpScalar(self.socket))
    }
}

thread_local! {
    /// Sockets already bound on this host, so a second bind to the same address
    /// joins them instead of failing.
    ///
    /// Keyed by the advertised address as well as the bind address: every host
    /// in a simulation binds `0.0.0.0:3478`, and only the advertised address
    /// tells them apart. `Weak`, so a finished simulation's sockets unbind and
    /// the next one starts clean.
    static REUSEPORT_GROUPS: RefCell<HashMap<(SocketAddr, Option<SocketAddr>), Weak<UdpSocket>>> =
        RefCell::new(HashMap::new());
}

/// Bind a socket, emulating `SO_REUSEPORT`.
///
/// Turmoil allows one bind per address, but a shard-per-core SFU binds the same
/// port once per worker and lets the kernel spread arrivals. Without that, a
/// simulation asking for several workers silently gets one — and a single-shard
/// node never executes a cross-shard path, so every plan that believes it is
/// testing one is testing something else.
///
/// The emulation shares the underlying socket across the group. Each worker
/// polls it, so a datagram goes to whichever is ready first. Real
/// `SO_REUSEPORT` is sticky per 4-tuple where this is not, which makes the
/// simulation the harsher of the two: a session's packets can land on any
/// worker, so the demuxer's misdelivery path runs constantly rather than only
/// when a route changes.
pub async fn bind_udp_socket(
    addr: SocketAddr,
    _mode: UdpMode,
    external_addr: Option<SocketAddr>,
) -> io::Result<BoundUdpSocket> {
    let key = (addr, external_addr);
    let existing =
        REUSEPORT_GROUPS.with(|groups| groups.borrow().get(&key).and_then(|weak| weak.upgrade()));

    let socket = match existing {
        Some(socket) => socket,
        None => {
            let socket = Arc::new(UdpSocket::bind(addr).await?);
            REUSEPORT_GROUPS
                .with(|groups| groups.borrow_mut().insert(key, Arc::downgrade(&socket)));
            socket
        }
    };

    let socket = udp_scalar::from_shared(socket, external_addr)?;
    let local_addr = socket.local_addr();
    Ok(BoundUdpSocket { socket, local_addr })
}
