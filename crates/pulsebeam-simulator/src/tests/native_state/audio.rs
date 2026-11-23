use super::{
    sdp::{connect_sdp_peer, wait_sdp_dtls},
    *,
};
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
        mime_type: "audio/opus".into(),
        clock_rate: codec.parameters.clock_rate,
        channels: codec.parameters.channels.map(u32::from),
        parameters: codec
            .parameters
            .fmtp
            .iter()
            .map(|(name, value)| wire::CodecParameter {
                name: name.clone(),
                value: value.clone(),
            })
            .collect(),
    };
    let rtp = wire::RtpParameters {
        extmap_allow_mixed: Some(binding.extmap_allow_mixed),
        mid: binding.mid.clone(),
        codec: Some(wire::RtpCodecParameters {
            payload_type: Some(u32::from(codec.payload_type)),
            rtx_payload_type: None,
            feedback: vec![wire::RtcpFeedback::TransportCc.as_str().into()],
            format: Some(format),
        }),
        header_extensions: binding
            .extensions
            .iter()
            .map(|extension| wire::RtpHeaderExtensionParameters {
                id: u32::from(extension.id),
                uri: extension.uri.clone(),
            })
            .collect(),
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
                track_id: "microphone".into(),
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

const SILENCE_OPUS: &[u8] = &[0xf8, 0xff, 0xfe];

fn send_frame(source: &EncodedAudioSource, payload: &[u8]) -> anyhow::Result<()> {
    let now = u64::try_from(super::super::common::native::now_us()?)?;
    source.push_opus_at_with_audio_level(
        &OpusInputFrame {
            data: payload.to_vec(),
            rtp_timestamp: u32::try_from(now / 1000)?.wrapping_mul(48),
            samples_per_channel: 960,
        },
        Duration::from_micros(now),
        OpusAudioLevel::new(127, false)?,
    )?;
    Ok(())
}

async fn push(source: &EncodedAudioSource, count: u32) -> anyhow::Result<()> {
    push_payload(source, count, SILENCE_OPUS).await
}

async fn push_payload(
    source: &EncodedAudioSource,
    count: u32,
    payload: &[u8],
) -> anyhow::Result<()> {
    for _ in 0..count {
        send_frame(source, payload)?;
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
            send_frame(source, SILENCE_OPUS)?;
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
        cross_shard: bool,
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
            id.track_id == "microphone"
                && id.participant_id == participant
                && !id.publication_id.is_empty(),
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
        assert_opus_gcm(&self.peer, &media.mid)?;
        let route = if cross_shard {
            "fabric_rtp_opus_shard_enqueued"
        } else {
            "fabric_rtp_opus_local_enqueued"
        };
        let enqueued = || pulsebeam::sim_metrics::routing_counter(route);
        let before = enqueued();
        let udp = self._host.observe_udp_rtp(server);
        // Opus code 3 with one 20 ms frame and 1,191 bytes of codec padding.
        let mut payload = vec![0xfb, 0x41, 255, 255, 255, 255, 175, 0xff, 0xfe];
        payload.resize(1_200, 0);
        push_payload(&source, 20, &payload).await?;
        let first = native.frames_with_payload(5, &payload)?;
        let peaks = udp.peaks();
        drop(udp);
        anyhow::ensure!(
            enqueued() > before,
            "Native Opus did not use the expected Fabric path: {route}"
        );
        // Both peers carry only the exact constant Opus source in this window.
        // Comparing the largest receiver primary with the smallest publisher
        // primary bounds every corresponding pair without sequence-offset assumptions.
        anyhow::ensure!(
            peaks
                .min_sent
                .is_some_and(|min| (1_217..=min).contains(&peaks.received)),
            "Native protected Opus did not exceed the old cap without growth: {peaks:?}"
        );
        assert_decoded_opus(first)?;
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
        connect_sdp_peer(
            &receiver,
            server,
            &format!("whep?track=native-state-wire.{participant}.audio.microphone"),
            "rtp-listener",
            &["subscribe"],
        )
        .await?;
        let sink = receive.receiver().attach_encoded_audio_sink()?;
        // The explicit WHEP binding selects an already-active source.
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

    pub(super) async fn prove_whip_whep_opus(
        &self,
        server: IpAddr,
        cross_shard: bool,
    ) -> anyhow::Result<()> {
        use pulsebeam_rtc_simulation::{CryptoOptions, RtcConfiguration};

        let peer_config = || {
            let mut config: RtcConfiguration = PeerConfiguration::default().into();
            config.options.crypto_options = CryptoOptions::prefer_gcm();
            config
        };
        let mut publisher = self
            ._factory
            .create_peer_connection_with_configuration(peer_config())?;
        let source = self._factory.create_encoded_audio_source(1)?;
        let track = self
            ._factory
            .create_encoded_audio_track("whip-opus", &source)?;
        let sender = publisher.add_audio_transceiver(&track, RtpTransceiverDirection::SendOnly)?;
        let opus = publisher
            .audio_sender_capabilities()?
            .into_iter()
            .find(|codec| {
                codec.name().eq_ignore_ascii_case("opus")
                    && codec
                        .parameters()
                        .iter()
                        .any(|param| param.key == "stereo" && param.value == "0")
            })
            .context("WHIP Opus capability")?;
        sender.set_audio_codec_preferences(&[opus])?;
        connect_sdp_peer(
            &publisher,
            server,
            "whip?room=native-state-wire&track=audio.whip-opus",
            "whip-speaker",
            &["join", "publish"],
        )
        .await?;
        wait_sdp_dtls(
            &publisher,
            sender.mid().context("WHIP negotiated MID")?.as_bytes(),
        )
        .await?;
        push(&source, 20).await?;

        let mut receiver = self
            ._factory
            .create_peer_connection_with_configuration(peer_config())?;
        let receiver_source = self._factory.create_encoded_audio_source(1)?;
        let receiver_track = self
            ._factory
            .create_encoded_audio_track("whep-opus", &receiver_source)?;
        let receive = receiver
            .add_audio_transceiver(&receiver_track, RtpTransceiverDirection::ReceiveOnly)?;
        let counter = || pulsebeam::sim_metrics::routing_counter("fabric_whep_rtp_enqueued");
        let before = counter();
        connect_sdp_peer(
            &receiver,
            server,
            "whep?track=native-state-wire.whip-speaker.audio.whip-opus",
            "whep-whip-listener",
            &["subscribe"],
        )
        .await?;
        let sink = receive.receiver().attach_encoded_audio_sink()?;
        wait_sdp_dtls(
            &receiver,
            receive.mid().context("WHEP negotiated MID")?.as_bytes(),
        )
        .await?;
        let udp = self._host.observe_udp_rtp(server);
        push(&source, 20).await?;
        let udp = udp.peaks();
        for (peer, mid) in [(&publisher, sender.mid()), (&receiver, receive.mid())] {
            let mid = mid.context("WHIP/WHEP negotiated MID")?;
            let info = peer
                .dtls_transport_by_mid(mid.as_bytes())
                .context("WHIP/WHEP media transport")?
                .information();
            anyhow::ensure!(
                matches!(info.srtp_cipher_suite(), Some(0x0007 | 0x0008)),
                "WHIP/WHEP did not negotiate full-tag GCM"
            );
        }
        let mut first = None;
        let mut frames = 0;
        while let Some(frame) = sink.try_next_frame() {
            anyhow::ensure!(
                frame.data == [0xf8, 0xff, 0xfe] && frame.samples_per_channel == 960,
                "WHIP-to-WHEP Opus contents"
            );
            anyhow::ensure!(
                frame.audio_level_dbov == Some(127) && frame.voice_activity == Some(false),
                "WHIP-to-WHEP Opus audio level"
            );
            first.get_or_insert(frame);
            frames += 1;
        }
        anyhow::ensure!(
            frames >= 5 && counter() > before,
            "WHIP-to-WHEP Fabric route (cross_shard={cross_shard}): {frames} frames, {} Fabric enqueues, UDP {udp:?}",
            counter().saturating_sub(before)
        );
        anyhow::ensure!(
            udp.min_sent
                .is_some_and(|min| (31..=min).contains(&udp.received)),
            "protected WHIP/WHEP Opus output grew: {udp:?}",
        );

        assert_decoded_opus(first.context("first authenticated WHEP Opus frame")?)?;
        receiver.close()?;
        publisher.close()?;
        Ok(())
    }
}

fn assert_opus_gcm(peer: &PeerConnection, mid: &str) -> anyhow::Result<()> {
    let transport = peer
        .dtls_transport_by_mid(mid.as_bytes())
        .context("installed Opus transport")?;
    anyhow::ensure!(
        matches!(
            transport.information().srtp_cipher_suite(),
            Some(0x0007 | 0x0008)
        ),
        "Opus did not negotiate full-tag GCM"
    );
    Ok(())
}

fn assert_decoded_opus(first: pulsebeam_rtc_simulation::EncodedAudioFrame) -> anyhow::Result<()> {
    use pulsebeam_rtc_simulation::{AudioDecoderFactory, Environment, SdpAudioFormat};
    let env = Environment::builder().build()?;
    let mut format = SdpAudioFormat::new(b"opus".to_vec(), 48_000, 2);
    format.parameters.insert(b"stereo".to_vec(), b"0".to_vec());
    let decoder = AudioDecoderFactory::builtin_opus()?
        .create(&env, &format)?
        .context("stock mono Opus decoder")?;
    let decoded = decoder
        .parse_payload(first.data, first.rtp_timestamp)?
        .into_iter()
        .next()
        .and_then(|packet| packet.frame)
        .context("Opus access unit")?
        .decode()?
        .context("fresh PCM from Opus")?;
    anyhow::ensure!(decoded.samples.len() == 960, "whole 20 ms Opus frame");
    Ok(())
}
