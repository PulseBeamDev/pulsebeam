use std::{net::Ipv4Addr, sync::Arc, time::Duration};

use crate::{entity, system};
use pulsebeam_runtime::net;

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

pub async fn create_system_ctx() -> system::SystemContext {
    let external_addr = "192.168.1.1:3478".parse().unwrap();
    let socket = net::UnifiedSocket::bind((Ipv4Addr::LOCALHOST, 0).into(), net::Transport::SimUdp)
        .await
        .unwrap();
    let (system_ctx, _) = system::SystemContext::spawn(external_addr, socket);
    system_ctx
}
