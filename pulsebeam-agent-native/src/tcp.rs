use bytes::{BufMut, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

pub const MAX_FRAME_LENGTH: usize = u16::MAX as usize;

#[derive(Debug)]
pub struct TcpSession {
    stream: TcpStream,
    read_buffer: BytesMut,
}

impl TcpSession {
    pub fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            read_buffer: BytesMut::with_capacity(2 + MAX_FRAME_LENGTH),
        }
    }

    pub async fn read_frame(&mut self) -> Result<Option<Vec<u8>>, TcpError> {
        let mut length = [0u8; 2];
        match self.stream.read_exact(&mut length).await {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(TcpError::Io(error.to_string())),
        }
        let frame_length = usize::from(u16::from_be_bytes(length));
        debug_assert!(frame_length <= MAX_FRAME_LENGTH);
        let mut frame = vec![0u8; frame_length];
        self.stream
            .read_exact(&mut frame)
            .await
            .map_err(|error| TcpError::Io(error.to_string()))?;
        Ok(Some(frame))
    }

    pub async fn write_frame(&mut self, payload: &[u8]) -> Result<(), TcpError> {
        let frame = encode_frame(payload)?;
        self.stream
            .write_all(&frame)
            .await
            .map_err(|error| TcpError::Io(error.to_string()))
    }

    pub fn into_inner(self) -> TcpStream {
        self.stream
    }

    pub fn buffered_capacity(&self) -> usize {
        self.read_buffer.capacity()
    }
}

pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<Vec<u8>>, TcpError> {
    let mut length = [0u8; 2];
    match reader.read_exact(&mut length).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(TcpError::Io(error.to_string())),
    }
    let frame_length = usize::from(u16::from_be_bytes(length));
    let mut frame = vec![0u8; frame_length];
    reader
        .read_exact(&mut frame)
        .await
        .map_err(|error| TcpError::Io(error.to_string()))?;
    Ok(Some(frame))
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    payload: &[u8],
) -> Result<(), TcpError> {
    let frame = encode_frame(payload)?;
    writer
        .write_all(&frame)
        .await
        .map_err(|error| TcpError::Io(error.to_string()))
}

pub fn encode_frame(payload: &[u8]) -> Result<Vec<u8>, TcpError> {
    let length =
        u16::try_from(payload.len()).map_err(|_| TcpError::FrameTooLarge(payload.len()))?;
    let mut frame = BytesMut::with_capacity(2usize.saturating_add(payload.len()));
    frame.put_u16(length);
    frame.extend_from_slice(payload);
    Ok(frame.to_vec())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TcpError {
    Io(String),
    FrameTooLarge(usize),
}

impl std::fmt::Display for TcpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "TCP I/O: {error}"),
            Self::FrameTooLarge(length) => write!(formatter, "TCP frame too large: {length}"),
        }
    }
}

impl std::error::Error for TcpError {}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rfc4571_round_trips_maximum_frame() {
        let payload = vec![7u8; MAX_FRAME_LENGTH];
        let frame = encode_frame(&payload).unwrap();
        let mut reader = &frame[..];
        assert_eq!(read_frame(&mut reader).await.unwrap(), Some(payload));
    }

    #[test]
    fn oversized_frames_are_rejected_before_write() {
        assert_eq!(
            encode_frame(&vec![0; MAX_FRAME_LENGTH + 1]),
            Err(TcpError::FrameTooLarge(MAX_FRAME_LENGTH + 1))
        );
    }
}
