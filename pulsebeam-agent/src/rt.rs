use bytes::Bytes;
use futures::Future;
use http::{Request, Response, StatusCode};
use std::io::{IoSliceMut, Result as IoResult};
use std::mem::MaybeUninit;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;

// --- Statically-Defined Error Types ---

/// Represents an error that occurred during an HTTP request.
#[derive(Error, Debug)]
pub enum HttpError {
    #[error("URL parsing error: {0}")]
    Url(String),
    #[error("DNS resolution failed or connection refused")]
    Connect,
    #[error("Connection timed out")]
    Timeout,
    #[error("Error sending request body")]
    Body,
    #[error("Exceeded maximum number of redirects")]
    Redirect,
    #[error("HTTP protocol error: {0}")]
    Protocol(String),
    #[error("A client-side error occurred: {0}")]
    Request(String),
    #[error("Server returned an error status code: {0}")]
    Status(StatusCode),
    #[error("A transport-layer security (TLS) error occurred: {0}")]
    Tls(String),
}

/// Represents an error from awaiting a spawned task's completion.
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
#[error("task panicked")]
pub struct JoinError;

// --- Abstraction Traits ---

/// A handle that allows awaiting the result of a spawned task.
pub trait JoinHandle<T>: Future<Output = Result<T, JoinError>> + Send {}

/// A trait for an asynchronous, zero-copy-friendly UDP socket.
pub trait UdpSocket: Send + Sync + 'static {
    /// Sends a batch of UDP packets to their respective destinations.
    /// Returns the number of packets successfully sent.
    fn send_batch<'a, D>(
        &'a self,
        packets: &'a [(D, SocketAddr)],
    ) -> impl Future<Output = IoResult<usize>> + Send + 'a
    where
        D: AsRef<[u8]> + Send + Sync;

    /// Receives a batch of UDP packets, filling the provided buffers.
    /// Returns the number of packets successfully received.
    fn receive_batch<'a>(
        &'a self,
        data_bufs: &'a mut [IoSliceMut<'a>],
        addr_bufs: &'a mut [MaybeUninit<SocketAddr>],
    ) -> impl Future<Output = IoResult<usize>> + Send + 'a;
}

/// A trait for a high-performance asynchronous HTTP client.
pub trait HttpClient: Send + Sync + 'static + Clone {
    /// Sends an HTTP request and returns the response.
    fn send(
        &self,
        request: Request<Bytes>,
    ) -> impl Future<Output = Result<Response<Bytes>, HttpError>> + Send;
}

/// The core lightweight runtime trait.
pub trait Runtime: Send + Sync + 'static + Clone {
    type JoinHandle<T: Send + 'static>: JoinHandle<T> + 'static;
    type UdpSocket: UdpSocket;
    type HttpClient: HttpClient;

    /// Spawns a new asynchronous task.
    fn spawn<F>(&self, future: F) -> Self::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static;

    /// Creates a new UDP socket bound to the given address.
    fn bind_udp_socket(
        &self,
        addr: SocketAddr,
    ) -> impl Future<Output = IoResult<Arc<Self::UdpSocket>>> + Send;

    /// Returns a handle to the runtime's HTTP client.
    fn http_client(&self) -> Self::HttpClient;

    /// Creates a future that completes after the specified duration.
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send;

    /// Returns the current time as an `Instant`.
    fn now(&self) -> Instant;
}

// --- Tokio Runtime Implementation (Feature-Gated) ---

/// This module is only compiled when the `tokio` feature is enabled.
#[cfg(feature = "tokio")]
pub mod tokio_runtime {
    use super::*; // Import traits and errors from parent module
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::task::JoinHandle as TokioJoinHandle;

    // --- JoinHandle Implementation ---
    #[derive(Debug)]
    pub struct TokioJoinHandleWrapper<T>(TokioJoinHandle<T>);

