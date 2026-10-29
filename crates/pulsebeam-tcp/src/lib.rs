//! RFC 4571 framed I/O over any Tokio byte stream.
//!
//! This crate has no runtime, task, routing, authentication or queue policy.

#[allow(
    clippy::disallowed_types,
    reason = "standalone framing API transfers owned frames; it has no shard or shared-payload policy"
)]
use bytes::Bytes;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};

/// A cancellation-safe, bounded RFC 4571 frame reader.
pub struct FrameReader<R> {
    io: R,
    max: usize,
    header: [u8; 2],
    header_read: usize,
    payload: Vec<u8>,
    payload_read: usize,
    ended: bool,
}

/// An RFC 4571 frame writer. Retire the stream if a send is cancelled or fails.
pub struct FrameWriter<W> {
    io: W,
    max: usize,
}

/// Split a stream into independent framed halves.
///
/// Rejects limits exceeding the unsigned 16-bit wire length before splitting.
#[allow(
    clippy::type_complexity,
    reason = "the return value exposes the independent standard Tokio read/write halves directly"
)]
pub fn split<T: AsyncRead + AsyncWrite + Unpin>(
    stream: T,
    max_frame_bytes: usize,
) -> io::Result<(FrameReader<ReadHalf<T>>, FrameWriter<WriteHalf<T>>)> {
    if max_frame_bytes > usize::from(u16::MAX) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "RFC 4571 limit exceeds u16",
        ));
    }
    let (read, write) = tokio::io::split(stream);
    Ok((
        FrameReader {
            io: read,
            max: max_frame_bytes,
            header: [0; 2],
            header_read: 0,
            payload: Vec::new(),
            payload_read: 0,
            ended: false,
        },
        FrameWriter {
            io: write,
            max: max_frame_bytes,
        },
    ))
}

#[allow(
    clippy::disallowed_types,
    reason = "each Bytes allocation is transferred to the caller, never cloned or shared here"
)]
impl<R: AsyncRead + Unpin> FrameReader<R> {
    /// Receive a complete frame. `None` is clean EOF; an empty frame is legal.
    ///
    /// Partial headers/payloads at EOF are errors. Invalid lengths are rejected
    /// before allocation. After an error the reader is terminal.
    pub async fn recv(&mut self) -> io::Result<Option<Bytes>> {
        if self.ended {
            return Ok(None);
        }
        let result = self.recv_inner().await;
        if result.is_err() || matches!(result, Ok(None)) {
            self.ended = true;
        }
        result
    }

    async fn recv_inner(&mut self) -> io::Result<Option<Bytes>> {
        while self.header_read < 2 {
            let remaining = self
                .header
                .get_mut(self.header_read..)
                .ok_or(io::ErrorKind::InvalidData)?;
            let n = self.io.read(remaining).await?;
            if n == 0 {
                return if self.header_read == 0 {
                    Ok(None)
                } else {
                    Err(io::ErrorKind::UnexpectedEof.into())
                };
            }
            self.header_read = self
                .header_read
                .checked_add(n)
                .ok_or(io::ErrorKind::InvalidData)?;
            if self.header_read == 2 {
                let len = usize::from(u16::from_be_bytes(self.header));
                if len > self.max {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "RFC 4571 frame exceeds limit",
                    ));
                }
                self.payload.resize(len, 0);
            }
        }
        while self.payload_read < self.payload.len() {
            let remaining = self
                .payload
                .get_mut(self.payload_read..)
                .ok_or(io::ErrorKind::InvalidData)?;
            let n = self.io.read(remaining).await?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            self.payload_read = self
                .payload_read
                .checked_add(n)
                .ok_or(io::ErrorKind::InvalidData)?;
        }
        self.header_read = 0;
        self.payload_read = 0;
        Ok(Some(Bytes::from(std::mem::take(&mut self.payload))))
    }
}

