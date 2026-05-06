use bytes::{Buf, BytesMut};
use std::io;
use std::net::SocketAddr;
use str0m::{
    Input, Rtc,
    net::{Protocol, Receive},
};
use tokio::io::AsyncWriteExt;
use tokio::time::Instant;

/// RFC 4571 framing over a single active TCP connection (RFC 6544 client role).
///
/// Encapsulates the stream, raw read buffer, and reassembly state so the main
/// `AgentActor` event loop contains no TCP-specific framing logic.
pub(crate) struct TcpSession {
    stream: Option<pulsebeam_core::net::TcpStream>,
    /// Raw read staging buffer.
    buf: Vec<u8>,
    /// RFC 4571 reassembly buffer for incomplete frames.
    recv_accum: BytesMut,
    /// Local address of the TCP socket (used as `Receive::destination`).
    local_addr: Option<SocketAddr>,
    /// Server's TCP address (used as `Receive::source`).
    server_addr: Option<SocketAddr>,
}

impl TcpSession {
    /// Maximum RFC 4571 payload — one MTU-sized datagram equivalent.
    const MAX_FRAME: usize = 1_500;

    /// No TCP connectivity; the select arm for this session parks indefinitely.
    pub(crate) fn inactive() -> Self {
        Self {
            stream: None,
            buf: vec![0u8; 2048],
            recv_accum: BytesMut::new(),
            local_addr: None,
            server_addr: None,
        }
    }

    pub(crate) fn new(
        stream: pulsebeam_core::net::TcpStream,
        local_addr: Option<SocketAddr>,
        server_addr: SocketAddr,
    ) -> Self {
        Self {
            stream: Some(stream),
            buf: vec![0u8; 2048],
            recv_accum: BytesMut::new(),
            local_addr,
            server_addr: Some(server_addr),
        }
    }

    /// Await readable data on the stream.  Parks forever when there is no
    /// stream, which causes the `tokio::select!` arm to never fire.
    pub(crate) async fn wait_recv(&mut self) -> io::Result<usize> {
        use tokio::io::AsyncReadExt;
        match &mut self.stream {
            None => std::future::pending().await,
            Some(s) => s.read(self.buf.as_mut_slice()).await,
        }
    }

    /// Handle the result of a `wait_recv` call.  Decodes all complete RFC 4571
    /// frames and delivers them to `rtc` as `Input::Receive`.  Closes the
    /// stream on EOF or I/O error.
    pub(crate) fn on_recv(&mut self, result: io::Result<usize>, rtc: &mut Rtc) {
        match result {
            Ok(0) => {
                tracing::warn!("TCP stream closed by server");
                self.close();
            }
            Ok(n) => {
                self.recv_accum.extend_from_slice(&self.buf[..n]);
                loop {
                    if self.recv_accum.len() < 2 {
                        break;
                    }
                    let len = u16::from_be_bytes([self.recv_accum[0], self.recv_accum[1]]) as usize;
                    if len == 0 || len > Self::MAX_FRAME {
                        tracing::warn!(len, "invalid TCP frame length, closing stream");
                        self.close();
                        break;
                    }
                    if self.recv_accum.len() < 2 + len {
                        break; // incomplete frame — wait for more data
                    }
                    self.recv_accum.advance(2);
                    let frame = self.recv_accum.split_to(len);
                    if let (Ok(contents), Some(src), Some(dst)) =
                        (frame[..].try_into(), self.server_addr, self.local_addr)
                    {
                        let _ = rtc.handle_input(Input::Receive(
                            Instant::now().into(),
                            Receive {
                                proto: Protocol::Tcp,
                                source: src,
                                destination: dst,
                                contents,
                            },
                        ));
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = ?e, "TCP read error, closing stream");
                self.close();
            }
        }
    }

    /// RFC 4571-frame `payload` and write it to the stream.  Closes the
    /// stream on write failure.
    pub(crate) async fn send(&mut self, payload: &[u8]) {
        if let Some(stream) = &mut self.stream {
            let header = (payload.len() as u16).to_be_bytes();
            if stream.write_all(&header).await.is_err() || stream.write_all(payload).await.is_err()
            {
                tracing::warn!("TCP write failed, closing stream");
                self.stream = None;
            }
        }
    }

    pub(crate) fn close(&mut self) {
        self.stream = None;
        self.recv_accum.clear();
    }
}
