use axum::serve::Listener;
use http_body_util::BodyExt;
use hyper::body::Body;
use hyper::{Method, Request, Response};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use pulsebeam::net::PacketSocket;
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Clone)]
pub struct VirtualUdpSocket(pub Arc<turmoil::net::UdpSocket>);

impl PacketSocket for VirtualUdpSocket {
    fn local_addr(&self) -> Result<SocketAddr, std::io::Error> {
        self.0.local_addr()
    }

    fn send_to(
        &self,
        buf: &[u8],
        addr: SocketAddr,
    ) -> impl Future<Output = std::io::Result<usize>> {
        self.0.send_to(buf, addr)
    }

    fn recv_from(
        &self,
        buf: &mut [u8],
    ) -> impl Future<Output = std::io::Result<(usize, SocketAddr)>> {
        self.0.recv_from(buf)
    }
}

pub struct VirtualTcpListener(pub turmoil::net::TcpListener);

impl Listener for VirtualTcpListener {
    type Io = turmoil::net::TcpStream;

    type Addr = SocketAddr;

    fn accept(&mut self) -> impl std::future::Future<Output = (Self::Io, Self::Addr)> + Send {
        async move { self.0.accept().await.unwrap() }
    }

    fn local_addr(&self) -> tokio::io::Result<Self::Addr> {
        self.0.local_addr()
    }
}

pub async fn offer(uri: String, offer: String) -> anyhow::Result<String> {
    let client = Client::builder(TokioExecutor::new()).build(connector::connector());
    let mut request = Request::new(offer);
    *request.method_mut() = Method::POST;
    *request.uri_mut() = uri.parse()?;
    request
        .headers_mut()
        .insert("Content-Type", "application/sdp".parse().unwrap());
    let res = client.request(request).await?;

    let (parts, body) = res.into_parts();
    let raw = body.collect().await?.to_bytes();
    let res = hyper::Response::from_parts(parts, raw);
    tracing::info!("status: {}, uri:{}", res.status(), uri);

    // let answer = String::from_utf8(res.body())?;
    // tracing::info!("body:\n{}", answer);
    Ok("".to_string())
}

mod connector {
    use hyper::Uri;
    use pin_project_lite::pin_project;
    use std::{future::Future, io::Error, pin::Pin};
    use tokio::io::AsyncWrite;
    use tower::Service;
    use turmoil::net::TcpStream;

    type Fut = Pin<Box<dyn Future<Output = Result<TurmoilConnection, Error>> + Send>>;

    pub fn connector()
    -> impl Service<Uri, Response = TurmoilConnection, Error = Error, Future = Fut> + Clone {
        tower::service_fn(|uri: Uri| {
            Box::pin(async move {
                let conn = TcpStream::connect(uri.authority().unwrap().as_str()).await?;
                Ok::<_, Error>(TurmoilConnection { fut: conn })
            }) as Fut
        })
    }

    pin_project! {
        pub struct TurmoilConnection{
            #[pin]
            fut: turmoil::net::TcpStream
        }
    }

    impl hyper::rt::Read for TurmoilConnection {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            mut buf: hyper::rt::ReadBufCursor<'_>,
        ) -> std::task::Poll<Result<(), Error>> {
            let n = unsafe {
                let mut tbuf = tokio::io::ReadBuf::uninit(buf.as_mut());
                let result = tokio::io::AsyncRead::poll_read(self.project().fut, cx, &mut tbuf);
                match result {
                    std::task::Poll::Ready(Ok(())) => tbuf.filled().len(),
                    other => return other,
                }
            };

            unsafe {
                buf.advance(n);
            }
            std::task::Poll::Ready(Ok(()))
        }
    }

    impl hyper::rt::Write for TurmoilConnection {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<Result<usize, Error>> {
            Pin::new(&mut self.fut).poll_write(cx, buf)
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Error>> {
            Pin::new(&mut self.fut).poll_flush(cx)
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Error>> {
            Pin::new(&mut self.fut).poll_shutdown(cx)
        }
    }

    impl hyper_util::client::legacy::connect::Connection for TurmoilConnection {
        fn connected(&self) -> hyper_util::client::legacy::connect::Connected {
            hyper_util::client::legacy::connect::Connected::new()
        }
    }
}
