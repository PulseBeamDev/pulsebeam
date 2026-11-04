use super::*;
use pulsebeam_rtc_simulation::EncodedAudioSink;

pub(super) struct NativeAudioReceiver {
    pub(super) peer: WirePeer,
    pub(super) sink: EncodedAudioSink,
    spec: wire::ResourceSpec,
    source: wire::Track,
    binding: wire::TrackBinding,
    ssrc: u32,
}

impl NativeAudioReceiver {
    pub(super) async fn connect(
        host: NativeHost,
        server: IpAddr,
        source: &wire::Track,
    ) -> anyhow::Result<Self> {
        let (mut peer, _) = WirePeer::connect_on(host, server, "native-rtp-listener", true).await?;
        let input = peer._factory.create_encoded_audio_source(1)?;
        let track = peer
            ._factory
            .create_encoded_audio_track("untransmitted-native-listener", &input)?;
        let transceiver = peer
            .peer
            .add_audio_transceiver(&track, RtpTransceiverDirection::ReceiveOnly)?;
        let opus = peer
            .peer
            .audio_receiver_capabilities()?
            .into_iter()
            .find(|codec| codec.name().eq_ignore_ascii_case("opus"))
            .context("native Opus decoder capability")?;
        transceiver.set_audio_codec_preferences(&[opus])?;
        let offer = peer
            .operation(peer.peer.create_offer()?)
            .await?
            .context("Native receive offer")?;
        peer.operation(peer.peer.set_local_description(offer)?)
            .await?;
        let offer = peer
            .peer
            .descriptions()?
            .pending_local
            .context("applied Native receive offer")?;
        let media = client::inspect(&offer.sdp)?
            .into_iter()
            .find(|media| media.kind == client::Kind::Audio)
            .context("actual Native receive MID")?;
        let receive_offer = peer
            .media
            .as_ref()
            .context("persistent receive context")?
            .receive_offer(&offer.sdp, &media.mid)?;
        // The raw wire and SDK use separate Bazel feature variants of the same schema.
        let receive_offer =
            wire::RtpReceiveOffer::decode(receive_offer.encode_to_vec().as_slice())?;
        let spec = wire::ResourceSpec {
            settings: Some(wire::ResourceSettings {
                enabled: Some(true),
                receive: Some(wire::resource_settings::Receive::Audio(Default::default())),
            }),
            declaration: Some(wire::resource_spec::Declaration::Subscription(
                wire::SubscriptionSpec {
                    track_id: source.track_id.clone(),
                    kind: wire::TrackKind::Audio as i32,
                    binding: None,
                    rtp_offer: Some(receive_offer),
                },
            )),
        };
        validation::validate_spec(&spec)
            .map_err(|error| anyhow::anyhow!("actual Native receive spec: {error:?}"))?;
        peer.desire(30, 30, Some(spec.clone()));
        let preparing = peer.confirmed(30, 30).await?;
        anyhow::ensure!(
            preparing.rejection.is_none(),
            "Native receive offer rejected: {preparing:?}"
        );
        let Some(wire::confirmed_state::Value::Owned(live)) = preparing.value else {
            anyhow::bail!("Native receive plan absent")
        };
        anyhow::ensure!(
            live.spec.as_ref() == Some(&spec)
                && live.track.as_ref() == Some(source)
                && live.execution == wire::ExecutionStatus::Preparing as i32
                && live.activity.is_none(),
            "Native receive plan/source mismatch: {live:?}"
        );
        let binding = live.binding.context("authoritative receive binding")?;
        let Some(wire::track_binding::Transport::Rtp(selected)) = &binding.transport else {
            anyhow::bail!("Native receive plan is not RTP")
        };
        let selected =
            pulsebeam_agent_core::RtpParameters::decode(selected.encode_to_vec().as_slice())?;
        let candidate = peer.media.as_mut().unwrap().prepare_receive_plan(
            &offer.sdp,
            client::MediaOwner {
                key: client::MediaKey("listening".into()),
                group: None,
            },
            &selected,
        )?;
        peer.operation(peer.peer.set_remote_description(SessionDescription {
            kind: SessionDescriptionType::Answer,
            sdp: candidate.answer().into(),
        })?)
        .await?;
        let descriptions = peer.peer.descriptions()?;
        let observed = peer.media.as_ref().unwrap().observe_receive_answer(
            &descriptions
                .remote()
                .context("completed Native receive answer")?
                .sdp,
            &media.mid,
        )?;
        let actual = transceiver.receiver().get_parameters();
        anyhow::ensure!(
            actual.codecs().iter().any(|codec| codec.payload_type()
                == i32::from(observed.codec.payload_type)
                && codec
                    .mime_type_bytes()
                    .is_some_and(|mime| mime.eq_ignore_ascii_case(b"audio/opus"))),
            "actual Native receive codec"
        );
        let extensions = actual.header_extensions();
        anyhow::ensure!(
            observed
                .extensions
                .iter()
                .all(|expected| (0..extensions.len()).any(|index| extensions
                    .get(index)
                    .is_some_and(
                        |extension| extension.id().to_int() == i32::from(expected.id)
                            && extension.uri_bytes() == expected.uri.as_bytes()
                    ))),
            "actual Native receive extensions"
        );
        let observed = wire::TrackBinding {
            transport: Some(wire::track_binding::Transport::Rtp(parameters(&observed)?)),
        };
        anyhow::ensure!(
            validation::bindings_equivalent(Some(&observed), Some(&binding)),
            "actual Native receive binding"
        );
        peer.media
            .as_mut()
            .unwrap()
            .finish(candidate.id(), client::ApplyOutcome::Applied)?;
        let ssrc = selected
            .encodings
            .first()
            .and_then(|encoding| encoding.ssrc)
            .context("server-selected receiving SSRC")?;
        let sink = transceiver.receiver().attach_encoded_audio_sink()?;
        let report = codec::encode_client(&wire::ClientStateBatch {
            records: vec![wire::ClientStateRecord {
                owned: 30,
                desired: None,
                execution: Some(wire::ExecutionReport {
                    revision: 31,
                    outcome: Some(wire::execution_report::Outcome::Installed(observed)),
                }),
            }],
        })
        .map_err(|error| anyhow::anyhow!("Native receive report: {error:?}"))?;
        anyhow::ensure!(
            peer.control.send(DataChannelMessage::binary(report)) == DataChannelSendResult::Sent,
            "Native receive report send"
        );
        let mut receiver = Self {
            peer,
            sink,
            spec,
            source: source.clone(),
            binding,
            ssrc,
        };
        receiver.state(30, wire::ExecutionStatus::Installed).await?;
        Ok(receiver)
    }

