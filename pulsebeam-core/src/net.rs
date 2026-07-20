#[cfg(not(feature = "sim"))]
pub use tokio::net::UdpSocket;
#[cfg(feature = "sim")]
pub use turmoil::net::UdpSocket;

#[cfg(not(feature = "sim"))]
pub use tokio::net::{TcpListener, TcpStream};

#[cfg(feature = "sim")]
pub use sim::{TurmoilListener as TcpListener, TurmoilStream as TcpStream};

#[cfg(not(feature = "sim"))]
pub use tokio::net::tcp::{OwnedReadHalf as TcpReadHalf, OwnedWriteHalf as TcpWriteHalf};

#[cfg(not(feature = "sim"))]
pub fn split_tcp(stream: TcpStream) -> (TcpReadHalf, TcpWriteHalf) {
    stream.into_split()
}

#[cfg(feature = "sim")]
pub use sim::{TurmoilReadHalf as TcpReadHalf, TurmoilWriteHalf as TcpWriteHalf};

#[cfg(feature = "sim")]
pub fn split_tcp(stream: TcpStream) -> (TcpReadHalf, TcpWriteHalf) {
    stream.into_split()
}

#[cfg(feature = "sim")]
mod sim {
    use axum::serve::Listener;
    use std::io;
    use std::net::SocketAddr;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
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

