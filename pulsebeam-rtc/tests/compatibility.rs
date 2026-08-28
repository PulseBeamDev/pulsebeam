use proptest::prelude::*;
use pulsebeam_rtc::{
    DtlsFingerprint, IceCandidate, IceCredentials, MediaDirection, MediaKind, ServerTransport,
    negotiate,
};
use str0m::sdp::{MediaAttribute, Sdp, SessionAttribute};

struct Fixture {
    name: &'static str,
    offer: &'static str,
    media: &'static [(MediaKind, MediaDirection)],
    remote_candidates: usize,
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "chrome-127-meet",
        offer: include_str!("fixtures/chrome-127-meet.sdp"),
        media: &[
            (MediaKind::Audio, MediaDirection::ReceiveOnly),
            (MediaKind::Video, MediaDirection::ReceiveOnly),
            (MediaKind::Application, MediaDirection::Bidirectional),
        ],
        remote_candidates: 1,
    },
    Fixture {
        name: "firefox-128-publisher",
        offer: include_str!("fixtures/firefox-128-publisher.sdp"),
        media: &[
            (MediaKind::Audio, MediaDirection::ReceiveOnly),
            (MediaKind::Video, MediaDirection::ReceiveOnly),
        ],
        remote_candidates: 1,
    },
    Fixture {
        name: "webkit-18-subscriber",
        offer: include_str!("fixtures/webkit-18-subscriber.sdp"),
        media: &[
            (MediaKind::Audio, MediaDirection::SendOnly),
            (MediaKind::Video, MediaDirection::SendOnly),
        ],
        remote_candidates: 1,
    },
];

fn server() -> ServerTransport {
    let ice = IceCredentials::new("localufrag".to_owned(), "localpassword".to_owned())
        .expect("valid local ICE credentials");
    let fingerprint = DtlsFingerprint::new("sha-256".to_owned(), Box::new([9; 32]))
        .expect("valid local fingerprint");
    let candidate =
        IceCandidate::new("candidate:1 1 UDP 2130706431 127.0.0.1 9000 typ host".to_owned())
            .expect("valid candidate");
    ServerTransport::new(7, ice, fingerprint, Box::new([candidate]))
}

#[test]
fn client_offer_corpus_produces_web_rtc_1_compatible_answers() {
    for fixture in FIXTURES {
        let result = negotiate(fixture.offer, &server())
            .unwrap_or_else(|error| panic!("{}: {error}", fixture.name));
        let answer = Sdp::parse(result.answer().as_str())
            .unwrap_or_else(|error| panic!("{}: invalid answer: {error}", fixture.name));
        answer
            .assert_consistency()
            .unwrap_or_else(|error| panic!("{}: inconsistent answer: {error}", fixture.name));

        assert_eq!(
            result.session().remote_candidates().len(),
            fixture.remote_candidates
        );
        assert_eq!(result.session().media_sections().len(), fixture.media.len());
        for (section, expected) in result.session().media_sections().iter().zip(fixture.media) {
            assert_eq!(
                (section.kind(), section.direction()),
                *expected,
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
}

proptest! {
    #[test]
    fn supported_offer_shapes_always_produce_consistent_answers(
        direction in prop_oneof![Just("sendonly"), Just("recvonly"), Just("inactive")],
        extension_id in 1u8..15,
        candidate in prop::bool::ANY,
    ) {
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
        let result = negotiate(&offer, &server()).expect("supported offer");
        let answer = Sdp::parse(result.answer().as_str()).expect("answer parses");

        prop_assert!(answer.assert_consistency().is_ok());
        prop_assert_eq!(answer.media_lines[0].ice_candidates().count(), 1);
    }
}
