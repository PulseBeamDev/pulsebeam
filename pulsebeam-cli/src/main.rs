use pulsebeam_agent::{
    MediaKind, TransceiverDirection,
    actor::{AgentBuilder, AgentEvent, TrackReceiver, TrackSender},
    media::H264Looper,
    signaling::HttpSignalingClient,
};
use pulsebeam_core::net::UdpSocket;

const RAW_H264: &[u8] = include_bytes!("video.h264");

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let http_client = Box::new(reqwest::Client::new());
    let signaling = HttpSignalingClient::new(http_client, "http://localhost:3000");
    let socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
    let mut agent = AgentBuilder::new(signaling, socket)
        .with_track(MediaKind::Video, TransceiverDirection::SendOnly, None)
        .connect("demo")
        .await
        .unwrap();

    while let Some(ev) = agent.next_event().await {
        tracing::info!("received event: {:?}", ev);
        match ev {
            AgentEvent::SenderAdded(sender) => handle_sender(sender),
            AgentEvent::ReceiverAdded(receiver) => handle_receiver(receiver),
            _ => {}
        }
    }
}

fn handle_sender(sender: TrackSender) {
    let looper = H264Looper::new(RAW_H264, 30);
    tokio::spawn(looper.run(sender));
}

fn handle_receiver(_receiver: TrackReceiver) {
    // TODO:
}
