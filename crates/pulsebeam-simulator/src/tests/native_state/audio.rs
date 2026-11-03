use super::*;
use pulsebeam_agent_core::client_sdp as client;
use pulsebeam_rtc_simulation::{
    EncodedAudioSource, OpusAudioLevel, OpusInputFrame, RtpTransceiverDirection,
};
use std::collections::{BTreeMap, BTreeSet};

mod receiver;

const MID: &str = "urn:ietf:params:rtp-hdrext:sdes:mid";
const AUDIO_LEVEL: &str = "urn:ietf:params:rtp-hdrext:ssrc-audio-level";
const TWCC: &str = "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01";

pub(super) fn profile() -> client::Capabilities {
    client::Capabilities {
        codecs: vec![client::CodecCapability {
            repair: false,
            parameters: client::CodecParameters {
                name: "opus".into(),
                clock_rate: 48000,
                channels: Some(2),
                fmtp: BTreeMap::from([
                    ("minptime".into(), "10".into()),
                    ("useinbandfec".into(), "1".into()),
                ]),
                feedback: BTreeSet::from(["transport-cc".into()]),
            },
        }],
        extensions: BTreeSet::from([MID.into(), TWCC.into(), AUDIO_LEVEL.into()]),
        rtcp_modes: BTreeSet::from([client::RtcpMode::Compound, client::RtcpMode::ReducedSize]),
    }
}

fn parameters(binding: &client::Binding) -> anyhow::Result<wire::RtpParameters> {
    anyhow::ensure!(
        binding.complete(),
        "native sender facts incomplete: {binding:?}"
    );
    let codec = &binding.codec;
    anyhow::ensure!(
        codec.parameters.name == "opus"
            && codec.parameters.clock_rate == 48000
            && codec.parameters.channels == Some(2),
        "prepared audio format"
    );
    anyhow::ensure!(
        codec.parameters.fmtp == profile().codecs[0].parameters.fmtp
            && codec.parameters.feedback == profile().codecs[0].parameters.feedback,
        "unexpected prepared Opus parameters: {:?}",
        codec.parameters
    );
    anyhow::ensure!(codec.repair.is_none(), "unexpected audio RTX");
    let format = wire::CodecFormat {
        codec: Some(wire::codec_format::Codec::Opus(wire::OpusParameters {
            min_packet_time_ms: codec
                .parameters
                .fmtp
                .get("minptime")
                .map(|value| value.parse())
                .transpose()?,
            in_band_fec: codec
                .parameters
                .fmtp
                .get("useinbandfec")
                .map(|value| match value.as_str() {
                    "1" => Ok(true),
                    "0" => Ok(false),
                    _ => Err(anyhow::anyhow!("invalid prepared FEC")),
                })
                .transpose()?,
            ..Default::default()
        })),
    };
    let rtp = wire::RtpParameters {
        mid: binding.mid.clone(),
        codec: Some(wire::RtpCodecParameters {
            payload_type: Some(u32::from(codec.payload_type)),
            rtx_payload_type: None,
            feedback: vec![wire::RtcpFeedback::TransportCc as i32],
            format: Some(format),
        }),
        header_extensions: binding
            .extensions
            .iter()
            .map(|extension| {
                Ok(wire::RtpHeaderExtensionParameters {
                    id: u32::from(extension.id),
                    r#type: match extension.uri.as_str() {
                        MID => wire::RtpHeaderExtensionType::RtpHeaderExtensionMid,
                        TWCC => wire::RtpHeaderExtensionType::RtpHeaderExtensionTransportCcDraft01,
                        AUDIO_LEVEL => wire::RtpHeaderExtensionType::RtpHeaderExtensionAudioLevel,
                        _ => anyhow::bail!("unexpected prepared extension {}", extension.uri),
                    } as i32,
                })
            })
            .collect::<anyhow::Result<_>>()?,
        encodings: binding
            .encodings
            .iter()
            .map(|encoding| wire::RtpEncodingParameters {
                ssrc: encoding.primary,
                rtx_ssrc: encoding.repair,
                rid: encoding.rid.clone().unwrap_or_default(),
                scalability_mode: None,
            })
            .collect(),
        rtcp: Some(wire::RtcpParameters {
            cname: binding.rtcp.cname.clone().context("prepared CNAME")?,
            reduced_size: binding.rtcp.reduced_size,
        }),
    };
    Ok(rtp)
}

