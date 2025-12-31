#[cfg(not(feature = "sim"))]
pub use tokio::net::UdpSocket;
#[cfg(feature = "sim")]
pub use turmoil::net::UdpSocket;

#[cfg(not(feature = "sim"))]
pub use tokio::net::{TcpListener, TcpStream};

#[cfg(feature = "sim")]
pub use sim::{TurmoilListener as TcpListener, TurmoilStream as TcpStream};

#[cfg(feature = "sim")]
mod sim {
    use axum::serve::Listener;
    use std::io;
    use std::net::SocketAddr;
    use turmoil::net::TcpListener as InnerListener;

    pub type TurmoilStream = turmoil::net::TcpStream;
    pub struct TurmoilListener(InnerListener);

    impl TurmoilListener {
        pub async fn bind(addr: SocketAddr) -> io::Result<Self> {
            let inner = InnerListener::bind(addr).await?;
            Ok(Self(inner))
        }

        pub fn local_addr(&self) -> io::Result<SocketAddr> {
            self.0.local_addr()
        }
    }

    impl Listener for TurmoilListener {
        type Io = TurmoilStream;
        type Addr = SocketAddr;

        async fn accept(&mut self) -> (Self::Io, Self::Addr) {
            self.0.accept().await.unwrap()
        }

        fn local_addr(&self) -> io::Result<Self::Addr> {
            self.0.local_addr()
        }
    }
}
