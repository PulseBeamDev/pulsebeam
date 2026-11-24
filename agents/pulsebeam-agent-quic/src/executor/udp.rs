use std::{
    io,
    net::{IpAddr, SocketAddr},
    time::Instant,
};

use noq_proto::ClientConfig;
use pulsebeam_agent_core::{Generation, QuicAdmit, QuicEffect, QuicEvent};
use pulsebeam_udp::UdpSocket;
use tokio::time;

use crate::transport::socket::{self, Egress, Flush};

use super::{Association, SubmitError};

#[derive(Debug)]
pub enum OpenError {
    Socket(io::Error),
    Admission(QuicEvent),
}

/// UDP reactor owner for one association. It retains the committed packet
/// across backpressure and cancellation of an in-progress wait.
pub struct UdpAssociation {
    socket: UdpSocket,
    association: Association,
    egress: Egress,
}

impl UdpAssociation {
    pub async fn connect(
        clock_origin: Instant,
        local: IpAddr,
        remote: IpAddr,
        config: ClientConfig,
        admit: QuicAdmit,
    ) -> Result<Self, OpenError> {
        if local.is_ipv4() != remote.is_ipv4() {
            return Err(OpenError::Socket(io::Error::new(
                io::ErrorKind::InvalidInput,
                "QUIC address family mismatch",
            )));
        }
        let socket = socket::bind(SocketAddr::new(local, 0))
            .await
            .map_err(OpenError::Socket)?
            .activate()
            .map_err(OpenError::Socket)?;
        let resolved = SocketAddr::new(remote, admit.placement.port.get());
        let association = Association::connect(
            clock_origin,
            time::Instant::now().into_std(),
            config,
            resolved,
            admit,
        )
        .map_err(OpenError::Admission)?;
        Ok(Self {
            socket,
            association,
            egress: Egress::default(),
        })
    }

    pub(super) async fn rebind(&mut self, local: IpAddr) -> io::Result<()> {
        if !matches!(self.association.stage, super::Stage::Active { .. }) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "QUIC association is not active",
            ));
        }
        if local.is_ipv4() != self.socket.local_addr().is_ipv4() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "QUIC address family mismatch",
            ));
        }
        let socket = socket::bind(SocketAddr::new(local, 0)).await?.activate()?;
        self.socket = socket;
        // A packet staged for the old socket cannot advance the new path.
        self.egress = Egress::default();
        self.association
            .transport
            .connection()
            .handle_network_change(None, time::Instant::now().into_std());
        Ok(())
    }

    pub fn generation(&self) -> Generation {
        self.association.generation()
    }

    pub(super) fn io_failure(&self) -> QuicEvent {
        let generation = self.generation();
        let failure = super::transient("QUIC UDP reactor failed");
        match self.association.stage {
            super::Stage::Admitting(_) => QuicEvent::AdmissionFailed {
                generation,
                failure,
            },
            super::Stage::Closing(_) => QuicEvent::Closed { generation },
            super::Stage::Active { .. } | super::Stage::Finished => QuicEvent::TransportLost {
                generation,
                failure,
            },
        }
    }

    #[cfg(feature = "sim")]
    pub(super) fn data_staging(&self) -> super::DataStaging {
        self.association.data_staging
    }

    pub(super) fn peer_fabric(&self) -> Option<crate::data_bindings::FabricOffer> {
        self.association.peer_fabric()
    }

    pub(super) fn offer_media_packet(
        &mut self,
        sender: &mut crate::transport::framing::Sender,
        now: std::time::Instant,
    ) -> Result<crate::transport::framing::SendProgress, crate::transport::framing::SendError> {
        self.association.offer_media_packet(sender, now)
    }

    pub(super) fn offer_media(
        &mut self,
        datagram: crate::records::Datagram<'_>,
    ) -> Result<(), crate::transport::DatagramAdmission> {
        self.association.offer_media(datagram)
    }

    pub fn submit(&mut self, effect: QuicEffect) -> Result<(), SubmitError> {
        self.association
            .submit(effect, time::Instant::now().into_std())
    }

    /// One bounded reactor turn. `None` means the owner can poll again or run
    /// other ready Core work. With no progress, waits for I/O or the next deadline.
    pub(super) async fn progress(
        &mut self,
        retry_payload_turn: bool,
        authorize: impl Fn(std::num::NonZeroU16) -> Option<(super::ReceiveBinding, bool)>,
    ) -> io::Result<Option<QuicEvent>> {
        let now = time::Instant::now().into_std();
        self.association.transport.service_timer(now);
        let event = self.association.poll_event(now, authorize);
        let mut more = false;
        self.egress.stage(self.socket.local_addr(), |out| {
            let (packet, pending) = self.association.poll_packet(now, out);
            more = pending;
            packet
        })?;
        let blocked = match self.egress.flush(&mut self.socket)? {
            Flush::PathMtu => {
                return Err(io::Error::other("QUIC path MTU below transport minimum"));
            }
            Flush::Sent => {
                more = true;
                false
            }
            Flush::Blocked => true,
            Flush::Empty => false,
        };
        match self.socket.try_recv_batch() {
            Ok(batch) => {
                for datagram in batch.iter() {
                    more = true;
                    self.association.receive(now, datagram);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        if let Some(event) = event {
            return Ok(Some(event));
        }
        // A control turn can unlock payload admission without emitting a packet.
        if !(more || retry_payload_turn && !blocked) {
            let socket = &self.socket;
            let deadline = self.association.deadline().map(time::Instant::from_std);
            let sleep = async {
                match deadline {
                    Some(at) => time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                ready = socket.readable() => ready?,
                ready = async {
                    if blocked { socket.writable().await } else { std::future::pending().await }
                } => ready?,
                () = sleep => {},
            }
        }
        Ok(None)
    }
}
