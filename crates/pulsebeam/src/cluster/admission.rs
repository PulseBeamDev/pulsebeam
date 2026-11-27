use anyhow::{Context, Result};
use std::net::SocketAddr;

use pulsebeam_link::{
    NetworkClass, NetworkPeer, PeerControl, PeerFingerprint, PeerRole, SecretCredential,
};
use pulsebeam_membership::NodeId;
use pulsebeam_runtime::{
    sync::{CancellationToken, mpsc},
    task,
};

use super::{boot_announcement::VerifiedBoot, generation_floor::Store, mesh::Member};

pub(crate) struct LocalBoot {
    pub(crate) member: Member,
    pub(crate) media: SecretCredential,
    store: Store,
}

#[derive(Clone)]
pub(crate) struct MediaCredentials {
    endpoint: SocketAddr,
    local: SecretCredential,
    role: PeerRole,
}

impl MediaCredentials {
    pub(crate) fn new(
        endpoint: SocketAddr,
        credential: SecretCredential,
        local: NodeId,
        remote: NodeId,
    ) -> Self {
        Self {
            endpoint,
            local: credential,
            role: if local < remote {
                PeerRole::Initiator
            } else {
                PeerRole::Responder
            },
        }
    }

    pub(crate) fn pin(&self, expected_peer: PeerFingerprint) -> NetworkPeer {
        NetworkPeer {
            endpoint: self.endpoint,
            credential: self.local.clone(),
            expected_peer,
            role: self.role,
            class: NetworkClass::Wan,
        }
    }
}

impl LocalBoot {
    pub(super) fn new(member: Member, store: Store) -> Result<Self> {
        let media = SecretCredential::generate().map_err(|error| {
            anyhow::anyhow!("generating ephemeral regional DTLS credential: {error:?}")
        })?;
        Ok(Self {
            member,
            media,
            store,
        })
    }

    pub(crate) async fn admit_sessions(
        self,
        candidates: mpsc::Receiver<BootSession>,
        admitted: mpsc::Sender<AdmittedBoot>,
        discovery: Option<DiscoveryBroker>,
        shutdown: CancellationToken,
    ) -> Result<()> {
        run(self.store, candidates, admitted, discovery, shutdown).await
    }
}

pub(crate) struct BootSession {
    pub(crate) peer: Member,
    pub(crate) media_leaf: PeerFingerprint,
    pub(crate) session: [u8; 32],
    pub(crate) control: PeerControl,
}

/// Only the durable admission owner can construct this proof. Controller code
/// uses the contained control lane only after replacing the old boot's state.
pub(crate) struct AdmittedBoot(BootSession);

impl AdmittedBoot {
    pub(crate) fn peer(&self) -> Member {
        self.0.peer
    }

    pub(crate) fn into_session(self) -> BootSession {
        self.0
    }
}

pub(crate) struct DiscoveryBroker {
    candidates: mpsc::Receiver<VerifiedBoot>,
    admitted: mpsc::Sender<AdmittedDiscovery>,
}

pub(crate) struct DiscoveryPort {
    pub(crate) candidates: mpsc::Sender<VerifiedBoot>,
    pub(crate) admitted: mpsc::Receiver<AdmittedDiscovery>,
}

impl DiscoveryBroker {
    pub(crate) fn channel(capacity: usize) -> (Self, DiscoveryPort) {
        let (candidate_tx, candidates) = mpsc::channel(capacity);
        let (admitted, admitted_rx) = mpsc::channel(capacity);
        (
            Self {
                candidates,
                admitted,
            },
            DiscoveryPort {
                candidates: candidate_tx,
                admitted: admitted_rx,
            },
        )
    }
}

pub(crate) struct AdmittedDiscovery(VerifiedBoot);

impl AdmittedDiscovery {
    pub(crate) fn into_boot(self) -> VerifiedBoot {
        self.0
    }
}

async fn run(
    mut store: Store,
    mut candidates: mpsc::Receiver<BootSession>,
    admitted: mpsc::Sender<AdmittedBoot>,
    mut discovery: Option<DiscoveryBroker>,
    shutdown: CancellationToken,
) -> Result<()> {
    enum Observation {
        Direct(Option<BootSession>),
        Routed(Option<VerifiedBoot>),
    }
    loop {
        let observation = pulsebeam_runtime::select! {
            biased;
            _ = shutdown.cancelled() => return Ok(()),
            candidate = candidates.recv() => Observation::Direct(candidate),
            candidate = async {
                match &mut discovery {
                    Some(broker) => broker.candidates.recv().await,
                    None => std::future::pending().await,
                }
            } => Observation::Routed(candidate),
        };
        let candidate = match observation {
            Observation::Direct(Some(candidate)) => candidate,
            Observation::Direct(None) => return Ok(()),
            Observation::Routed(None) => {
                discovery = None;
                continue;
            }
            Observation::Routed(Some(proof)) => {
                let Some(slot) = discovery
                    .as_ref()
                    .and_then(|broker| broker.admitted.try_reserve().ok())
                else {
                    continue;
                };
                let (next, allowed, proof) = task::spawn_blocking(move || {
                    let permitted = store.discover(&proof);
                    (store, permitted, proof)
                })
                .await
                .context("joining regional discovery persistence")?;
                store = next;
                if allowed? {
                    slot.send(AdmittedDiscovery(proof));
                }
                continue;
            }
        };
        // Even if the lane closed while queued, remember an authenticated newer
        // boot. Its predecessor must not become eligible again after a connect flap.
        let peer = candidate.peer;
        let pin = candidate.media_leaf;
        let (next, allowed) = task::spawn_blocking(move || {
            let permitted = store.admit(peer.node, peer.generation, pin);
            (store, permitted)
        })
        .await
        .context("joining regional boot persistence")?;
        store = next;
        if allowed? {
            let proof = AdmittedBoot(candidate);
            pulsebeam_runtime::select! {
                _ = shutdown.cancelled() => return Ok(()),
                result = admitted.send(proof) => {
                    if result.is_err() { return Ok(()); }
                }
            }
        }
    }
}

