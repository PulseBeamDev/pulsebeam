use pulsebeam_core::auth::KeyRegistry;

use crate::control::controller::{ControllerSender, operator};

#[derive(Clone)]
pub struct SimStandbyControl {
    sender: ControllerSender,
    registry: KeyRegistry,
}

impl SimStandbyControl {
    pub(crate) fn new(sender: ControllerSender, registry: KeyRegistry) -> Self {
        Self { sender, registry }
    }

    pub async fn submit(&self, request: &[u8]) -> serde_json::Result<Vec<u8>> {
        operator::reply(request, &self.registry, &self.sender).await
    }
}
