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
    use std::pin::Pin;
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use turmoil::net::TcpListener as InnerListener;

    /// A thin wrapper around `turmoil::net::TcpStream` that adds the non-blocking
    /// `try_read`, `try_write`, and `readable` methods that the production code
    /// relies on from `tokio::net::TcpStream`.
    ///
    /// - `try_read` is implemented via `poll_read` with a noop waker (returns
    ///   `WouldBlock` when no data is immediately available).
    /// - `try_write` delegates to turmoil's built-in `try_write`.
    /// - `readable` is implemented via `poll_peek` (which does NOT consume data)
    ///   so the actual waker is registered and the caller is notified when data
    ///   arrives.
    pub struct TurmoilStream(pub(super) turmoil::net::TcpStream);

    impl std::fmt::Debug for TurmoilStream {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_tuple("TurmoilStream")
                .field(&self.0.local_addr())
                .finish()
        }
    }

    fn noop_waker() -> Waker {
        const VTABLE: RawWakerVTable = RawWakerVTable::new(
            |_| RawWaker::new(std::ptr::null(), &VTABLE),
            |_| {},
            |_| {},
            |_| {},
        );
        unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
    }

    impl TurmoilStream {
        pub async fn connect(addr: SocketAddr) -> io::Result<Self> {
            turmoil::net::TcpStream::connect(addr).await.map(Self)
        }

        pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
            self.0.set_nodelay(nodelay)
        }

        pub fn local_addr(&self) -> io::Result<SocketAddr> {
            self.0.local_addr()
        }

        pub fn peer_addr(&self) -> io::Result<SocketAddr> {
            self.0.peer_addr()
        }

        /// Non-blocking read: returns `WouldBlock` if no data is currently available.
        pub fn try_read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            let mut read_buf = ReadBuf::new(buf);
            match Pin::new(&mut self.0).poll_read(&mut cx, &mut read_buf) {
                Poll::Ready(Ok(())) => Ok(read_buf.filled().len()),
                Poll::Pending => Err(io::Error::from(io::ErrorKind::WouldBlock)),
                Poll::Ready(Err(e)) => Err(e),
            }
        }

        /// Non-blocking write: delegates to turmoil's built-in `try_write`.
        pub fn try_write(&self, buf: &[u8]) -> io::Result<usize> {
            self.0.try_write(buf)
        }

        /// Async readiness check: resolves when at least one byte is available to
        /// read without consuming it (uses turmoil's `poll_peek`).
        pub async fn readable(&mut self) -> io::Result<()> {
            let mut peek_byte = [0u8; 1];
            let stream = &mut self.0;
            futures::future::poll_fn(move |cx| {
                let mut read_buf = ReadBuf::new(&mut peek_byte);
                Pin::new(&mut *stream)
                    .poll_peek(cx, &mut read_buf)
                    .map_ok(|_| ())
            })
            .await
        }
    }

    impl AsyncRead for TurmoilStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for TurmoilStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.0).poll_write(cx, buf)
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_flush(cx)
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_shutdown(cx)
        }
    }

    pub struct TurmoilListener(InnerListener);

    impl TurmoilListener {
        pub async fn bind(addr: SocketAddr) -> io::Result<Self> {
            let inner = InnerListener::bind(addr).await?;
            Ok(Self(inner))
        }

        pub async fn accept(&self) -> io::Result<(TurmoilStream, SocketAddr)> {
            self.0.accept().await.map(|(s, addr)| (TurmoilStream(s), addr))
        }

        pub fn local_addr(&self) -> io::Result<SocketAddr> {
            self.0.local_addr()
        }
    }

    impl Listener for TurmoilListener {
        type Io = TurmoilStream;
        type Addr = SocketAddr;

        async fn accept(&mut self) -> (Self::Io, Self::Addr) {
            let (s, addr) = self.0.accept().await.unwrap();
            (TurmoilStream(s), addr)
        }

        fn local_addr(&self) -> io::Result<Self::Addr> {
            self.0.local_addr()
        }
    }
}

pub type HttpRequest = http::Request<Vec<u8>>;
pub type HttpResponse = Result<http::Response<Vec<u8>>, HttpError>;
pub type HttpError = Box<dyn std::error::Error + Send + Sync>;
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type HttpResult<'a> = BoxFuture<'a, HttpResponse>;

pub trait AsyncHttpClient: Send + Sync {
    fn execute(&self, req: HttpRequest) -> HttpResult<'_>;
}

#[cfg(feature = "reqwest")]
impl AsyncHttpClient for reqwest::Client {
    fn execute(&self, req: HttpRequest) -> HttpResult<'_> {
        let client = self.clone();
        Box::pin(async move {
            let reqwest_req = req.try_into().map_err(|e| Box::new(e) as HttpError)?;

            let res = client
                .execute(reqwest_req)
                .await
                .map_err(|e| Box::new(e) as HttpError)?;

            let status = res.status();
            let headers = res.headers().clone();
            let body_bytes = res.bytes().await.map_err(|e| Box::new(e) as HttpError)?;

            let mut builder = http::Response::builder().status(status);

            if let Some(h) = builder.headers_mut() {
                *h = headers;
            }

            builder
                .body(body_bytes.to_vec())
                .map_err(|e| Box::new(e) as HttpError)
        })
    }
}
