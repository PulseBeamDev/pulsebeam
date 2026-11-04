mod video;

use super::*;
use crate::participant::core::native_data::tests::frame_for;
use crate::rtp::{AUDIO_FREQUENCY, MediaTime, RtpPacket};

fn receiver_spec(track: TrackId, mid: &str, ssrc: u32, enabled: bool) -> wire::ResourceSpec {
    let mut source = spec(enabled);
    source.settings.as_mut().unwrap().receive =
        Some(wire::resource_settings::Receive::Audio(Default::default()));
    let Some(resource_spec::Declaration::Publication(publication)) = source.declaration.take()
    else {
        panic!("source fixture")
    };
    let Some(wire::publication_spec::Kind::Audio(audio)) = publication.kind else {
        panic!("audio fixture")
    };
    let Some(wire::track_binding::Transport::Rtp(mut parameters)) =
        audio.binding.unwrap().transport
    else {
        panic!("RTP fixture")
    };
    parameters.mid = mid.into();
    parameters.encodings[0].ssrc = Some(ssrc);
    parameters.rtcp.as_mut().unwrap().cname = "component-receiver-cname".into();
    source.declaration = Some(resource_spec::Declaration::Subscription(
        wire::SubscriptionSpec {
            track_id: track.as_str(),
            kind: wire::TrackKind::Audio as i32,
            binding: Some(wire::TrackBinding {
                transport: Some(wire::track_binding::Transport::Rtp(parameters)),
            }),
        },
    ));
    source
}

fn desire(
    participant: &mut Participant,
    sink: &mut MockParticipantSink,
    reference: u64,
    revision: u64,
    spec: Option<wire::ResourceSpec>,
) {
    let value = spec.map_or(wire::desired_state::Value::Absent(true), |spec| {
        wire::desired_state::Value::Present(Box::new(spec))
    });
    participant.handle_native_signaling(&frame_for(reference, revision, value), sink);
    for _ in 0..16 {
        if !participant.advance_native_source(sink) {
            break;
        }
    }
    assert!(participant.disconnect_reason.is_none());
}

