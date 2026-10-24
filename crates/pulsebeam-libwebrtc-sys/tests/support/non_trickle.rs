use std::{thread, time::Duration};

use pulsebeam_webrtc_sys::{
    IceGatheringState, ManualClock, PeerConnection, PeerConnectionEvent, SessionDescription,
    SimulatedNetwork,
};

/// Wait for native ICE gathering, then send the *gathered* SDP rather than
/// forwarding per-candidate events. Preserve unrelated events for the caller.
pub fn gathered_local_description(
    peer: &PeerConnection,
    clock: &ManualClock,
    network: &SimulatedNetwork,
    events: &mut Vec<PeerConnectionEvent>,
) -> SessionDescription {
    gathered_local_description_with_progress(peer, events, || {
        while let Some(packet) = network.next_packet() {
            network.deliver(packet.id).unwrap();
        }
        clock.advance(Duration::from_millis(1)).unwrap();
        // Threaded SDK queues use real timers as well as the simulated clock.
        thread::sleep(Duration::from_millis(1));
    })
}

pub fn gathered_local_description_with_progress(
    peer: &PeerConnection,
    events: &mut Vec<PeerConnectionEvent>,
    mut progress: impl FnMut(),
) -> SessionDescription {
    let mut complete = events.iter().any(|event| {
        matches!(
            event,
            PeerConnectionEvent::IceGatheringStateChanged(IceGatheringState::Complete)
        )
    });
    for _ in 0..10_000 {
        while let Some(event) = peer.try_next_event() {
            complete |= matches!(
                event,
                PeerConnectionEvent::IceGatheringStateChanged(IceGatheringState::Complete)
            );
            if !matches!(event, PeerConnectionEvent::IceCandidate(_)) {
                events.push(event);
            }
        }
        if complete {
            let descriptions = peer.descriptions().unwrap();
            let description = descriptions
                .pending_local
                .or(descriptions.current_local)
                .expect("gathering completed without a local description");
            assert!(
                description.sdp.contains("a=candidate:"),
                "gathered SDP must carry the candidates used for non-trickle signaling"
            );
            return description;
        }
        progress();
    }
    panic!(
        "ICE gathering did not complete under the virtual clock: events={events:#?}, descriptions={:?}",
        peer.descriptions()
    );
}
