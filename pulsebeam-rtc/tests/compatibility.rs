use pulsebeam_rtc::{MediaDirection, MediaKind, RtcNegotiation, RtcPeer};
use str0m::sdp::{MediaAttribute, Sdp, SessionAttribute};

struct Fixture {
    name: &'static str,
    offer: &'static str,
    media: &'static [(MediaKind, MediaDirection)],
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "chrome-127-meet",
        offer: include_str!("fixtures/chrome-127-meet.sdp"),
        media: &[
            (MediaKind::Audio, MediaDirection::ReceiveOnly),
            (MediaKind::Video, MediaDirection::Inactive),
            (MediaKind::Application, MediaDirection::Bidirectional),
        ],
    },
    Fixture {
        name: "firefox-128-publisher",
        offer: include_str!("fixtures/firefox-128-publisher.sdp"),
        media: &[
            (MediaKind::Audio, MediaDirection::ReceiveOnly),
            (MediaKind::Video, MediaDirection::Inactive),
        ],
    },
    Fixture {
        name: "webkit-18-subscriber",
        offer: include_str!("fixtures/webkit-18-subscriber.sdp"),
        media: &[
            (MediaKind::Audio, MediaDirection::SendOnly),
            (MediaKind::Video, MediaDirection::SendOnly),
        ],
    },
];

fn negotiate(offer: &str) -> Result<RtcNegotiation, String> {
    RtcPeer::accept(
        std::time::Instant::now(),
        7,
        offer,
        "localufrag".to_owned(),
        "localpassword".to_owned(),
        vec!["candidate:1 1 UDP 2130706431 127.0.0.1 9000 typ host".to_owned()].into_boxed_slice(),
    )
    .map(|(_, negotiation)| negotiation)
    .map_err(|error| error.to_string())
}

#[test]
fn client_offer_corpus_produces_web_rtc_1_compatible_answers() -> Result<(), String> {
    for fixture in FIXTURES {
        let result =
            negotiate(fixture.offer).map_err(|error| format!("{}: {error}", fixture.name))?;
        let answer = Sdp::parse(result.answer())
            .map_err(|error| format!("{}: invalid answer: {error}", fixture.name))?;
        answer
            .assert_consistency()
            .map_err(|error| format!("{}: inconsistent answer: {error}", fixture.name))?;

        assert_eq!(result.media().len(), fixture.media.len());
        for (section, expected) in result.media().iter().zip(fixture.media) {
            assert_eq!(
                (section.kind(), section.direction()),
                *expected,
                "{}",
                fixture.name
            );
        }
        for (section, media) in result.media().iter().zip(answer.media_lines.iter()) {
            assert_eq!(
                media.disabled,
                section.kind() != MediaKind::Application && section.codecs().is_empty(),
                "{}",
                fixture.name
            );
        }

        assert!(
            answer
                .session
                .attrs
                .iter()
                .any(|attribute| matches!(attribute, SessionAttribute::IceLite))
        );
        assert!(answer.session.attrs.iter().all(|attribute| {
            !matches!(
                attribute,
                SessionAttribute::IceUfrag(_)
                    | SessionAttribute::IcePwd(_)
                    | SessionAttribute::Fingerprint(_)
                    | SessionAttribute::Setup(_)
                    | SessionAttribute::Candidate(_)
                    | SessionAttribute::EndOfCandidates
            )
        }));
        for (index, media) in answer.media_lines.iter().enumerate() {
            assert!(media.attrs.iter().any(|attribute| {
                matches!(attribute, MediaAttribute::IceUfrag(value) if value == "localufrag")
            }));
            assert!(media.attrs.iter().any(|attribute| {
                matches!(attribute, MediaAttribute::IcePwd(value) if value == "localpassword")
            }));
            assert!(
                media
                    .attrs
                    .iter()
                    .any(|attribute| matches!(attribute, MediaAttribute::Fingerprint(_)))
            );
            assert!(
                media
                    .attrs
                    .iter()
                    .any(|attribute| matches!(attribute, MediaAttribute::Setup(_)))
            );
            assert_eq!(media.ice_candidates().count(), usize::from(index == 0));
            assert_eq!(media.end_of_candidates(), index == 0);
        }
    }
    Ok(())
}

#[test]
fn browser_offer_corpus_negotiates_only_pulsebeam_codecs() -> Result<(), String> {
    for fixture in FIXTURES {
        let result =
            negotiate(fixture.offer).map_err(|error| format!("{}: {error}", fixture.name))?;
        let answer = Sdp::parse(result.answer())
            .map_err(|error| format!("{}: invalid answer: {error}", fixture.name))?;

        for (section, answer_section) in result.media().iter().zip(answer.media_lines.iter()) {
            let supported = match section.kind() {
                MediaKind::Audio => "opus",
                MediaKind::Video => "h264",
                MediaKind::Application => continue,
            };
            assert!(
                section
                    .codecs()
                    .iter()
                    .all(|codec| codec.name().eq_ignore_ascii_case(supported))
            );
            if !section.codecs().is_empty() {
                assert!(
                    answer_section.rtp_params().iter().all(|codec| codec
                        .spec
                        .codec
                        .to_string()
                        .eq_ignore_ascii_case(supported))
                );
            }
        }
    }
    Ok(())
}

#[test]
fn supported_offer_shapes_always_produce_consistent_answers() -> Result<(), String> {
    for direction in ["sendonly", "recvonly", "inactive"] {
        for extension_id in 1u8..15 {
            for candidate in [false, true] {
                let candidate = if candidate {
                    "a=candidate:1 1 udp 2130706431 127.0.0.1 5000 typ host\r\n"
                } else {
                    ""
                };
                let offer = format!(
                    "v=0\r\n\
             o=- 1 2 IN IP4 127.0.0.1\r\n\
             s=-\r\n\
             t=0 0\r\n\
             a=group:BUNDLE 0\r\n\
             a=ice-ufrag:remoteufrag\r\n\
             a=ice-pwd:remotepassword\r\n\
             a=fingerprint:sha-256 01:02:03:04\r\n\
             a=setup:actpass\r\n\
             m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
             c=IN IP4 0.0.0.0\r\n\
             a=mid:0\r\n\
             a={direction}\r\n\
             a=rtcp-mux\r\n\
             a=rtpmap:111 opus/48000/2\r\n\
             a=extmap:{extension_id} urn:ietf:params:rtp-hdrext:ssrc-audio-level\r\n\
             {candidate}"
                );
                let result = negotiate(&offer).map_err(|error| {
                    format!("{direction} extension {extension_id} candidate {candidate:?}: {error}")
                })?;
                let answer = Sdp::parse(result.answer()).map_err(|error| {
                    format!("{direction} extension {extension_id} candidate {candidate:?}: {error}")
                })?;

                assert!(answer.assert_consistency().is_ok());
                assert_eq!(answer.media_lines[0].ice_candidates().count(), 1);
            }
        }
    }
    Ok(())
}