#[cfg(all(test, feature = "sim"))]
pub(crate) mod fixture {
    use std::num::NonZeroU64;

    use super::*;
    use crate::cluster::identity::Identity;

    pub(crate) struct State {
        identity: Identity,
        path: tempfile::TempDir,
    }

    impl State {
        pub(crate) fn new() -> Result<Self> {
            let path = tempfile::tempdir()?;
            let identity = Identity::allocate(path.path())?;
            Ok(Self { identity, path })
        }

        pub(crate) fn node(&self) -> NodeId {
            self.identity.node_id
        }

        pub(crate) fn discovery(
            &self,
            epoch: [u8; 16],
            direct: &[NodeId],
            other: &[NodeId],
        ) -> Result<(Journal, DiscoveryPort)> {
            let slots = |nodes: &[NodeId]| {
                nodes
                    .iter()
                    .copied()
                    .map(|node| (node, NonZeroU64::MIN))
                    .collect::<Vec<_>>()
            };
            let store = Store::enroll(
                self.path.path(),
                &self.identity,
                epoch,
                slots(direct),
                slots(other),
            )?;
            let (broker, port) = DiscoveryBroker::channel(8);
            Ok((Journal { store, broker }, port))
        }
    }

    pub(crate) struct Journal {
        store: Store,
        broker: DiscoveryBroker,
    }

    impl Journal {
        pub(crate) fn drive(&mut self) -> Result<()> {
            while let Ok(candidate) = self.broker.candidates.try_recv() {
                if self.store.discover(&candidate)? {
                    assert!(
                        self.broker
                            .admitted
                            .try_send(AdmittedDiscovery(candidate))
                            .is_ok()
                    );
                }
            }
            Ok(())
        }
    }
}

#[cfg(all(test, feature = "cluster-tests"))]
mod tests {
    use std::num::{NonZeroU64, NonZeroUsize};

    use crate::cluster::{REGIONAL_FRAME_BYTES, identity::Identity};
    use pulsebeam_link::StreamControl;

    use super::*;

    #[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
    async fn broker_publishes_only_durable_exact_boots() {
        let dir = tempfile::tempdir().unwrap();
        let identity = Identity::allocate(dir.path()).unwrap();
        let node = NodeId::new();
        let routed = NodeId::new();
        let epoch = [8; 16];
        let store = Store::enroll(
            dir.path(),
            &identity,
            epoch,
            [(node, NonZeroU64::MIN)],
            [(routed, NonZeroU64::MIN)],
        )
        .unwrap();
        let (tx, rx) = mpsc::channel(1);
        let (admitted, mut accepted) = mpsc::channel(1);
        let shutdown = CancellationToken::new();
        let (discovery, mut port) = DiscoveryBroker::channel(1);
        let broker = task::spawn(run(store, rx, admitted, Some(discovery), shutdown));
        let candidate = |generation, pin| {
            let (control, stream) =
                StreamControl::new_authenticated(NonZeroUsize::new(REGIONAL_FRAME_BYTES).unwrap());
            let input = BootSession {
                peer: Member {
                    node,
                    generation: NonZeroU64::new(generation).unwrap(),
                },
                media_leaf: PeerFingerprint::from_bytes([pin; 32]),
                session: [pin; 32],
                control,
            };
            (input, stream)
        };
        let (current, _stream) = candidate(5, 5);
        tx.send(current).await.ok().unwrap();
        assert_eq!(
            accepted
                .recv()
                .await
                .unwrap()
                .into_session()
                .peer
                .generation
                .get(),
            5
        );
        use crate::cluster::boot_announcement::{Announcement, Scope};
        let key = pulsebeam_core::auth::ApiSigningKey::from_seed([17; 32]);
        let scope = Scope {
            epoch,
            roster: [7; 32],
        };
        let discovered = Member {
            node: routed,
            generation: NonZeroU64::new(8).unwrap(),
        };
        let frame = Announcement::encode(scope, discovered, 2000, &key);
        let proof = Announcement::decode(&frame)
            .unwrap()
            .verify(scope, key.public_key(), 2000)
            .unwrap();
        assert!(port.candidates.send(proof).await.is_ok());
        assert_eq!(
            port.admitted.recv().await.unwrap().into_boot().origin,
            discovered
        );
        assert!(matches!(
            accepted.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        let (stale, _stream) = candidate(4, 4);
        tx.send(stale).await.ok().unwrap();
        let (wrong_pin, _stream) = candidate(5, 9);
        tx.send(wrong_pin).await.ok().unwrap();
        let (reconnect, _stream) = candidate(5, 5);
        tx.send(reconnect).await.ok().unwrap();
        drop(tx);
        let accepted_pin = accepted
            .recv()
            .await
            .unwrap()
            .into_session()
            .media_leaf
            .to_bytes();
        assert_eq!(accepted_pin, [5; 32]);
        broker.await.unwrap().unwrap();
        assert!(accepted.recv().await.is_none());
    }
}