    async fn state(
        &mut self,
        revision: u64,
        execution: wire::ExecutionStatus,
    ) -> anyhow::Result<()> {
        loop {
            let value = self.peer.confirmed(30, revision).await?;
            anyhow::ensure!(
                value.rejection.is_none(),
                "Native RTP receive rejected: {value:?}"
            );
            let Some(wire::confirmed_state::Value::Owned(live)) = value.value else {
                anyhow::bail!("Native RTP receiver disappeared")
            };
            anyhow::ensure!(
                live.spec.as_ref() == Some(&self.spec) && live.track.as_ref() == Some(&self.source),
                "Native receive accepted source/spec mismatch"
            );
            anyhow::ensure!(live.activity.is_none(), "receiver invented source activity");
            anyhow::ensure!(
                validation::bindings_equivalent(live.binding.as_ref(), Some(&self.binding)),
                "Native receive plan changed within its owned lifetime"
            );
            if live.execution == execution as i32 {
                return Ok(());
            }
            anyhow::ensure!(
                live.execution != wire::ExecutionStatus::Failed as i32,
                "Native receiver failed: {live:?}"
            );
        }
    }

    pub(super) async fn enabled(&mut self, revision: u64, enabled: bool) -> anyhow::Result<()> {
        self.spec.settings.as_mut().unwrap().enabled = Some(enabled);
        self.peer.desire(30, revision, Some(self.spec.clone()));
        self.state(
            revision,
            if enabled {
                wire::ExecutionStatus::Installed
            } else {
                wire::ExecutionStatus::Disabled
            },
        )
        .await
    }

    pub(super) fn frames(&self, minimum: usize) -> anyhow::Result<()> {
        let mut count = 0;
        while let Some(frame) = self.sink.try_next_frame() {
            anyhow::ensure!(
                frame.data == [0xf8, 0xff, 0xfe]
                    && frame.ssrc == self.ssrc
                    && frame.samples_per_channel == 960
                    && frame.audio_level_dbov == Some(127)
                    && frame.voice_activity == Some(false),
                "real Native receive payload/SSRC/Opus facts"
            );
            count += 1;
        }
        anyhow::ensure!(
            count >= minimum,
            "Native RTP received only {count} frames, expected at least {minimum}"
        );
        Ok(())
    }

    pub(super) async fn absent(
        &mut self,
        revision: u64,
        reason: wire::AbsenceReason,
    ) -> anyhow::Result<()> {
        loop {
            let value = self.peer.confirmed(30, revision).await?;
            anyhow::ensure!(value.rejection.is_none(), "Native receive absence rejected");
            if let Some(wire::confirmed_state::Value::Absent(absent)) = value.value {
                anyhow::ensure!(
                    absent.reason == reason as i32,
                    "Native receiver cutoff revision={revision}: expected={reason:?}, actual={:?}",
                    wire::AbsenceReason::try_from(absent.reason)
                );
                return Ok(());
            }
        }
    }
}
