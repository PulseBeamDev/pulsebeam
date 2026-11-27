use std::num::NonZeroU64;

use pulsebeam_link::{LinkError, NetworkPeer, PeerControl};
use pulsebeam_runtime::sync::CancellationToken;

use super::*;

pub(crate) const MAX_EPOCH_WINDOWS: usize = 4096;

#[cfg(all(test, target_os = "linux", not(feature = "sim")))]
mod tests;

const LIMITS: wan::Limits = wan::Limits {
    active_windows: 128,
    epoch_windows: MAX_EPOCH_WINDOWS,
};

/// One fixed-address, nonfragmenting socket, reused only by fresh WAN
/// associations. Closing a session drops its key epoch and shard destinations.
pub(crate) struct Service {
    socket: UdpSocket,
    opens: pulsebeam_queue::Receiver<Open>,
    origin: Instant,
}

pub(crate) struct Provisioner(pulsebeam_queue::Sender<Open>);

struct Open {
    id: NetworkId,
    driver: wan::Driver<FabricWanCodec, Window>,
    commands: pulsebeam_queue::Receiver<Command>,
    media: egress::MediaInbox,
}

impl Service {
    pub(crate) async fn bind(
        address: SocketAddr,
        origin: Instant,
    ) -> io::Result<(Self, Provisioner)> {
        if address.ip().is_unspecified() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric WAN requires a concrete local address",
            ));
        }
        let socket = bind_socket(address).await?;
        let (sender, opens) = pulsebeam_queue::bounded(NonZeroUsize::MIN);
        Ok((
            Self {
                socket,
                opens,
                origin,
            },
            Provisioner(sender),
        ))
    }

    #[cfg(test)]
    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr()
    }

    pub(crate) async fn run(self, shutdown: CancellationToken) -> io::Result<()> {
        let Self {
            mut socket,
            mut opens,
            origin,
        } = self;
        loop {
            let request = pulsebeam_runtime::select! {
                _ = shutdown.cancelled() => return Ok(()),
                request = opens.recv() => request,
            };
            let Some(Open {
                id,
                driver,
                commands,
                media,
            }) = request
            else {
                return Ok(());
            };
            let mut host = WanHost::attach(socket, id, origin, driver, LIMITS.active_windows);
            host.commands = Some(commands);
            host.egress.inbox = Some(media);
            let endpoint = host.binding().peer;
            tracing::debug!(epoch = id.epoch(), %endpoint, "Fabric WAN association opened");
            let result = host.drive(&shutdown).await;
            socket = host.socket;
            match result {
                Ok(()) => {}
                Err(Error::Io(error)) => return Err(error),
                Err(error) => {
                    tracing::debug!(epoch = id.epoch(), %endpoint, ?error, "Fabric WAN association closed");
                }
            }
        }
    }
}

impl Provisioner {
    pub(crate) fn open(
        &self,
        peer: NetworkPeer,
        id: NetworkId,
        generation: NonZeroU64,
        origin: Instant,
    ) -> Result<(Commands, PeerControl), LinkError> {
        let (driver, lane) = wan::Driver::with_pinned_peer(
            peer,
            id,
            generation,
            LIMITS,
            FabricWanCodec,
            Instant::now().into_std(),
            origin.into_std(),
        )
        .map_err(|_| LinkError::Security)?;
        Ok((self.install(id, driver)?, lane))
    }

    pub(crate) fn open_media(
        &self,
        peer: NetworkPeer,
        id: NetworkId,
        generation: NonZeroU64,
    ) -> Result<Commands, LinkError> {
        let driver = wan::Driver::media_only(
            peer,
            id,
            generation,
            LIMITS,
            FabricWanCodec,
            Instant::now().into_std(),
        )
        .map_err(|_| LinkError::Security)?;
        self.install(id, driver)
    }

    fn install(
        &self,
        id: NetworkId,
        driver: wan::Driver<FabricWanCodec, Window>,
    ) -> Result<Commands, LinkError> {
        let (port, commands, media) = command_channel(id);
        self.0
            .try_send(Open {
                id,
                driver,
                commands,
                media,
            })
            .map_err(|error| match error {
                pulsebeam_queue::TrySendError::Full(_) => LinkError::Capacity,
                pulsebeam_queue::TrySendError::Closed(_) => LinkError::Closed,
            })?;
        Ok(port)
    }
}
