use super::*;
use pulsebeam_rtc_simulation::EncodedAudioSink;

pub(super) struct NativeAudioReceiver {
    pub(super) peer: WirePeer,
    pub(super) sink: EncodedAudioSink,
    spec: wire::ResourceSpec,
    source: wire::Track,
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
        let codec = media
            .codecs
            .iter()
            .find(|codec| codec.parameters.name == "opus")
            .context("actual Native receive payload")?;
        let candidate = peer
            .media
            .as_mut()
            .context("persistent receive context")?
            .prepare(
                &offer.sdp,
                &[client::Preparation {
                    mid: media.mid.clone(),
                    payload_type: codec.payload_type,
                    publication: None,
                    reception: Some(client::MediaOwner {
                        key: client::MediaKey("listening".into()),
                        group: None,
                    }),
                }],
            )?;
        peer.operation(peer.peer.set_remote_description(SessionDescription {
            kind: SessionDescriptionType::Answer,
            sdp: candidate.answer().into(),
        })?)
        .await?;
        peer.media
            .as_mut()
            .unwrap()
            .finish(candidate.id(), client::ApplyOutcome::Applied)?;
        let binding = peer
            .media
            .as_ref()
            .unwrap()
            .bindings()
            .context("applied receive bindings")?
            .receptions
            .first()
            .context("actual receive binding")?
            .clone();
        let parameters = parameters(&binding)?;
        let ssrc = parameters
            .encodings
            .first()
            .and_then(|encoding| encoding.ssrc)
            .context("applied receiving SSRC")?;
        let spec = wire::ResourceSpec {
            settings: Some(wire::ResourceSettings {
                enabled: Some(true),
                receive: Some(wire::resource_settings::Receive::Audio(Default::default())),
            }),
            declaration: Some(wire::resource_spec::Declaration::Subscription(
                wire::SubscriptionSpec {
                    track_id: source.track_id.clone(),
                    kind: wire::TrackKind::Audio as i32,
                    binding: Some(wire::TrackBinding {
                        transport: Some(wire::track_binding::Transport::Rtp(parameters)),
                    }),
                },
            )),
        };
        validation::validate_spec(&spec)
            .map_err(|error| anyhow::anyhow!("actual Native receive spec: {error:?}"))?;
        let sink = transceiver.receiver().attach_encoded_audio_sink()?;
        peer.desire(30, 30, Some(spec.clone()));
        let mut receiver = Self {
            peer,
            sink,
            spec,
            source: source.clone(),
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
