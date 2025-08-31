use std::{sync::Arc, time::Duration};

use crate::entity;

pub fn create_participant() -> (Arc<entity::ParticipantId>, Box<str0m::Rtc>) {
    let external = entity::ExternalParticipantId::new(entity::new_random_id("tp", 10)).unwrap();
    let participant_id = entity::ParticipantId::new(external);
    let participant_id = Arc::new(participant_id);
    let rtc = str0m::Rtc::new();

    (participant_id, Box::new(rtc))
}

pub fn create_room(name: &str) -> Arc<entity::RoomId> {
    Arc::new(entity::RoomId::try_from(name).unwrap())
}

pub fn create_sim<'a>() -> turmoil::Sim<'a> {
    let tick = Duration::from_millis(100);
    turmoil::Builder::new()
        .tick_duration(tick)
        .ip_version(turmoil::IpVersion::V4)
        .build()
}
