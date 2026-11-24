use std::time::Instant;
use std::{io, net::SocketAddr};

use noq_proto::{ConnectionHandle, EcnCodepoint, FourTuple, ServerConfig};
use pulsebeam_udp::{Ecn, ReceivedDatagram, UdpSocket};

use super::close;
use super::listener::{Association, Listener};
use super::socket::{Egress, Flush};

/// Packet execution owned by the caller's logical shard.
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
    pub fn on_shard(config: ServerConfig, shard: u16) -> Result<Self, noq_proto::ConfigError> {
        Ok(Self {
            listener: Listener::on_shard(config, shard)?,
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

    pub fn receive(
        &mut self,
        local: SocketAddr,
        item: ReceivedDatagram<'_>,
        now: Instant,
        service: impl FnOnce(ConnectionHandle, &mut Association),
    ) -> io::Result<()> {
        let path = FourTuple::new(item.source, Some(item.destination.ip()));
        // Interleave application DATAGRAM reads with batched socket input. Noq
        // evicts old DATAGRAMs when its byte window fills, even on a lossless path.
        let receive = |out: &mut Vec<u8>| {
            self.listener
                .receive_then(now, path, ecn(item.ecn), item.payload, out, service)
        };
        // Stateless replies are best-effort when the single send slot is full.
        if self.egress.is_pending() {
            receive(&mut self.stateless_scratch);
        } else {
            self.egress.stage(local, receive)?;
        }
        self.schedule();
        Ok(())
    }

    /// A WouldBlock send retains the exact committed packet; further output
    /// is not pulled into a second queue.
    pub fn poll(
        &mut self,
        socket: &mut UdpSocket,
        now: Instant,
        service: impl FnMut(ConnectionHandle, &mut Association),
    ) -> io::Result<()> {
        if self
            .listener
            .deadline()
            .is_some_and(|deadline| deadline <= now)
        {
            self.schedule();
        }
        let local = socket.local_addr();
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