pub(super) fn check(source: &Track) {
    let mut participant =
        participant_with_capabilities(native_data::unix_now() + 60, Some(capabilities()));
    let mut sink = MockParticipantSink::new();
    let id = source.id();
    participant
        .signaling
        .observe_participant(wire::Participant {
            participant_id: source.meta().origin.as_str(),
            participant_external_id: "source".into(),
        })
        .unwrap();
    let mut handles = slotmap::SlotMap::<TrackHandle, _>::with_key();
    let handle = handles.insert(id);
    participant.add_track_candidate(handle, source.clone());
    let first = receiver_spec(id, "listen", 321, true);
    pulsebeam_proto::state::validation::validate_spec(&first).expect("valid receiving fixture");
    desire(&mut participant, &mut sink, 1, 1, Some(first.clone()));
    let (_, live) = last_live(&mut participant);
    assert_eq!(live.spec, Some(first.clone()));
    assert_eq!(live.track, track_descriptor(source));
    assert_eq!(
        live.participant.unwrap().participant_id,
        source.meta().origin.as_str()
    );
    assert_eq!(
        live.execution,
        wire::ExecutionStatus::WaitingForSource as i32
    );
    assert!(live.activity.is_none());
    let mid = Mid::from("listen");
    assert_eq!(participant.transport.stream_tx_ssrc(mid), Some(321.into()));
    assert_eq!(
        participant
            .transport
            .preferred_send_pt(mid, MediaKind::Audio),
        Some(111u8.into())
    );
    assert!(
        !participant
            .native_receivers
            .can_write_rtp(mid, id, 321.into(), 111u8.into())
    );
    participant.native_source_binding(id, handle, true);
    for _ in 0..16 {
        if !participant.advance_native_source(&mut sink) {
            break;
        }
    }
    assert_eq!(
        last_live(&mut participant).1.execution,
        wire::ExecutionStatus::Installed as i32
    );
    desire(
        &mut participant,
        &mut sink,
        1,
        2,
        Some(receiver_spec(id, "changed", 399, true)),
    );
    let (rejected, live) = last_live(&mut participant);
    assert_eq!(
        rejected.rejection.unwrap().code,
        wire::FailureCode::InvalidConfiguration as i32
    );
    assert_eq!(live.spec, Some(first.clone()));
    assert!(
        participant
            .transport
            .stream_tx_ssrc(Mid::from("changed"))
            .is_none()
    );
    assert_eq!(participant.transport.stream_tx_ssrc(mid), Some(321.into()));
    let second = receiver_spec(id, "second", 322, true);
    desire(&mut participant, &mut sink, 3, 3, Some(second));
    confirmations(&mut participant);
    let mut packet = RtpPacket {
        rtp_ts: MediaTime::new(960, AUDIO_FREQUENCY),
        ..Default::default()
    };
    assert!(packet.ext_vals.audio_level.is_none());
    participant.on_track_packet(handle, TrackPacketRef::Rtp(&packet), None);
    let mut outputs = Vec::new();
    while let Some(write) = participant.stream_writer.pop() {
        let StreamWrite::Audio {
            track_id,
            mid,
            ssrc,
            pt,
            ..
        } = write
        else {
            panic!("audio output")
        };
        assert_eq!(track_id, id);
        assert_eq!(pt, 111u8.into());
        outputs.push((mid, ssrc));
    }
    assert_eq!(
        outputs,
        vec![(mid, 321.into()), (Mid::from("second"), 322.into())]
    );
    desire(
        &mut participant,
        &mut sink,
        4,
        4,
        Some(receiver_spec(id, "collision", 322, true)),
    );
    let rejected = native_data::tests::confirmations_for(&mut participant, 4)
        .pop()
        .unwrap();
    assert_eq!(
        rejected.rejection.unwrap().code,
        wire::FailureCode::Incompatible as i32
    );
    assert!(matches!(
        rejected.value,
        Some(wire::confirmed_state::Value::Absent(_))
    ));
    assert!(
        participant
            .transport
            .stream_tx_ssrc(Mid::from("collision"))
            .is_none()
    );
    assert_eq!(
        participant.transport.stream_tx_ssrc(Mid::from("second")),
        Some(322.into())
    );
    video::check(&mut participant, &mut sink, &mut handles, source);
    packet.seq_no = 2u64.into();
    packet.rtp_ts = MediaTime::new(1920, AUDIO_FREQUENCY);
    participant.on_track_packet(handle, TrackPacketRef::Rtp(&packet), None);
    desire(
        &mut participant,
        &mut sink,
        1,
        13,
        Some(receiver_spec(id, "listen", 321, false)),
    );
    assert_eq!(
        last_live(&mut participant).1.execution,
        wire::ExecutionStatus::Disabled as i32
    );
    assert!(
        !participant
            .native_receivers
            .can_write_rtp(mid, id, 321.into(), 111u8.into())
    );
    assert!(
        matches!(participant.stream_writer.pop(), Some(StreamWrite::Audio { mid, .. }) if mid == Mid::from("second"))
    );
    assert!(participant.stream_writer.pop().is_none());
    desire(&mut participant, &mut sink, 1, 14, Some(first));
    assert_eq!(
        last_live(&mut participant).1.execution,
        wire::ExecutionStatus::Installed as i32
    );
    assert!(
        participant
            .native_receivers
            .can_write_rtp(mid, id, 321.into(), 111u8.into())
    );
    desire(&mut participant, &mut sink, 1, 15, None);
    assert!(participant.transport.stream_tx_ssrc(mid).is_none());
    assert!(
        participant
            .transport
            .stream_tx_ssrc(Mid::from("second"))
            .is_some()
    );
    assert_eq!(sink.owned_unsubscribes, vec![5, 7, 1]);
    participant.complete_native_resource_cutoff(1, true);
    assert!(
        confirmations(&mut participant)
            .iter()
            .any(|value| matches!(value.value, Some(wire::confirmed_state::Value::Absent(_))))
    );
    participant.handle_native_signaling(
        &wire::codec::encode_client(&wire::ClientStateBatch {
            records: vec![wire::ClientStateRecord {
                owned: 3,
                desired: None,
                execution: Some(wire::ExecutionReport {
                    revision: 16,
                    failure: Some(failure(wire::FailureCode::InternalFailure)),
                }),
            }],
        })
        .unwrap(),
        &mut sink,
    );
    let failed = last_live_for(&mut participant, 3).1;
    assert_eq!(failed.execution, wire::ExecutionStatus::Failed as i32);
    assert_eq!(failed.spec, Some(receiver_spec(id, "second", 322, true)));
    assert_eq!(
        failed.execution_failure.unwrap().code,
        wire::FailureCode::InternalFailure as i32
    );
    participant.native_source_binding(id, handle, false);
    participant.native_source_binding(id, handle, true);
    for _ in 0..16 {
        if !participant.advance_native_source(&mut sink) {
            break;
        }
    }
    assert!(!participant.native_receivers.can_write_rtp(
        Mid::from("second"),
        id,
        322.into(),
        111u8.into(),
    ));
    participant.native_source_removed(id, crate::participant::effect::TrackRemoval::SourceEnded);
    for _ in 0..16 {
        if !participant.advance_native_source(&mut sink) {
            break;
        }
    }
    assert!(
        participant
            .transport
            .stream_tx_ssrc(Mid::from("second"))
            .is_none()
    );
    assert_eq!(sink.owned_unsubscribes, vec![5, 7, 1, 3]);
    assert!(participant.disconnect_reason.is_none());
}
