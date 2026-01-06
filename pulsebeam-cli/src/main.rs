use std::time::Duration;

use pulsebeam_agent::{
    MediaKind, TransceiverDirection,
    actor::{AgentBuilder, AgentEvent},
    media::H264Looper,
    signaling::HttpSignalingClient,
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
    let signaling = HttpSignalingClient::new(http_client, "http://localhost:3000");
    let socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
    let mut join_set = JoinSet::new();
    let mut agent = AgentBuilder::new(signaling, socket)
        .with_track(MediaKind::Video, TransceiverDirection::SendOnly, None)
        .connect("demo")
        .await
        .unwrap();
    let mut interval = tokio::time::interval(Duration::from_secs(5));

    loop {
        tokio::select! {
            Some(event) = agent.next_event() => {
                match event {
                    AgentEvent::SenderAdded(sender) => {
                        let looper = H264Looper::new(RAW_H264, 30);
                        join_set.spawn(looper.run(sender));
                    }
                    AgentEvent::ReceiverAdded(_recv) => {
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
