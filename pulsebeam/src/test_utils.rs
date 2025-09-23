use std::{net::Ipv4Addr, sync::Arc, time::Duration};

use crate::{entity, message::TrackMeta, system, track};
use pulsebeam_runtime::{actor, net};
use str0m::media::Mid;

pub fn create_participant_id() -> Arc<entity::ParticipantId> {
    let participant_id = entity::ParticipantId::new();
    Arc::new(participant_id)
}

pub fn create_participant() -> (Arc<entity::ParticipantId>, str0m::Rtc) {
    let participant_id = create_participant_id();
    let rtc = str0m::Rtc::new();

    (participant_id, rtc)
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
    let socket = net::UnifiedSocket::bind(
        (Ipv4Addr::LOCALHOST, 0).into(),
        net::Transport::SimUdp,
        Some(external_addr),
    )
    .await
    .unwrap();
    let (system_ctx, _) = system::SystemContext::spawn(socket);
    system_ctx
}

pub fn create_track_id() -> Arc<entity::TrackId> {
    let participant_id = create_participant_id();
    Arc::new(entity::TrackId::new(participant_id, Mid::new()))
}
