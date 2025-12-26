use pulsebeam_agent::{
    MediaFrame, MediaTime,
    actor::{AgentBuilder, AgentEvent},
    media::H264Looper,
};
use std::time::Duration;
use tokio::time::Instant;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let raw_bytes = include_bytes!("video.h264");
    tracing::info!("joining");
    let mut agent = AgentBuilder::new("http://localhost:3000", "demo")
        .join()
        .await
        .unwrap();
    tracing::info!("joined");

    while let Some(ev) = agent.next_event().await {
        tracing::info!("received event: {:?}", ev);
        match ev {
            AgentEvent::SenderAdded(sender) => {
                tokio::spawn(async move {
                    let mut looper = H264Looper::new(raw_bytes);
                    let mut interval =
                        tokio::time::interval(Duration::from_nanos(1_000_000_000 / 30));
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
            _ => {}
        }
    }
}