fn declaration(binding: &client::Binding) -> anyhow::Result<wire::ResourceSpec> {
    let rtp = parameters(binding)?;
    let spec = wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(true),
            receive: None,
        }),
        declaration: Some(wire::resource_spec::Declaration::Publication(
            wire::PublicationSpec {
                label: "microphone".into(),
                kind: Some(wire::publication_spec::Kind::Audio(
                    wire::AudioPublication {
                        binding: Some(wire::TrackBinding {
                            transport: Some(wire::track_binding::Transport::Rtp(rtp)),
                        }),
                    },
                )),
            },
        )),
    };
    validation::validate_spec(&spec)
        .map_err(|error| anyhow::anyhow!("prepared source spec: {error:?}"))?;
    Ok(spec)
}

fn send_frame(source: &EncodedAudioSource) -> anyhow::Result<()> {
    let now = u64::try_from(super::super::common::native::now_us()?)?;
    source.push_opus_at_with_audio_level(
        &OpusInputFrame {
            data: vec![0xf8, 0xff, 0xfe],
            rtp_timestamp: u32::try_from(now / 1000)?.wrapping_mul(48),
            samples_per_channel: 960,
        },
        Duration::from_micros(now),
        OpusAudioLevel::new(127, false)?,
    )?;
    Ok(())
}

async fn push(source: &EncodedAudioSource, count: u32) -> anyhow::Result<()> {
    for _ in 0..count {
        send_frame(source)?;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(60)).await;
    Ok(())
}

async fn drain_in_flight(mut received: impl FnMut() -> bool) {
    // The control ACK fences server egress, not media already queued for playback.
    tokio::time::sleep(Duration::from_millis(100)).await;
    while received() {}
}

