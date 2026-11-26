use std::time::Duration;

use pulsebeam_agent_quic::executor::{
    host::{Agent, Error, VideoState},
    timed::{Policy, ReadState, Receive},
};

pub async fn read_camera(
    host: &mut Agent,
    room: &str,
    participant: &str,
    mut process: impl FnMut(Receive),
) -> Result<(), Error> {
    let selector = host.room(room).participant(participant).video("camera");
    let subscription = selector.subscribe(host)?;
    let result = async {
        let binding = loop {
            match subscription.state(host)? {
                VideoState::Waiting | VideoState::Recovering => host.turn().await?,
                VideoState::Usable { binding } => break binding,
                VideoState::Closed => return Ok(()),
                VideoState::Rejected { error } => return Err(error),
            }
        };
        assert_eq!(binding.room(), room);
        assert_eq!(binding.participant_id(), participant);
        assert_eq!(binding.track_id(), "camera");
        assert!(!binding.publication_id().is_empty());

        let overlap = selector.subscribe(host)?;
        overlap.close(host);
        let VideoState::Usable { binding: retained } = subscription.state(host)? else {
            panic!("closing an overlapping owner withdrew an installed binding");
        };
        assert_eq!(retained.incarnation(), binding.incarnation());

        let reader = binding.encoded(
            host,
            Policy::Recording {
                reorder: Duration::from_millis(120),
            },
        )?;
        let result = async {
            loop {
                match reader.next(host) {
                    ReadState::Ready(event) => process(event),
                    ReadState::Idle => host.turn().await?,
                    ReadState::Closed => return Ok(()),
                    ReadState::Failed(error) => return Err(error),
                }
            }
        }
        .await;
        reader.close(host);
        result
    }
    .await;
    subscription.close(host);
    result
}