    impl<T: Send> Future for TokioJoinHandleWrapper<T> {
        type Output = Result<T, JoinError>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            Pin::new(&mut self.0).poll(cx).map_err(|_| JoinError)
        }
    }

    impl<T: Send + 'static> super::JoinHandle<T> for TokioJoinHandleWrapper<T> {}

    // --- UdpSocket Implementation ---
    pub struct TokioUdpSocket(tokio::net::UdpSocket);

    impl super::UdpSocket for TokioUdpSocket {
        async fn send_batch<'a, D>(&'a self, packets: &'a [(D, SocketAddr)]) -> IoResult<usize>
        where
            D: AsRef<[u8]> + Send + Sync,
        {
            // Note: Tokio's UdpSocket::send_to is not a true batch operation at the syscall level.
            // For true batching, `sendmmsg` would be needed, which is not yet stable in Tokio.
            // This implementation iterates, which is functionally correct.
            let mut packets_sent = 0;
            for (data, addr) in packets {
                self.0.send_to(data.as_ref(), *addr).await?;
                packets_sent += 1;
            }
            Ok(packets_sent)
        }

        async fn receive_batch<'a>(
            &'a self,
            data_bufs: &'a mut [IoSliceMut<'a>],
            addr_bufs: &'a mut [MaybeUninit<SocketAddr>],
        ) -> IoResult<usize> {
            let mut packets_received = 0;
            if data_bufs.is_empty() {
                return Ok(0);
            }

            // A single call to `recv_from` will do for this implementation.
            // True `recvmmsg` would be more efficient but is not available in Tokio yet.
            match self.0.recv_from(&mut data_bufs[0]).await {
                Ok((len, addr)) => {
                    // Update the length of the slice that was written to.
                    // This is safe because `recv_from` guarantees it has written `len` bytes.
                    let data_slice = &mut data_bufs[0];
                    let new_slice =
                        unsafe { std::slice::from_raw_parts_mut(data_slice.as_mut_ptr(), len) };
                    *data_slice = IoSliceMut::new(new_slice);
                    addr_bufs[0].write(addr);
                    packets_received += 1;
                }
                Err(e) => return Err(e),
            }
            Ok(packets_received)
        }
    }

    // --- HttpClient Implementation (using reqwest) ---
    #[derive(Clone)]
    pub struct TokioHttpClient(reqwest::Client);

    // Helper to map reqwest errors to our static HttpError enum
    fn map_reqwest_error(e: reqwest::Error) -> HttpError {
        if e.is_connect() {
            HttpError::Connect
        } else if e.is_timeout() {
            HttpError::Timeout
        } else if e.is_redirect() {
            HttpError::Redirect
        } else if e.is_body() {
            HttpError::Body
        } else if e.is_request() {
            HttpError::Request(e.to_string())
        } else if let Some(status) = e.status() {
            HttpError::Status(status)
        } else if e.is_tls() {
            HttpError::Tls(e.to_string())
        } else if let Some(url_err) = e.url() {
            HttpError::Url(url_err.to_string())
        } else {
            HttpError::Protocol(e.to_string())
        }
    }

    impl super::HttpClient for TokioHttpClient {
        async fn send(&self, req: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
            let reqwest_req = req
                .try_into()
                .map_err(|e| HttpError::Request(format!("{}", e)))?;

            let response = self
                .0
                .execute(reqwest_req)
                .await
                .map_err(map_reqwest_error)?;

            let mut builder = Response::builder()
                .status(response.status())
                .version(response.version());

            // Copy headers from reqwest::Response to http::Response
            for (key, value) in response.headers().iter() {
                builder = builder.header(key, value);
            }

            let body = response.bytes().await.map_err(map_reqwest_error)?;

            builder
                .body(body)
                .map_err(|e| HttpError::Protocol(e.to_string()))
        }
    }

    // --- Runtime Implementation ---
    #[derive(Clone)]
    pub struct TokioRuntime {
        http_client: TokioHttpClient,
    }

    impl TokioRuntime {
        pub fn new() -> Self {
            Self {
                http_client: TokioHttpClient(reqwest::Client::new()),
            }
        }
    }

    impl Default for TokioRuntime {
        fn default() -> Self {
            Self::new()
        }
    }

    impl super::Runtime for TokioRuntime {
        type JoinHandle<T: Send + 'static> = TokioJoinHandleWrapper<T>;
        type UdpSocket = TokioUdpSocket;
        type HttpClient = TokioHttpClient;

        fn spawn<F>(&self, future: F) -> Self::JoinHandle<F::Output>
        where
            F: Future + Send + 'static,
            F::Output: Send + 'static,
        {
            TokioJoinHandleWrapper(tokio::spawn(future))
        }

        async fn bind_udp_socket(&self, addr: SocketAddr) -> IoResult<Arc<Self::UdpSocket>> {
            let socket = tokio::net::UdpSocket::bind(addr).await?;
            Ok(Arc::new(TokioUdpSocket(socket)))
        }

        fn http_client(&self) -> Self::HttpClient {
            self.http_client.clone()
        }

        async fn sleep(&self, duration: Duration) {
            tokio::time::sleep(duration).await;
        }

        fn now(&self) -> Instant {
            // Note: Tokio's time driver might have a different Instant source.
            // For simplicity and common use cases, `Instant::now()` is sufficient.
            Instant::now()
        }
    }
}
