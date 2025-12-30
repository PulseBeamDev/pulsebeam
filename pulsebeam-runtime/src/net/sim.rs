use super::{RecvPacketBatch, SendPacketBatch, Transport};
use bytes::Bytes;
use std::{
    collections::VecDeque,
    io::{self, ErrorKind},
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, mpsc};

// --- Writer Implementation ---

#[derive(Clone)]
pub struct SimSocketWriter {
    // Channel to the background task that performs the async write
    tx: mpsc::Sender<(Bytes, SocketAddr)>,
    transport: Transport,
    local_addr: SocketAddr,
}

impl SimSocketWriter {
    pub fn new_udp(socket: Arc<turmoil::net::UdpSocket>) -> Self {
        let local_addr = socket.local_addr().unwrap();
        let (tx, mut rx) = mpsc::channel::<(Bytes, SocketAddr)>(1024);

        // Background writer task
        tokio::spawn(async move {
            while let Some((buf, dst)) = rx.recv().await {
                let _ = socket.send_to(&buf, dst).await;
            }
        });

        Self {
            tx,
            transport: Transport::Udp,
            local_addr,
        }
    }

    /// Generic constructor for any AsyncWrite object (including split TCP WriteHalves)
    pub fn new_io<W>(mut writer: W, local_addr: SocketAddr, transport: Transport) -> Self
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (tx, mut rx) = mpsc::channel::<(Bytes, SocketAddr)>(1024);

        // Background writer task
        tokio::spawn(async move {
            while let Some((buf, _)) = rx.recv().await {
                if writer.write_all(&buf).await.is_err() {
                    break;
                }
            }
        });

        Self {
            tx,
            transport,
            local_addr,
        }
    }

    pub fn transport(&self) -> Transport {
        self.transport
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn max_gso_segments(&self) -> usize {
        1 // Turmoil doesn't support GSO, so we report 1 to force manual chunking
    }

    pub async fn writable(&self) -> io::Result<()> {
        // use reserve() to wait for capacity, effectively "writable" check
        self.tx
            .reserve()
            .await
            .map(|_| ())
            .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "writer closed"))
    }

    pub fn try_send_batch(&self, batch: &SendPacketBatch) -> io::Result<bool> {
        if batch.segment_size == 0 {
            return Err(io::Error::new(ErrorKind::InvalidInput, "segment_size 0"));
        }

        // Manual Segmentation (Simulate GSO/Fragmentation)
        // Since `turmoil` accepts full packets, we chunk them based on segment_size.
        let mut offset = 0;
        let len = batch.buf.len();

        while offset < len {
            let end = std::cmp::min(offset + batch.segment_size, len);

            // We must copy the data to pass it to the channel (and thus the background task)
            let buf = Bytes::copy_from_slice(&batch.buf[offset..end]);

            // Try to enqueue for the background writer
            match self.tx.try_send((buf, batch.dst)) {
                Ok(_) => {
                    offset = end;
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    // Backpressure: If channel is full, we stop and report partial success or wait.
                    if offset == 0 {
                        return Ok(false);
                    } else {
                        // We successfully queued some chunks.
                        return Ok(true);
                    }
                }
                Err(_) => return Err(io::Error::new(ErrorKind::BrokenPipe, "writer closed")),
            }
        }

        Ok(true)
    }
}

// --- Reader Implementation ---

struct InboxMsg {
    buf: Bytes,
    src: SocketAddr,
}

struct SharedReaderState {
    queue: Mutex<VecDeque<io::Result<InboxMsg>>>,
    notify: Notify,
}

pub struct SimSocketReader {
    shared: Arc<SharedReaderState>,
    local_addr: SocketAddr,
    transport: Transport,
}

impl SimSocketReader {
    pub fn new_udp(socket: Arc<turmoil::net::UdpSocket>) -> Self {
        let shared = Arc::new(SharedReaderState {
            queue: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
        });
        let local_addr = socket.local_addr().unwrap();
        let s2 = shared.clone();

        // Background reader task
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            loop {
                let res = match socket.recv_from(&mut buf).await {
                    Ok((len, src)) => Ok(InboxMsg {
                        buf: Bytes::copy_from_slice(&buf[..len]),
                        src,
                    }),
                    Err(e) => Err(e),
                };

                {
                    let mut q = s2.queue.lock().unwrap();
                    q.push_back(res);
                }
                s2.notify.notify_one();
            }
        });

        Self {
            shared,
            local_addr,
            transport: Transport::Udp,
        }
    }

    /// Generic constructor for any AsyncRead object (including split TCP ReadHalves)
    pub fn new_io<R>(
        mut reader: R,
        local_addr: SocketAddr,
        peer_addr: SocketAddr,
        transport: Transport,
    ) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        let shared = Arc::new(SharedReaderState {
            queue: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
        });
        let s2 = shared.clone();

        // Background reader task
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            loop {
                let res = match reader.read(&mut buf).await {
                    Ok(0) => Err(io::Error::new(ErrorKind::ConnectionAborted, "EOF")),
                    Ok(len) => Ok(InboxMsg {
                        buf: Bytes::copy_from_slice(&buf[..len]),
                        src: peer_addr,
                    }),
                    Err(e) => Err(e),
                };

                let is_err = res.is_err();
                {
                    let mut q = s2.queue.lock().unwrap();
                    q.push_back(res);
                }
                s2.notify.notify_one();

                if is_err {
                    break;
                }
            }
        });

        Self {
            shared,
            local_addr,
            transport,
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn close_peer(&mut self, _peer_addr: &SocketAddr) {
        // TCP Stream teardown is handled by dropping the socket/halves
    }

    pub async fn readable(&self) -> io::Result<()> {
        // Wait until the queue is not empty
        loop {
            {
                let q = self.shared.queue.lock().unwrap();
                if !q.is_empty() {
                    return Ok(());
                }
            }
            // Wait for notification from background task
            self.shared.notify.notified().await;
        }
    }

    pub fn try_recv_batch(&mut self, packets: &mut Vec<RecvPacketBatch>) -> io::Result<()> {
        let mut q = self.shared.queue.lock().unwrap();

        // Drain up to BATCH_SIZE items from the internal queue
        while let Some(res) = q.pop_front() {
            match res {
                Ok(msg) => {
                    packets.push(RecvPacketBatch {
                        src: msg.src,
                        dst: self.local_addr,
                        len: msg.buf.len(),
                        stride: msg.buf.len(),
                        buf: msg.buf,
                        transport: self.transport,
                    });

                    if packets.len() >= super::BATCH_SIZE {
                        break;
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}
