use pulsebeam_agent::{
    MediaFrame, MediaTime,
    actor::{AgentBuilder, AgentEvent},
    media::H264Looper,
};
use std::time::Duration;

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
                    let mut interval = tokio::time::interval(Duration::from_millis(33));
                    let mut ts = 0;

                    loop {
                        interval.tick().await;
                        let Some(frame) = looper.next() else {
                            break;
                        };

                        let frame = MediaFrame {
                            ts: MediaTime::from_90khz(ts),
                            data: frame,
                        };
                        sender.try_send(frame);
                        ts += 3000;
                    }
                });
            }
            _ => {}
        }
    }
}
