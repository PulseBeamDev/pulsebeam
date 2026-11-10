use crate::negotiation::NegotiatedSessionFacts;
use is::{Candidate, IceCreds};
use std::time::Instant;

pub(crate) use pulsebeam_transport::dtls_srtp::*;
use str0m::crypto::Fingerprint;

pub(crate) fn from_session(
    facts: &NegotiatedSessionFacts,
    limits: crate::ConnectionLimits,
    now: Instant,
) -> Result<Transport, TransportError> {
    let remote_candidates = facts
        .remote_candidates
        .iter()
        .map(|candidate| {
            Candidate::from_sdp_string(candidate).map_err(|_| TransportError::Configuration)
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_boxed_slice();
    let local_ice = IceCreds {
        ufrag: facts.local_ice.ufrag.clone(),
        pass: facts.local_ice.password.clone(),
    };
    let remote_ice = IceCreds {
        ufrag: facts.remote_ice.ufrag.clone(),
        pass: facts.remote_ice.password.clone(),
    };
    let mut config = TransportConfig::from_candidates(
        local_ice,
        facts.local_candidates.clone(),
        remote_ice,
        remote_candidates,
        facts.dtls_identity.clone(),
        Fingerprint {
            hash_func: facts.remote_fingerprint.algorithm.to_owned(),
            bytes: facts.remote_fingerprint.value.to_vec(),
        },
        facts.local_dtls_role,
    )
    .with_rtp_payload_types(
        facts.inbound_payload_types(),
        facts.outbound_payload_types(),
    );
    config.max_output_bytes = limits.max_transport_output_bytes;
    config.replay_limits = ReplayLimits {
        rtp_bytes: limits.max_rtp_replay_state_bytes,
        rtcp_bytes: limits.max_rtcp_replay_state_bytes,
    };
    Transport::new(config, now)
}