        /// Consume the stream and return separate owned read and write halves.
        ///
        /// Both halves share the underlying socket via an `Arc<Mutex<...>>`.  The
        /// mutex is never contended in the single-threaded turmoil simulator.
        pub fn into_split(self) -> (TurmoilReadHalf, TurmoilWriteHalf) {
            let inner = Arc::new(Mutex::new(self.0));
            (
                TurmoilReadHalf {
                    inner: Arc::clone(&inner),
                },
                TurmoilWriteHalf { inner },
            )
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
    // ── Owned split halves ────────────────────────────────────────────────────

    /// Read half of a split `TurmoilStream`.  Both halves share the underlying
    /// socket via `Arc<Mutex<...>>`; the mutex is never contended in the
    /// single-threaded turmoil simulator.
    pub struct TurmoilReadHalf {
        pub(super) inner: Arc<Mutex<turmoil::net::TcpStream>>,
    }

    impl TurmoilReadHalf {
        /// Returns `WouldBlock` when no data is immediately available.
        pub fn try_read(&self, buf: &mut [u8]) -> io::Result<usize> {
            let mut guard = self
                .inner
                .try_lock()
                .expect("TurmoilReadHalf: mutex contended in single-threaded sim");
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            let mut read_buf = ReadBuf::new(buf);
            match Pin::new(&mut *guard).poll_read(&mut cx, &mut read_buf) {
                Poll::Ready(Ok(())) => Ok(read_buf.filled().len()),
                Poll::Pending => Err(io::Error::from(io::ErrorKind::WouldBlock)),
                Poll::Ready(Err(e)) => Err(e),
            }
        }

        /// Resolves when at least one byte is readable without consuming data.
        pub async fn readable(&self) -> io::Result<()> {
            let inner = Arc::clone(&self.inner);
            let mut peek_buf = [0u8; 1];
            futures::future::poll_fn(move |cx| {
                let mut guard = inner
                    .try_lock()
                    .expect("TurmoilReadHalf: mutex contended in single-threaded sim");
                let result = {
                    let mut rb = ReadBuf::new(&mut peek_buf);
                    Pin::new(&mut *guard).poll_peek(cx, &mut rb).map_ok(|_| ())
                };
                drop(guard); // release before returning Pending
                result
            })
            .await
        }
    }

    /// Write half of a split `TurmoilStream`.
    pub struct TurmoilWriteHalf {
        pub(super) inner: Arc<Mutex<turmoil::net::TcpStream>>,
    }

    impl TurmoilWriteHalf {
        /// Non-blocking write: returns `WouldBlock` if the kernel buffer is full.
        pub fn try_write(&self, buf: &[u8]) -> io::Result<usize> {
            let guard = self
                .inner
                .try_lock()
                .expect("TurmoilWriteHalf: mutex contended in single-threaded sim");
            (*guard).try_write(buf)
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

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
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
            self.0
                .accept()
                .await
                .map(|(s, addr)| (TurmoilStream(s), addr))
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

/// Test-only bandwidth shaper for the turmoil-backed simulator.
///
/// Turmoil's own knobs (latency, jitter, `fail_rate`) never constrain
/// throughput, so this polices egress bytes against a settable rate:
/// packets within budget are sent, over-budget packets are dropped. Lives
/// in `pulsebeam-core` (shared by `pulsebeam-runtime` and the agent) so
/// both the SFU's egress path and the agent's UDP send path can use it.
/// Only exists under the `sim` feature.
///
/// Two registries, keyed differently on purpose:
/// - [`set_downlink_bandwidth`]/[`admit`] key by *destination* IP (only the
///   SFU sends to a given receiver, so destination alone is unambiguous).
/// - [`set_uplink_bandwidth`]/[`admit_uplink`] key by *source* IP instead —
///   a sender and receiver in the same test both address the SFU, so a
///   destination-keyed bucket would let a sender's media burst starve an
///   unrelated receiver's ICE/RTCP traffic to that same SFU.
#[cfg(feature = "sim")]
pub mod shaper {
    use std::collections::HashMap;
    use std::net::IpAddr;
    use std::sync::{Mutex, OnceLock};
    // Not `std::time::Instant`: under turmoil this must track the
    // simulated clock (which turmoil fast-forwards independent of real
    // wall-clock time), or the bucket's elapsed-time math desyncs
    // completely from the simulation.
    use tokio::time::Instant;

    struct Bucket {
        bytes_per_sec: f64,
        burst_bytes: f64,
        available_bytes: f64,
        last_refill: Instant,
    }

    impl Bucket {
        fn new(bytes_per_sec: f64, now: Instant) -> Self {
            let burst_bytes = Self::burst_for_rate(bytes_per_sec);
            Self {
                bytes_per_sec,
                burst_bytes,
                available_bytes: burst_bytes,
                last_refill: now,
            }
        }

        /// A small, rate-relative burst allowance (50ms worth of the configured
        /// rate) so a single oversized packet isn't perpetually starved, while
        /// staying far too small to mask a real capacity change from BWE.
        fn burst_for_rate(bytes_per_sec: f64) -> f64 {
            (bytes_per_sec * 0.05).max(8_000.0)
        }

        fn set_rate(&mut self, bytes_per_sec: f64, now: Instant) {
            self.refill(now);
            self.bytes_per_sec = bytes_per_sec;
            self.burst_bytes = Self::burst_for_rate(bytes_per_sec);
            self.available_bytes = self.available_bytes.min(self.burst_bytes);
        }

        fn refill(&mut self, now: Instant) {
            let elapsed = now.saturating_duration_since(self.last_refill).as_secs_f64();
            self.last_refill = now;
            self.available_bytes = (self.available_bytes + elapsed * self.bytes_per_sec)
                .min(self.burst_bytes);
        }

        fn admit(&mut self, len: usize, now: Instant) -> bool {
            self.refill(now);
            if self.available_bytes >= len as f64 {
                self.available_bytes -= len as f64;
                true
            } else {
                false
            }
        }
    }

    fn buckets() -> &'static Mutex<HashMap<IpAddr, Bucket>> {
        static BUCKETS: OnceLock<Mutex<HashMap<IpAddr, Bucket>>> = OnceLock::new();
        BUCKETS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn uplink_buckets() -> &'static Mutex<HashMap<IpAddr, Bucket>> {
        static BUCKETS: OnceLock<Mutex<HashMap<IpAddr, Bucket>>> = OnceLock::new();
        BUCKETS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn set_bandwidth(registry: &Mutex<HashMap<IpAddr, Bucket>>, ip: IpAddr, bytes_per_sec: Option<u64>) {
        let now = Instant::now();
        let mut map = registry.lock().unwrap();
        match bytes_per_sec {
            None => {
                map.remove(&ip);
            }
            Some(rate) => {
                map.entry(ip)
                    .and_modify(|b| b.set_rate(rate as f64, now))
                    .or_insert_with(|| Bucket::new(rate as f64, now));
            }
        }
    }

    /// Set (or clear, with `None`) the enforced byte rate for egress packets
    /// destined to `ip`. Takes effect immediately: on a step down, packets
    /// sent above the new rate are dropped from the very next call; on a
    /// step up, the bucket simply refills faster.
    pub fn set_downlink_bandwidth(ip: IpAddr, bytes_per_sec: Option<u64>) {
        set_bandwidth(buckets(), ip, bytes_per_sec);
    }

    /// Set (or clear, with `None`) the enforced byte rate for egress packets
    /// *sent from* `ip`, regardless of destination. See the module doc for
    /// why this is a separate, source-keyed registry rather than reusing
    /// [`set_downlink_bandwidth`].
    pub fn set_uplink_bandwidth(ip: IpAddr, bytes_per_sec: Option<u64>) {
        set_bandwidth(uplink_buckets(), ip, bytes_per_sec);
    }

    /// Remove every configured limit, in both registries. Simulator tests
    /// reuse a per-process registry across independently-subnetted test
    /// cases; call this at the start of a scenario that shapes bandwidth to
    /// avoid depending on leftover state from an earlier test.
    pub fn clear_all() {
        buckets().lock().unwrap().clear();
        uplink_buckets().lock().unwrap().clear();
    }

    /// Returns `true` if a packet of `len` bytes to `dst` may be sent now.
    /// Destinations with no configured limit are always admitted.
    pub fn admit(dst: std::net::SocketAddr, len: usize) -> bool {
        let mut map = buckets().lock().unwrap();
        let Some(bucket) = map.get_mut(&dst.ip()) else {
            return true;
        };
        bucket.admit(len, Instant::now())
    }

    /// Returns `true` if a packet of `len` bytes sent from `src` may be sent
    /// now. Sources with no configured limit are always admitted.
    pub fn admit_uplink(src: IpAddr, len: usize) -> bool {
        let mut map = uplink_buckets().lock().unwrap();
        let Some(bucket) = map.get_mut(&src) else {
            return true;
        };
        bucket.admit(len, Instant::now())
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
