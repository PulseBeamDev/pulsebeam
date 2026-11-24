use std::{net::SocketAddr, time::Instant};

use bytes::BytesMut;
use noq_proto::{
    ClientConfig, ConnectError, Connection, ConnectionHandle, DatagramEvent, Endpoint, FourTuple,
    Transmit,
};
use pulsebeam_udp::ReceivedDatagram;

use crate::records::Datagram;

use super::{
    DatagramAdmission, PacketScheduler, endpoint, forward_endpoint_event, service_timeout,
};

pub mod admission;

#[derive(Debug)]
pub enum StartError {
    Endpoint(noq_proto::ConfigError),
    Connect(ConnectError),
}

/// Single-association client packet engine. The caller owns UDP I/O and the clock.
pub struct ClientTransport {
    endpoint: Endpoint,
    handle: ConnectionHandle,
    connection: Connection,
    scheduler: PacketScheduler,
    response: Vec<u8>,
}

impl ClientTransport {
    pub fn connect(
        now: Instant,
        config: ClientConfig,
        address: SocketAddr,
        authority: &str,
    ) -> Result<Self, StartError> {
        let mut endpoint = endpoint(None).map_err(StartError::Endpoint)?;
        let (handle, connection) = endpoint
            .connect(now, config, address, authority)
            .map_err(StartError::Connect)?;
        Ok(Self {
            endpoint,
            handle,
            connection,
            scheduler: PacketScheduler::default(),
            response: Vec::new(),
        })
    }

    pub fn connection(&mut self) -> &mut Connection {
        &mut self.connection
    }

    /// Current maximum encoded application datagram size, including its envelope.
    pub fn datagram_limit(&mut self) -> Option<usize> {
        self.connection.datagrams().max_size()
    }

    /// The association owner must resolve the accepted binding before offering payload.
    pub fn offer_datagram(&mut self, datagram: Datagram<'_>) -> Result<(), DatagramAdmission> {
        self.scheduler.offer_bound(&mut self.connection, datagram)
    }

    /// Drive one frame on an already installed sender binding.
    pub fn offer_packet(
        &mut self,
        sender: &mut super::framing::Sender,
        now: Instant,
    ) -> Result<super::framing::SendProgress, super::framing::SendError> {
        sender.offer(&mut self.scheduler, &mut self.connection, now)
    }

    /// Consume at most one received datagram. The callback must resolve the binding
    /// before inspecting its payload.
    pub fn read_datagram<T>(
        &mut self,
        receive: impl FnOnce(Datagram<'_>) -> T,
    ) -> Option<Result<T, crate::frame::Error>> {
        super::read_datagram(&mut self.connection, receive)
    }

    pub fn deadline(&mut self) -> Option<Instant> {
        self.connection.poll_timeout()
    }

    /// Do bounded transport work and produce at most one packet. Flush a packet before
    /// polling again. The return flag requests another turn for endpoint bookkeeping.
    pub fn poll(&mut self, now: Instant, out: &mut Vec<u8>) -> (Option<Transmit>, bool) {
        self.service_timer(now);
        let more = (0..16)
            .all(|_| forward_endpoint_event(&mut self.endpoint, self.handle, &mut self.connection));
        (
            self.scheduler.poll(&mut self.connection, now, out, false),
            more,
        )
    }

    pub(crate) fn service_timer(&mut self, now: Instant) {
        service_timeout(&mut self.connection, now);
    }

    pub fn receive(&mut self, now: Instant, datagram: ReceivedDatagram<'_>) {
        self.response.clear();
        if let Some(DatagramEvent::ConnectionEvent(handle, event)) = self.endpoint.handle(
            now,
            FourTuple::new(datagram.source, Some(datagram.destination.ip())),
            None,
            BytesMut::from(datagram.payload),
            &mut self.response,
        ) && handle == self.handle
        {
            self.connection.handle_event(event);
        }
    }
}
