//! Link's application payload limit within the initial DTLS wire profile.

pub(crate) const PAYLOAD_BUDGET: usize = pulsebeam_transport::PAYLOAD_BUDGET;
pub(crate) const FABRIC_IMAGE: usize = pulsebeam_transport::FABRIC_V1_IMAGE_BYTES;
#[cfg(test)]
pub(crate) const UDP_BUDGET: usize = pulsebeam_transport::UDP_BUDGET;
