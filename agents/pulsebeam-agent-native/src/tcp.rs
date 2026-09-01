use bytes::{Buf, BytesMut};
use std::io;
use std::net::SocketAddr;

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
    /// A framed payload whose write was only partially accepted by TCP.
    pending_write: BytesMut,
    /// Local address of the TCP socket (used as `Receive::destination`).
    local_addr: Option<SocketAddr>,
    /// Server's TCP address (used as `Receive::source`).
    server_addr: Option<SocketAddr>,
}

impl TcpSession {
    /// RFC 4571's length field is the complete 16-bit unsigned range.
    const MAX_FRAME: usize = u16::MAX as usize;
    const MAX_PENDING_WRITE: usize = Self::MAX_FRAME.saturating_mul(8);

    /// No TCP connectivity; the select arm for this session parks indefinitely.
    pub(crate) fn inactive() -> Self {
        Self {
            stream: None,
            buf: vec![0u8; 2048],
            recv_accum: BytesMut::new(),
            pending_write: BytesMut::new(),
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
            pending_write: BytesMut::new(),
            local_addr,
            server_addr: Some(server_addr),
        }
    }

    pub(crate) fn server_addr(&self) -> Option<SocketAddr> {
        self.server_addr
    }

    pub(crate) fn local_addr(&self) -> Option<SocketAddr> {
        self.local_addr
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

    pub(crate) fn receive_frames(&mut self, result: io::Result<usize>) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        match result {
            Ok(0) => {
                tracing::warn!("TCP stream closed by server");
                self.close();
            }
            Ok(n) => {
                let Some(chunk) = self.buf.get(..n) else {
                    debug_assert!(false, "recv reported {n} bytes into a smaller buffer");
                    return frames;
                };
                self.recv_accum.extend_from_slice(chunk);
                loop {
                    if self.recv_accum.len() < 2 {
                        break;
                    }
                    let (Some(&hi), Some(&lo)) = (self.recv_accum.first(), self.recv_accum.get(1))
                    else {
                        break;
                    };
                    let len = usize::from(u16::from_be_bytes([hi, lo]));
                    if len == 0 || len > Self::MAX_FRAME {
                        tracing::warn!(len, "invalid TCP frame length, closing stream");
                        self.close();
                        break;
                    }
                    if self.recv_accum.len() < len.saturating_add(2) {
                        break; // incomplete frame — wait for more data
                    }
                    self.recv_accum.advance(2);
                    let frame = self.recv_accum.split_to(len);
                    frames.push(frame.to_vec());
                }
            }
            Err(e) => {
                tracing::warn!(error = ?e, "TCP read error, closing stream");
                self.close();
            }
        }
        frames
    }

    pub(crate) fn try_send(&mut self, payload: &[u8]) {
        let Some(stream) = self.stream.as_ref() else {
            return;
        };

        let length = payload.len();
        if length > u16::MAX as usize {
            tracing::error!("TCP payload exceeds 64KB RFC limit");
            return;
        }

        let Ok(header_len) = u16::try_from(length) else {
            tracing::error!("TCP payload exceeds 64KB RFC limit");
            return;
        };
        let header = header_len.to_be_bytes();

        let mut packet = Vec::with_capacity(header.len().saturating_add(payload.len()));
        packet.extend_from_slice(&header);
        packet.extend_from_slice(payload);
        let pending = self.pending_write.len().saturating_add(packet.len());
        if pending > Self::MAX_PENDING_WRITE {
            tracing::warn!(pending, "TCP write queue is full, dropping frame");
            return;
        }
        self.pending_write.extend_from_slice(&packet);

        while !self.pending_write.is_empty() {
            match stream.try_write(&self.pending_write) {
                Ok(0) => {
                    self.close();
                    return;
                }
                Ok(n) => {
                    debug_assert!(n <= self.pending_write.len());
                    if n > self.pending_write.len() {
                        self.close();
                        return;
                    }
                    self.pending_write.advance(n);
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) => {
                    tracing::warn!("TCP pending write failed, closing stream: {:?}", e);
                    self.close();
                    return;
                }
            }
        }

        debug_assert!(self.pending_write.len() <= Self::MAX_PENDING_WRITE);
    }

    pub(crate) fn close(&mut self) {
        self.stream = None;
        self.recv_accum.clear();
        self.pending_write.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn framed(payload: &[u8]) -> Vec<u8> {
        let length = u16::try_from(payload.len()).unwrap();
        let mut frame = length.to_be_bytes().to_vec();
        frame.extend_from_slice(payload);
        frame
    }

    #[test]
    fn receive_reassembles_partial_and_consecutive_frames() {
        let first = vec![7; 5_635];
        let second = vec![9; 37];
        let bytes = [framed(&first), framed(&second)].concat();
        let split = 2_001;
        let mut tcp = TcpSession::inactive();

        tcp.buf = bytes[..split].to_vec();
        assert!(tcp.receive_frames(Ok(split)).is_empty());
        tcp.buf = bytes[split..].to_vec();
        let frames = tcp.receive_frames(Ok(bytes.len() - split));

        assert_eq!(frames, vec![first, second]);
    }

    #[test]
    fn receive_rejects_zero_length_frames() {
        let mut tcp = TcpSession::inactive();
        tcp.buf = vec![0, 0];

        assert!(tcp.receive_frames(Ok(2)).is_empty());
        assert!(tcp.recv_accum.is_empty());
    }
}
