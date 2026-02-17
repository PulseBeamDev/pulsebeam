use std::time::Duration;

use pulsebeam_agent::{
    MediaKind, SimulcastLayer, TransceiverDirection,
    actor::{AgentBuilder, AgentEvent, LocalTrack},
    api::HttpApiClient,
    media::H264Looper,
};
use pulsebeam_core::net::UdpSocket;
use tokio::task::JoinSet;

const RAW_H264: &[u8] = include_bytes!("video.h264");

fn main() {
    tracing_subscriber::fmt::init();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(main_loop());
}

async fn main_loop() {
    let http_client = Box::new(reqwest::Client::new());
    let api = HttpApiClient::new(http_client, "http://localhost:3000").unwrap();
    let socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
    let mut join_set = JoinSet::new();
    let mut agent = AgentBuilder::new(api, socket)
        .with_track(
            MediaKind::Video,
            TransceiverDirection::SendOnly,
            Some(vec![
                SimulcastLayer::new("f"),
                SimulcastLayer::new("h"),
                SimulcastLayer::new("q"),
            ]),
        )
        .connect("demo")
        .await
        .unwrap();
    let mut interval = tokio::time::interval(Duration::from_secs(5));

    loop {
        tokio::select! {
            Some(event) = agent.next_event() => {
                match event {
                    AgentEvent::LocalTrackAdded(track) => {
                        join_set.spawn(handle_local_track(track));
                    }
                    AgentEvent::RemoteTrackAdded(_recv) => {
                    }
                    _ => {}
                }
            }

            _ = interval.tick() => {
                if let Some(stats) = agent.get_stats().await {
                    tracing::info!("stats: {:?}", stats);
                }
            }
        }
    }
}

async fn handle_local_track(track: LocalTrack) {
    tracing::info!("handling: {}(rid={:?})", track.mid, track.rid);
    let looper = H264Looper::new(RAW_H264, 30);
    looper.run(track).await;
}