impl<W: AsyncWrite + Unpin> FrameWriter<W> {
    /// Write one complete frame, preserving FIFO order across completed sends.
    ///
    /// Cancellation or an I/O error can leave a partial frame on the wire. The
    /// caller must retire the stream, never retry or send another frame on it.
    pub async fn send(&mut self, packet: &[u8]) -> io::Result<()> {
        let len = u16::try_from(packet.len())
            .ok()
            .filter(|_| packet.len() <= self.max)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "RFC 4571 frame exceeds limit")
            })?;
        self.io.write_all(&len.to_be_bytes()).await?;
        self.io.write_all(packet).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn partial_io_null_frames_and_wire_order() {
        let (a, b) = tokio::io::duplex(1);
        let (_, mut writer) = split(a, 65535).unwrap();
        let (mut reader, _) = split(b, 65535).unwrap();
        let payload = vec![42; 65535];
        let send = async {
            writer.send(&[]).await.unwrap();
            writer.send(b"abc").await.unwrap();
            writer.send(&payload).await.unwrap();
            drop(writer);
        };
        let recv = async {
            assert!(reader.recv().await.unwrap().unwrap().is_empty());
            assert_eq!(&reader.recv().await.unwrap().unwrap()[..], b"abc");
            assert_eq!(reader.recv().await.unwrap().unwrap(), payload);
            assert!(reader.recv().await.unwrap().is_none());
        };
        tokio::join!(send, recv);
    }

    #[tokio::test]
    async fn big_endian_and_coalesced_frames() {
        let (mut raw, framed) = tokio::io::duplex(1024);
        let (mut reader, mut writer) = split(framed, 256).unwrap();
        raw.write_all(&[0, 0, 0, 3, 1, 2, 3]).await.unwrap();
        assert!(reader.recv().await.unwrap().unwrap().is_empty());
        assert_eq!(&reader.recv().await.unwrap().unwrap()[..], &[1, 2, 3]);
        writer.send(&[7; 256]).await.unwrap();
        let mut wire = vec![0; 258];
        raw.read_exact(&mut wire).await.unwrap();
        assert_eq!(&wire[..2], &[1, 0]);
        assert_eq!(&wire[2..], &[7; 256]);
    }

    #[tokio::test]
    async fn cancelled_receive_preserves_partial_header_and_payload() {
        let (mut raw, framed) = tokio::io::duplex(32);
        let (mut reader, _) = split(framed, 32).unwrap();
        for part in [&[0][..], &[3, 1][..]] {
            raw.write_all(part).await.unwrap();
            tokio::select! {
                biased;
                result = reader.recv() => panic!("incomplete frame: {result:?}"),
                _ = std::future::ready(()) => {}
            }
        }
        raw.write_all(&[2, 3]).await.unwrap();
        assert_eq!(&reader.recv().await.unwrap().unwrap()[..], &[1, 2, 3]);
    }

    #[tokio::test]
    async fn limits_and_truncated_eof_are_terminal() {
        let (a, _) = tokio::io::duplex(1);
        assert!(split(a, 65536).is_err());
        for (wire, kind) in [
            (&[0][..], io::ErrorKind::UnexpectedEof),
            (&[0, 2, 1][..], io::ErrorKind::UnexpectedEof),
            (&[0, 4][..], io::ErrorKind::InvalidData),
        ] {
            let (mut raw, framed) = tokio::io::duplex(32);
            let (mut reader, _) = split(framed, 3).unwrap();
            raw.write_all(wire).await.unwrap();
            drop(raw);
            assert_eq!(reader.recv().await.unwrap_err().kind(), kind);
            assert!(reader.recv().await.unwrap().is_none());
        }
        let (a, mut b) = tokio::io::duplex(8);
        let (_, mut writer) = split(a, 3).unwrap();
        assert_eq!(
            writer.send(b"long").await.unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        writer.send(b"ok").await.unwrap();
        let mut wire = [0; 4];
        b.read_exact(&mut wire).await.unwrap();
        assert_eq!(&wire, b"\0\x02ok");
    }
}
