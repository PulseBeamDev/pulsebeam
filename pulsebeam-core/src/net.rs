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

        pub async fn accept(&self) -> io::Result<(TurmoilStream, SocketAddr)> {
            self.0.accept().await
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
