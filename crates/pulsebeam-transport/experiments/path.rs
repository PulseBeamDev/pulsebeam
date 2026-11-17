//! Production UDP/QUIC session construction shared with protocol controls.
use super::*;
#[derive(Clone, Copy)]
pub(super) enum MediaPath {
    Udp,
    Quic { cc: QuicCongestion, overlay: bool },
}
impl MediaPath {
    #[cfg(test)]
    pub(super) fn overlay(self) -> bool {
        match self {
            Self::Udp => true,
            Self::Quic { overlay, .. } => overlay,
        }
    }
    pub(super) fn inner_cc(self) -> &'static str {
        match self {
            Self::Udp => "none",
            Self::Quic {
                cc: QuicCongestion::OuterControlled,
                ..
            } => "outer_controlled",
            Self::Quic {
                cc: QuicCongestion::Cubic,
                ..
            } => "cubic",
            Self::Quic {
                cc: QuicCongestion::Bbr3,
                ..
            } => "bbr3",
        }
    }
    pub(super) fn sessions(self, origin: Instant, addresses: [SocketAddr; 2]) -> [Session; 2] {
        let role = |who| {
            if who == 0 {
                PeerRole::Initiator
            } else {
                PeerRole::Responder
            }
        };
        match self {
            Self::Udp => {
                let credentials = [
                    pulsebeam_transport::Credential::generate().unwrap(),
                    pulsebeam_transport::Credential::generate().unwrap(),
                ];
                std::array::from_fn(|who| {
                    Session::dtls(
                        pulsebeam_transport::Peer {
                            endpoint: addresses[1 - who],
                            credential: credentials[who].clone(),
                            expected_peer: credentials[1 - who].fingerprint(),
                            role: role(who),
                            class: NetworkClass::Wan,
                        },
                        origin,
                    )
                })
            }
            Self::Quic { cc, overlay } => {
                let credentials = [
                    QuicCredential::generate().unwrap(),
                    QuicCredential::generate().unwrap(),
                ];
                std::array::from_fn(|who| {
                    Session::new(
                        QuicPeer {
                            endpoint: addresses[1 - who],
                            credential: credentials[who].clone(),
                            expected_peer: credentials[1 - who].fingerprint(),
                            role: role(who),
                            class: NetworkClass::Wan,
                            tuning: if cc == QuicCongestion::OuterControlled {
                                QuicTuning::default()
                            } else {
                                QuicTuning::webtransport_like(cc).unwrap()
                            },
                        },
                        overlay,
                        origin,
                    )
                })
            }
        }
    }
}
