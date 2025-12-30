use pulsebeam_agent::{
    MediaFrame, MediaKind, MediaTime, TransceiverDirection,
    actor::{AgentBuilder, AgentEvent, TrackReceiver, TrackSender},
    media::H264Looper,
    signaling::HttpSignalingClient,
};
use std::time::Duration;
use tokio::time::Instant;

const RAW_H264: &[u8] = include_bytes!("video.h264");

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let signaling = HttpSignalingClient::default();
    let mut agent = AgentBuilder::new(signaling)
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
        let mut interval = tokio::time::interval(Duration::from_nanos(1_000_000_000 / 30));
        let start = Instant::now();
        loop {
            let now = interval.tick().await;
            let elapsed = now - start;

            // Calculate TS based on actual elapsed time to prevent drift
            let ts = (elapsed.as_secs_f64() * 90000.0) as u64;

            let frame = MediaFrame {
                ts: MediaTime::from_90khz(ts),
                data: looper.next().unwrap(),
            };
            sender.try_send(frame);
        }
    });
}

fn handle_receiver(_receiver: TrackReceiver) {
    // TODO:
}
