//! TCP sockets, framed connections, and optional HTTP serving at the runtime boundary.

#[cfg(feature = "http")]
use std::future::Future;
use std::{
    io,
    net::{SocketAddr, SocketAddrV6},
};

#[cfg(not(feature = "sim"))]
pub use tokio::net::{TcpListener, TcpStream};
#[cfg(feature = "sim")]
pub use turmoil::net::TcpStream;

#[cfg(feature = "sim")]
pub struct TcpListener(turmoil::net::TcpListener);

#[cfg(feature = "sim")]
impl From<pulsebeam_core::net::TcpListener> for TcpListener {
    fn from(listener: pulsebeam_core::net::TcpListener) -> Self {
        Self(listener.into_inner())
    }
}

#[cfg(feature = "sim")]
impl TcpListener {
    pub async fn bind(addr: SocketAddr) -> io::Result<Self> {
        turmoil::net::TcpListener::bind(addr).await.map(Self)
    }

    pub async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        self.0.accept().await
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr()
    }
}

#[cfg(all(feature = "sim", feature = "http"))]
impl axum::serve::Listener for TcpListener {
    type Io = TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.0.accept().await {
                Ok(connection) => return connection,
                Err(_) => tokio::task::yield_now().await,
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.0.local_addr()
    }
}

/// Bind a reusable listener, preferring dual-stack IPv6 on native hosts.
pub async fn bind(addr: SocketAddr) -> io::Result<TcpListener> {
    #[cfg(feature = "sim")]
    return TcpListener::bind(addr).await;
    #[cfg(not(feature = "sim"))]
    bind_native(addr, false)
}

/// Bind an IPv6-only listener, including when listening on the wildcard address.
pub async fn bind_ipv6(addr: SocketAddrV6) -> io::Result<TcpListener> {
    if addr.ip().to_ipv4_mapped().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "IPv4-mapped addresses are not valid IPv6 listeners",
        ));
    }
    #[cfg(feature = "sim")]
    return TcpListener::bind(addr.into()).await;
    #[cfg(not(feature = "sim"))]
    bind_native(addr.into(), true)
}

#[cfg(not(feature = "sim"))]
fn bind_native(addr: SocketAddr, ipv6_only: bool) -> io::Result<TcpListener> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    if addr.is_ipv6() {
        socket.set_only_v6(ipv6_only)?;
    }
    socket.set_nonblocking(true)?;
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    TcpListener::from_std(socket.into())
}

/// A framed TCP connection. Framing limits and protocol policy belong to the caller.
pub type Connection = TcpStream;
pub struct Reader(pulsebeam_tcp::FrameReader<tokio::io::ReadHalf<TcpStream>>);
pub struct Writer(pulsebeam_tcp::FrameWriter<tokio::io::WriteHalf<TcpStream>>);

impl Reader {
    pub fn is_partial(&self) -> bool {
        self.0.is_partial()
    }

    pub async fn recv(&mut self) -> io::Result<Option<bytes::Bytes>> {
        self.0.recv().await
    }
}

impl Writer {
    /// A cancelled or failed send may have written a partial frame; retire the connection.
    pub async fn send(&mut self, packet: &[u8]) -> io::Result<()> {
        self.0.send(packet).await
    }
}

pub fn split(connection: Connection, max_frame_bytes: usize) -> io::Result<(Reader, Writer)> {
    let (reader, writer) = pulsebeam_tcp::split(connection, max_frame_bytes)?;
    Ok((Reader(reader), Writer(writer)))
}

/// Serve an SFU-owned router until the supplied shutdown future resolves.
#[cfg(feature = "http")]
pub async fn serve_http(
    listener: TcpListener,
    router: axum::Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
}

#[cfg(all(test, not(feature = "sim")))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ipv6_only_listener_does_not_enable_ipv4_mapped_connections() {
        let listener = bind_ipv6("[::]:0".parse().unwrap()).await.unwrap();
        assert!(socket2::SockRef::from(&listener).only_v6().unwrap());
        let mapped = bind_ipv6("[::ffff:127.0.0.1]:0".parse().unwrap()).await;
        assert_eq!(mapped.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }
}
