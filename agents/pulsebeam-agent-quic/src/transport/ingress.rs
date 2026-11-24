use std::io;
use std::time::Instant;

use noq_proto::{ConnectionHandle, EcnCodepoint, FourTuple, ServerConfig};
use pulsebeam_udp::{Ecn, UdpSocket};

use super::close;
use super::listener::{Association, Listener};
use super::socket::{Egress, Flush};

const RECEIVE_BUDGET: usize = 32;

/// Packet execution for a dedicated socket owned by the caller's logical shard.
/// Socket registration is managed separately; all protocol and egress state
/// stays in this value when the shard moves.
pub struct Ingress {
    listener: Listener,
    egress: Egress,
    pending: Option<ConnectionHandle>,
    scan: usize,
    stateless_scratch: Vec<u8>,
}

impl Ingress {
    pub fn new(config: ServerConfig) -> Result<Self, noq_proto::ConfigError> {
        Ok(Self {
            listener: Listener::new(config)?,
            egress: Egress::default(),
            pending: None,
            scan: 0,
            stateless_scratch: Vec::new(),
        })
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.listener.deadline()
    }

    pub fn wants_write(&self) -> bool {
        self.egress.is_pending()
    }

    pub fn runnable(&self) -> bool {
        self.scan != 0 && !self.wants_write()
    }

    /// The owner calls this when a controller command can produce output.
    pub fn schedule(&mut self) {
        self.scan = self.listener.association_count();
    }

    pub fn admitted(&mut self, handle: ConnectionHandle) -> bool {
        self.listener.admitted(handle)
    }

    pub fn get(&mut self, handle: ConnectionHandle) -> Option<&mut Association> {
        self.listener.get(handle)
    }

    fn flush(&mut self, socket: &mut UdpSocket, now: Instant) -> io::Result<bool> {
        match self.egress.flush(socket)? {
            Flush::Blocked => return Ok(false),
            Flush::PathMtu => {
                if let Some(handle) = self.pending.take()
                    && let Some(peer) = self.listener.get(handle)
                {
                    close(&mut peer.connection, now);
                }
                self.egress.discard();
                self.schedule();
            }
            Flush::Empty | Flush::Sent => {}
        }
        self.pending = None;
        Ok(true)
    }

    /// Packet and connection work per call is bounded. A WouldBlock send
    /// keeps the exact committed UDP packet until writable; further protocol
    /// output is not pulled into a second queue.
    pub fn turn(
        &mut self,
        socket: &mut UdpSocket,
        now: Instant,
        service: impl FnMut(ConnectionHandle, &mut Association),
    ) -> io::Result<()> {
        if !self.flush(socket, now)? {
            return Ok(());
        }
        if self
            .listener
            .deadline()
            .is_some_and(|deadline| deadline <= now)
        {
            self.schedule();
        }
        let local = socket.local_addr();
        let mut received = false;
        match socket.try_recv_batch() {
            Ok(batch) => {
                for item in batch.iter().take(RECEIVE_BUDGET) {
                    received = true;
                    let path = FourTuple::new(item.source, Some(item.destination.ip()));
                    // Stateless replies are best-effort. Continue processing
                    // this receive batch even if the single reply slot is full.
                    if self.egress.is_pending() {
                        self.listener.receive(
                            now,
                            path,
                            ecn(item.ecn),
                            item.payload,
                            &mut self.stateless_scratch,
                        );
                    } else {
                        self.egress.stage(local, |out| {
                            self.listener
                                .receive(now, path, ecn(item.ecn), item.payload, out)
                        })?;
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        if received {
            self.schedule();
        }
        if !self.flush(socket, now)? {
            return Ok(());
        }
        self.listener.begin_turn();
        let mut service = service;
        let mut generated = false;
        while self.scan != 0 {
            let mut visit = None;
            self.egress.stage(local, |out| {
                let (handle, tx) = self.listener.poll_one(now, out, &mut service)?;
                visit = Some(handle);
                tx
            })?;
            let Some(handle) = visit else { break };
            self.scan = self.scan.saturating_sub(1);
            if self.egress.is_pending() {
                generated = true;
                self.pending = Some(handle);
                if !self.flush(socket, now)? {
                    break;
                }
            }
        }
        // Drained associations may reduce the number of visits available.
        if generated {
            self.schedule();
        } else {
            self.scan = self.scan.min(self.listener.association_count());
        }
        Ok(())
    }
}

fn ecn(mark: Option<Ecn>) -> Option<EcnCodepoint> {
    match mark? {
        Ecn::Ect0 => Some(EcnCodepoint::Ect0),
        Ecn::Ect1 => Some(EcnCodepoint::Ect1),
        Ecn::Ce => Some(EcnCodepoint::Ce),
        Ecn::NotEct => None,
    }
}
