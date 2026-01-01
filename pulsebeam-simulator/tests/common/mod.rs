use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use pulsebeam_core::net::{AsyncHttpClient, HttpError, HttpRequest, HttpResult};

pub fn add(a: u32, b: u32) -> u32 {
    a + b
}

pub fn create_http_client() -> Box<dyn AsyncHttpClient> {
    let client = Client::builder(TokioExecutor::new()).build(connector::connector());
    let client = HyperClientWrapper(client);
    Box::new(client)
}

pub struct HyperClientWrapper<C>(pub Client<C, Full<Bytes>>);

impl<C> AsyncHttpClient for HyperClientWrapper<C>
where
    // These bounds are required for Hyper to actually send a request
    C: tower::Service<http::Uri> + Clone + Send + Sync + 'static,
    C::Response: hyper::rt::Read
        + hyper::rt::Write
        + hyper_util::client::legacy::connect::Connection
        + Send
        + Unpin,
    C::Future: Send + Unpin,
    C::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    fn execute(&self, req: HttpRequest) -> HttpResult<'_> {
        let client = self.0.clone();

        Box::pin(async move {
            // 1. Convert http::Request<Vec<u8>> -> http::Request<Full<Bytes>>
            let (parts, body) = req.into_parts();
            let hyper_req = http::Request::from_parts(parts, Full::new(Bytes::from(body)));

            // 2. Execute via Hyper
            let res = client
                .request(hyper_req)
                .await
                .map_err(|e| Box::new(e) as HttpError)?;

            // 3. Buffer the streaming body back into a Vec<u8>
            let (parts, res_body) = res.into_parts();
            let bytes = res_body
                .collect()
                .await
                .map_err(|e| Box::new(e) as HttpError)?
                .to_bytes();

            Ok(http::Response::from_parts(parts, bytes.to_vec()))
        })
    }
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
