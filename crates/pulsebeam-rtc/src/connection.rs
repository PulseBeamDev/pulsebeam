use std::{cell::Cell, marker::PhantomData};

use sha2::{Digest, Sha256};

use crate::{
    AcceptError, ConnectionConfig, ConnectionEntropy, NetworkInput, ReceiveError, SdpAnswer,
    SdpOffer, SessionInfo, TimePoint,
    negotiation::{self, NegotiatedSessionFacts},
    time::MonotonicObserver,
    transport::Transport,
};

pub struct Connection {
    _config: ConnectionConfig,
    _session: NegotiatedSessionFacts,
    _time: MonotonicObserver,
    _subsystems: SubsystemSlots,
    _not_sync: PhantomData<Cell<()>>,
}

#[allow(
    dead_code,
    reason = "protocol subsystems are initialized by subsequent plans"
)]
struct SubsystemSlots {
    transport: Transport,
    sctp: Option<()>,
}

pub struct AcceptedConnection {
    pub connection: Connection,
    pub answer: SdpAnswer,
    pub session: SessionInfo,
}

impl Connection {
    #[allow(
        dead_code,
        reason = "the public receive lifecycle is introduced after transport preparation"
    )]
    pub(crate) fn receive(
        &mut self,
        at: TimePoint,
        input: NetworkInput,
    ) -> Result<(), ReceiveError> {
        let at = self._time.observe(at);
        self._subsystems
            .transport
            .receive(at.monotonic, input)
            .map_err(|error| match error {
                crate::transport::TransportError::Closed => ReceiveError::Closed,
                crate::transport::TransportError::QueueFull => ReceiveError::InputLimitExceeded,
                _ => ReceiveError::InvalidNetworkEnvelope,
            })
    }
    pub fn accept(
        config: ConnectionConfig,
        offer: SdpOffer,
        at: TimePoint,
        entropy: ConnectionEntropy,
    ) -> Result<AcceptedConnection, AcceptError> {
        let config = config.validate()?;
        if config.local_candidates.is_empty() {
            return Err(AcceptError::InvalidConfiguration);
        }
        let mut entropy = EntropyConsumer::new(entropy);
        let negotiated = negotiation::negotiate(&config, &offer, at, &mut entropy)?;
        let session = negotiated.session.clone();
        let transport = Transport::from_session(&negotiated.facts, at.monotonic)
            .map_err(|_| AcceptError::CryptographicFailure)?;
        let connection = Self {
            _config: config,
            _session: negotiated.facts,
            _time: MonotonicObserver::starting_at(at),
            _subsystems: SubsystemSlots {
                transport,
                sctp: None,
            },
            _not_sync: PhantomData,
        };
        Ok(AcceptedConnection {
            connection,
            answer: negotiated.answer,
            session,
        })
    }
}

pub(crate) struct EntropyConsumer {
    seed: [u8; 32],
    counter: u32,
}

impl EntropyConsumer {
    pub(crate) fn new(entropy: ConnectionEntropy) -> Self {
        Self {
            seed: entropy.into_bytes(),
            counter: 0,
        }
    }

    pub(crate) fn take<const N: usize>(&mut self, domain: &[u8]) -> [u8; N] {
        let mut output = [0; N];
        for chunk in output.chunks_mut(32) {
            let mut hash = Sha256::new();
            hash.update(b"pulsebeam-rtc-v3\0");
            hash.update(domain);
            hash.update(self.counter.to_be_bytes());
            hash.update(self.seed);
            let digest = hash.finalize();
            for (target, source) in chunk.iter_mut().zip(digest) {
                *target = source;
            }
            self.counter = self.counter.wrapping_add(1);
        }
        output
    }
}

impl Drop for EntropyConsumer {
    fn drop(&mut self) {
        self.seed.fill(0);
    }
}
