use super::*;
use pulsebeam_agent_core::client_sdp as client;
use pulsebeam_rtc_simulation::{
    CryptoOptions, MediaType, RtcConfiguration, RtpHeaderExtensionDirection,
    RtpTransceiverDirection, VideoEncoderFactoryHandle, VideoFrame,
};

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

pub(super) async fn connect_sdp_peer(
    peer: &PeerConnection,
    server: IpAddr,
    endpoint: &str,
    identity: &str,
    permissions: &[&str],
) -> anyhow::Result<()> {
    let expiry = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs()
        + 60;
    let token = pulsebeam_server::sign_participant_token(
        &pulsebeam_server::insecure_dev_api_key()?,
        &serde_json::from_value(
            serde_json::json!([{"rooms":["native-state-wire"],"allow":permissions}]),
        )?,
        identity,
        expiry,
    )?
    .token;
    let mut gathered = false;
    let offer = operation(peer, peer.create_offer()?, &mut gathered)
        .await?
        .context("RTP offer")?;
    operation(peer, peer.set_local_description(offer)?, &mut gathered).await?;
    while !gathered {
        while let Some(event) = peer.try_next_event() {
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
    let offer = peer
        .descriptions()?
        .pending_local
        .context("gathered RTP offer")?;
    let request = http::Request::builder()
        .method("POST")
        .uri(format!("http://{server}:7070/api/v1/{endpoint}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/sdp")
        .body(offer.sdp.into_bytes())?;
    let response = create_http_client()
        .execute(request)
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    anyhow::ensure!(
        response.status() == http::StatusCode::CREATED,
        "{endpoint} bootstrap {} {}",
        response.status(),
        String::from_utf8_lossy(response.body())
    );
    operation(
        peer,
        peer.set_remote_description(SessionDescription {
            kind: SessionDescriptionType::Answer,
            sdp: String::from_utf8(response.body().to_vec())?,
        })?,
        &mut gathered,
    )
    .await?;
    Ok(())
}

pub(super) async fn wait_sdp_dtls(peer: &PeerConnection, mid: &[u8]) -> anyhow::Result<()> {
    let transport = peer
        .dtls_transport_by_mid(mid)
        .context("SDP media transport")?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while transport.information().state()
            != pulsebeam_rtc_simulation::DtlsTransportState::Connected
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("SDP DTLS connection deadline")
}

impl WirePeer {
    pub(super) async fn prove_whip_whep_h264(&self, server: IpAddr) -> anyhow::Result<()> {
        const WIDTH: u32 = 160;
        const HEIGHT: u32 = 96;
        let factory = self
            ._host
            .raw_peer_factory(Some(VideoEncoderFactoryHandle::builtin()?))?;
        let codecs = factory
            .get_rtp_sender_capabilities(MediaType::Video)
            .codecs()
            .into_iter()
            .find(|codec| {
                let base = codec.base_codec();
                base.name_bytes().eq_ignore_ascii_case(b"h264")
                    && base
                        .parameters()
                        .iter()
                        .any(|p| p.key == b"packetization-mode" && p.value == b"1")
            })
            .context("stock H.264 packetization-mode 1 capability")?;
        let codecs = [codecs];
        let make_peer = |direction| -> anyhow::Result<_> {
            let mut config: RtcConfiguration = PeerConfiguration::default().into();
            config.options.crypto_options = CryptoOptions::prefer_gcm();
            let peer = factory.create_peer_connection_with_configuration(config)?;
            let source = factory.create_video_source()?;
            let track = factory.create_video_track("whip-h264", &source)?;
            let transceiver = peer.add_video_transceiver(&track, direction)?;
            transceiver.set_rtp_codec_preferences(&codecs)?;
            let mut extensions = transceiver.header_extensions_to_negotiate()?;
            for extension in &mut extensions {
                if extension.uri() == pulsebeam_core::dd::URI {
                    extension.direction = RtpHeaderExtensionDirection::Stopped;
                }
            }
            transceiver.set_header_extensions_to_negotiate(&extensions)?;
            Ok((peer, source, transceiver))
        };
        let (mut publisher, source, sender) = make_peer(RtpTransceiverDirection::SendOnly)?;
        connect_sdp_peer(
            &publisher,
            server,
            "whip?room=native-state-wire&track=video.whip-h264",
            "whip-camera",
            &["join", "publish"],
        )
        .await?;
        wait_sdp_dtls(
            &publisher,
            sender.mid().context("WHIP video MID")?.as_bytes(),
        )
        .await?;
        let push = |frame: u32| -> anyhow::Result<()> {
            let area = usize::try_from(WIDTH * HEIGHT)?;
            let mut pixels = vec![128; area * 3 / 2];
            pixels[..area].fill(70 + u8::try_from(frame)?);
            source.push_frame(&VideoFrame::i420(
                WIDTH,
                HEIGHT,
                pixels,
                super::super::common::native::now_us()?,
                frame * 3000,
            )?)?;
            Ok(())
        };
        for frame in 1..=8 {
            push(frame)?;
            tokio::time::sleep(Duration::from_millis(34)).await;
        }

        let (mut receiver, _, receive) = make_peer(RtpTransceiverDirection::ReceiveOnly)?;
        connect_sdp_peer(
            &receiver,
            server,
            "whep?track=native-state-wire.whip-camera.video.whip-h264",
            "whep-camera-viewer",
            &["subscribe"],
        )
        .await?;
        for (peer, mid) in [(&publisher, sender.mid()), (&receiver, receive.mid())] {
            let mid = mid.context("negotiated video MID")?;
            wait_sdp_dtls(peer, mid.as_bytes()).await?;
            let answer = peer
                .descriptions()?
                .current_remote
                .context("applied SDP answer")?;
            let section = client::inspect(&answer.sdp)?
                .into_iter()
                .find(|section| section.kind == client::Kind::Video)
                .context("H.264 media section")?;
            anyhow::ensure!(
                matches!(section.codecs.as_slice(), [codec] if codec.parameters.name == "h264"
                    && codec.parameters.fmtp.get("packetization-mode").is_some_and(|mode| mode == "1"))
                    && !answer.sdp.contains(pulsebeam_core::dd::URI),
                "WHIP/WHEP must negotiate one ordinary H.264 codec without DD: {section:?}",
            );
        }
        let remote = receive
            .receiver()
            .track()
            .context("WHEP H.264 video track")?;
        let sink = remote.attach_sink()?;
        let udp = self._host.observe_udp_rtp(server);
        let counter = pulsebeam::sim_metrics::routing_counter;
        let before_whep = counter("fabric_whep_rtp_enqueued");
        let h264_enqueued = || {
            counter("fabric_rtp_h264_local_enqueued") + counter("fabric_rtp_h264_shard_enqueued")
        };
        let before_h264 = h264_enqueued();
        let mut decoded = std::collections::BTreeSet::new();
        for frame in 9..=28 {
            push(frame)?;
            tokio::time::sleep(Duration::from_millis(34)).await;
            while let Some(picture) = sink.try_next_frame() {
                assert_eq!((picture.width, picture.height), (WIDTH, HEIGHT));
                decoded.insert(picture.rtp_timestamp);
            }
            if decoded.len() >= 3 {
                break;
            }
        }
        let peaks = udp.peaks();
        anyhow::ensure!(
            decoded.len() >= 3
                && counter("fabric_whep_rtp_enqueued") > before_whep
                && h264_enqueued() > before_h264,
            "WHEP did not decode fresh Fabric H.264: {decoded:?}, UDP {peaks:?}, WHEP admissions {}, H.264 admissions {}",
            counter("fabric_whep_rtp_enqueued") - before_whep,
            h264_enqueued() - before_h264,
        );
        anyhow::ensure!(
            peaks.sent_count > 0 && peaks.received_count > 0 && peaks.received <= 1_216,
            "H.264 protected WHEP datagrams exceeded the route budget: {peaks:?}",
        );
        receiver.close()?;
        publisher.close()?;
        Ok(())
    }
}
