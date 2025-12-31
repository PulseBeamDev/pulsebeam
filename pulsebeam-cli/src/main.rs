use pulsebeam_agent::{
    MediaFrame, MediaKind, MediaTime, TransceiverDirection,
    actor::{AgentBuilder, AgentEvent, TrackReceiver, TrackSender},
    media::H264Looper,
    signaling::HttpSignalingClient,
};
use pulsebeam_core::net::UdpSocket;
use std::time::Duration;

const RAW_H264: &[u8] = include_bytes!("video.h264");

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let signaling = HttpSignalingClient::default();
    let socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
    let mut agent = AgentBuilder::new(signaling, socket)
        .with_track(MediaKind::Video, TransceiverDirection::SendOnly, None)
        .join("demo")
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
    tokio::spawn(async move {
        let mut looper = H264Looper::new(RAW_H264);
        let frame_duration_90khz = 90000 / 30;
        let mut ticker = tokio::time::interval(Duration::from_nanos(1_000_000_000 / 30));
        let mut current_ts: u64 = 0;

        loop {
            let now = ticker.tick().await;
            let frame_data = looper.next().unwrap();
            let frame = MediaFrame {
                ts: MediaTime::from_90khz(current_ts),
                data: frame_data,
                capture_time: now,
            };

            sender.try_send(frame);
            current_ts += frame_duration_90khz;
        }
    });
}

fn handle_receiver(_receiver: TrackReceiver) {
    // TODO:
}
