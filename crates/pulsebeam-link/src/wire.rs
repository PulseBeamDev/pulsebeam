//! Link's application payload limit within the initial DTLS wire profile.

pub(crate) const PAYLOAD_BUDGET: usize = pulsebeam_transport::PAYLOAD_BUDGET;
#[cfg(test)]
pub(crate) const UDP_BUDGET: usize = pulsebeam_transport::UDP_BUDGET;