async fn silence(
    source: &EncodedAudioSource,
    mut received: impl FnMut() -> bool,
) -> anyhow::Result<()> {
    for tick in 0..300 {
        if tick < 100 && tick % 20 == 0 {
            send_frame(source)?;
        }
        anyhow::ensure!(
            !received(),
            "gated source forwarded media at silence tick {tick}"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    anyhow::ensure!(
        !received(),
        "gated source delivered at end of receive window"
    );
    Ok(())
}

async fn operation(
    peer: &PeerConnection,
    operation: OperationId,
    gathered: &mut bool,
) -> anyhow::Result<Option<SessionDescription>> {
    loop {
        while let Some(event) = peer.try_next_event() {
            match event {
                PeerConnectionEvent::OperationComplete(completion) => {
                    anyhow::ensure!(
                        completion.operation_id == operation,
                        "unexpected receiver operation"
                    );
                    return Ok(completion.result?);
                }
                PeerConnectionEvent::IceGatheringStateChanged(IceGatheringState::Complete) => {
                    *gathered = true;
                }
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

impl WirePeer {
    async fn audio_state(
        &mut self,
        revision: u64,
        execution: wire::ExecutionStatus,
        expected: &wire::ResourceSpec,
        activity: wire::MediaActivity,
    ) -> anyhow::Result<wire::Track> {
        loop {
            let value = self.confirmed(20, revision).await?;
            anyhow::ensure!(value.rejection.is_none(), "audio rejected: {value:?}");
            let Some(wire::confirmed_state::Value::Owned(live)) = value.value else {
                anyhow::bail!("audio source absent")
            };
            anyhow::ensure!(
                live.spec.as_ref() == Some(expected),
                "accepted audio spec differs from prepared declaration"
            );
            anyhow::ensure!(
                live.execution == execution as i32,
                "unexpected audio execution"
            );
            if live.activity == Some(activity as i32) {
                return live.track.context("source descriptor");
            }
        }
    }

    pub(super) async fn prove_audio_publication(
        &mut self,
        server: IpAddr,
        participant: &str,
    ) -> anyhow::Result<()> {
        let source = self._factory.create_encoded_audio_source(1)?;
        let track = self
            ._factory
            .create_encoded_audio_track("native-opus", &source)?;
        let transceiver = self
            .peer
            .add_audio_transceiver(&track, RtpTransceiverDirection::SendOnly)?;
        let mono = self
            .peer
            .audio_sender_capabilities()?
            .into_iter()
            .find(|codec| {
                codec.name().eq_ignore_ascii_case("opus")
                    && codec
                        .parameters()
                        .iter()
                        .any(|parameter| parameter.key == "stereo" && parameter.value == "0")
            })
            .context("native mono Opus capability")?;
        transceiver.set_audio_codec_preferences(&[mono])?;
        let offer = self
            .operation(self.peer.create_offer()?)
            .await?
            .context("audio offer")?;
        self.operation(self.peer.set_local_description(offer)?)
            .await?;
        let offer = self
            .peer
            .descriptions()?
            .pending_local
            .context("applied audio offer")?;
        let media = client::inspect(&offer.sdp)?
            .into_iter()
            .find(|media| media.kind == client::Kind::Audio)
            .context("actual audio MID")?;
        let selected = media
            .codecs
            .iter()
            .find(|codec| codec.parameters.name == "opus")
            .context("actual audio payload")?;
        let candidate = self
            .media
            .as_mut()
            .context("persistent media context")?
            .prepare(
                &offer.sdp,
                &[client::Preparation {
                    mid: media.mid.clone(),
                    payload_type: selected.payload_type,
                    publication: Some(client::MediaOwner {
                        key: client::MediaKey("microphone".into()),
                        group: None,
                    }),
                    reception: None,
                }],
            )?;
        self.operation(self.peer.set_remote_description(SessionDescription {
            kind: SessionDescriptionType::Answer,
            sdp: candidate.answer().into(),
        })?)
        .await?;
        self.media
            .as_mut()
            .unwrap()
            .finish(candidate.id(), client::ApplyOutcome::Applied)?;
        push(&source, 3).await?;
        let mut observed = self
            .media
            .as_ref()
            .unwrap()
            .bindings()
            .context("prepared publication set")?
            .publications[0]
            .encodings
            .clone();
        observed[0].primary = transceiver.sender().get_parameters().encodings()[0].ssrc();
        let actual = transceiver.sender().get_parameters().rtcp();
        anyhow::ensure!(actual.mux(), "actual RTCP mux");
        let cname = actual.cname_bytes();
        let cname = if cname.is_empty() {
            transceiver.applied_local_cname_bytes()
        } else {
            Some(cname)
        }
        .context("actual applied CNAME")?;
        self.media.as_mut().unwrap().observe_sender(
            &media.mid,
            &observed,
            &client::RtcpParameters {
                cname: Some(String::from_utf8(cname)?),
                reduced_size: actual.reduced_size(),
            },
        )?;
        let binding = self
            .media
            .as_ref()
            .unwrap()
            .bindings()
            .context("observed publication set")?
            .publications
            .first()
            .context("observed publication")?
            .clone();
        let spec = declaration(&binding)?;
        self.desire(20, 20, Some(spec.clone()));
        let id = self
            .audio_state(
                20,
                wire::ExecutionStatus::Installed,
                &spec,
                wire::MediaActivity::Idle,
            )
            .await?;
        anyhow::ensure!(
            id.label == "microphone" && id.participant_id == participant && !id.track_id.is_empty(),
            "canonical audio source owner/label/identity"
        );

        let mut native =
            receiver::NativeAudioReceiver::connect(self._host.clone(), server, &id).await?;
        anyhow::ensure!(
            self.audio_state(
                20,
                wire::ExecutionStatus::Installed,
                &spec,
                wire::MediaActivity::Active
            )
            .await?
                == id,
            "Native demand changed source"
        );
        push(&source, 20).await?;
        native.frames(5)?;
        native.enabled(31, false).await?;
        anyhow::ensure!(
            self.audio_state(
                20,
                wire::ExecutionStatus::Installed,
                &spec,
                wire::MediaActivity::Idle
            )
            .await?
                == id,
            "last Native receiver did not remove demand"
        );
        drain_in_flight(|| native.sink.try_next_frame().is_some()).await;
        silence(&source, || native.sink.try_next_frame().is_some())
            .await
            .context("Native demand-off receive window")?;
        native.enabled(32, true).await?;
        anyhow::ensure!(
            self.audio_state(
                20,
                wire::ExecutionStatus::Installed,
                &spec,
                wire::MediaActivity::Active
            )
            .await?
                == id,
            "Native receive resume changed source"
        );

        let mut receiver = self
            ._factory
            .create_peer_connection(PeerConfiguration::default())?;
        let receiver_source = self._factory.create_encoded_audio_source(1)?;
        let receiver_track = self
            ._factory
            .create_encoded_audio_track("untransmitted-listener", &receiver_source)?;
        let receive = receiver
            .add_audio_transceiver(&receiver_track, RtpTransceiverDirection::ReceiveOnly)?;
        let mut gathered = false;
        let offer = operation(&receiver, receiver.create_offer()?, &mut gathered)
            .await?
            .context("WHEP offer")?;
        operation(
            &receiver,
            receiver.set_local_description(offer)?,
            &mut gathered,
        )
        .await?;
        while !gathered {
            while let Some(event) = receiver.try_next_event() {
                if matches!(
                    event,
                    PeerConnectionEvent::IceGatheringStateChanged(IceGatheringState::Complete)
                ) {
                    gathered = true;
                }
            }
            if !gathered {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
        let offer = receiver
            .descriptions()?
            .pending_local
            .context("gathered WHEP offer")?;
        let expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
            + 60;
        let token = pulsebeam_server::sign_participant_token(
            &pulsebeam_server::insecure_dev_api_key()?,
            "native-state-wire",
            "rtp-listener",
            expiry,
        )?;
        let request = http::Request::builder()
            .method("POST")
            .uri(format!("http://{server}:7070/api/v1/whep"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/sdp")
            .body(offer.sdp.into_bytes())?;
        let response = create_http_client()
            .execute(request)
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        anyhow::ensure!(
            response.status() == http::StatusCode::CREATED,
            "WHEP bootstrap {} {}",
            response.status(),
            String::from_utf8_lossy(response.body())
        );
        operation(
            &receiver,
            receiver.set_remote_description(SessionDescription {
                kind: SessionDescriptionType::Answer,
                sdp: String::from_utf8(response.body().to_vec())?,
            })?,
            &mut gathered,
        )
        .await?;
        let sink = receive.receiver().attach_encoded_audio_sink()?;
        // WHEP joins an already-Active source; require media, not a redundant state event.
        push(&source, 20).await?;
        let mut frames = 0;
        while let Some(frame) = sink.try_next_frame() {
            anyhow::ensure!(
                frame.data == [0xf8, 0xff, 0xfe],
                "received encoded Opus payload"
            );
            anyhow::ensure!(
                frame.audio_level_dbov == Some(127)
                    && frame.voice_activity == Some(false)
                    && frame.samples_per_channel == 960,
                "actual negotiated Opus level/V/duration"
            );
            frames += 1;
        }
        anyhow::ensure!(
            frames >= 5,
            "real Native->WHEP Opus received only {frames} frames"
        );

        native.frames(5)?;
        native.enabled(33, false).await?;
        drain_in_flight(|| {
            sink.try_next_frame().is_some() || native.sink.try_next_frame().is_some()
        })
        .await;
        silence(&source, || native.sink.try_next_frame().is_some())
            .await
            .context("Native receive disabled with live WHEP demand")?;
        anyhow::ensure!(
            sink.try_next_frame().is_some(),
            "WHEP stopped with an unrelated Native receiver"
        );
        while sink.try_next_frame().is_some() {}
        native.enabled(34, true).await?;
        push(&source, 10).await?;
        native.frames(1)?;
        while sink.try_next_frame().is_some() {}

        let mut disabled = spec.clone();
        disabled.settings.as_mut().unwrap().enabled = Some(false);
        self.desire(20, 21, Some(disabled.clone()));
        anyhow::ensure!(
            self.audio_state(
                21,
                wire::ExecutionStatus::Disabled,
                &disabled,
                wire::MediaActivity::Idle
            )
            .await?
                == id,
            "disable changed identity"
        );
        drain_in_flight(|| {
            sink.try_next_frame().is_some() || native.sink.try_next_frame().is_some()
        })
        .await;
        silence(&source, || {
            sink.try_next_frame().is_some() || native.sink.try_next_frame().is_some()
        })
        .await
        .context("disabled source receive window")?;
        self.desire(20, 22, Some(spec.clone()));
        anyhow::ensure!(
            self.audio_state(
                22,
                wire::ExecutionStatus::Installed,
                &spec,
                wire::MediaActivity::Active
            )
            .await?
                == id,
            "resume changed identity"
        );
        push(&source, 10).await?;
        anyhow::ensure!(
            sink.try_next_frame().is_some(),
            "resumed source did not deliver"
        );
        native.frames(1)?;
        self.desire(20, 23, None);
        let absent = self.confirmed(20, 23).await?;
        anyhow::ensure!(
            absent.rejection.is_none()
                && matches!(absent.value, Some(wire::confirmed_state::Value::Absent(_))),
            "audio cutoff did not complete"
        );
        drain_in_flight(|| {
            sink.try_next_frame().is_some() || native.sink.try_next_frame().is_some()
        })
        .await;
        silence(&source, || {
            sink.try_next_frame().is_some() || native.sink.try_next_frame().is_some()
        })
        .await
        .context("retired source receive window")?;
        native.absent(34, wire::AbsenceReason::SourceEnded).await?;
        native.peer.desire(30, 35, None);
        native.absent(35, wire::AbsenceReason::Gone).await?;
        native.peer.peer.close()?;
        receiver.close()?;
        Ok(())
    }
}
