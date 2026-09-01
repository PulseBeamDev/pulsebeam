#![allow(
    clippy::arithmetic_side_effects,
    clippy::collapsible_if,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::manual_contains,
    clippy::redundant_closure_for_method_calls,
    clippy::uninlined_format_args,
    reason = "compatibility assertions deliberately use concise fixture indexing and panic messages"
)]

use std::{
    collections::HashSet,
    panic::{AssertUnwindSafe, catch_unwind},
};

use proptest::prelude::*;
use pulsebeam_rtc::{
    DtlsFingerprint, DtlsRole, IceCandidate, IceCredentials, MediaDirection, MediaKind,
    NegotiationParameters, RtcConfiguration, RtcNegotiation,
};

struct Fixture {
    name: &'static str,
    offer: &'static str,
    mids: &'static [&'static str],
    directions: &'static [MediaDirection],
    video_rids: &'static [&'static str],
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "chrome-representative",
        offer: include_str!("fixtures/chrome-representative.sdp"),
        mids: &["audio", "video", "data"],
        directions: &[
            MediaDirection::SendOnly,
            MediaDirection::SendOnly,
            MediaDirection::Bidirectional,
        ],
        video_rids: &["q", "h", "f"],
    },
    Fixture {
        name: "firefox-representative",
        offer: include_str!("fixtures/firefox-representative.sdp"),
        mids: &["0", "1", "2"],
        directions: &[
            MediaDirection::SendOnly,
            MediaDirection::ReceiveOnly,
            MediaDirection::Bidirectional,
        ],
        video_rids: &["low", "high"],
    },
];

fn parameters() -> NegotiationParameters {
    let ice = IceCredentials::new(
        "localufrag".to_owned(),
        "localpasswordlongenough22".to_owned(),
    )
    .expect("valid test ICE credentials");
    let fingerprint = DtlsFingerprint::new("sha-256".to_owned(), Box::new([9; 32]))
        .expect("valid test fingerprint");
    let candidate =
        IceCandidate::new("candidate:1 1 UDP 2130706431 127.0.0.1 9000 typ host".to_owned())
            .expect("valid test candidate");
    NegotiationParameters::new(ice, fingerprint, Box::new([candidate]))
}

fn negotiate(offer: &str, configuration: &RtcConfiguration) -> Result<RtcNegotiation, String> {
    let parameters = parameters();
    pulsebeam_rtc::negotiate(offer, configuration, &parameters).map_err(|error| error.to_string())
}

fn lines_with_prefix<'a>(sdp: &'a str, prefix: &str) -> impl Iterator<Item = &'a str> {
    sdp.lines()
        .map(str::trim)
        .filter(move |line| line.starts_with(prefix))
}

fn answer_sections(answer: &str) -> Vec<Vec<&str>> {
    let mut sections = Vec::new();
    let mut current = None;
    for line in answer.lines().map(str::trim) {
        if line.starts_with("m=") {
            if let Some(section) = current.replace(Vec::new()) {
                sections.push(section);
            }
        }
        if let Some(section) = &mut current {
            if !line.is_empty() {
                section.push(line);
            }
        }
    }
    if let Some(section) = current {
        if !section.is_empty() {
            sections.push(section);
        }
    }
    sections
}

fn section_attr<'a>(section: &'a [&'a str], prefix: &str) -> Option<&'a str> {
    section
        .iter()
        .copied()
        .find_map(|line| line.strip_prefix(prefix))
}

fn answer_direction(section: &[&str]) -> Option<MediaDirection> {
    section.iter().find_map(|line| match *line {
        "a=sendonly" => Some(MediaDirection::SendOnly),
        "a=recvonly" => Some(MediaDirection::ReceiveOnly),
        "a=inactive" => Some(MediaDirection::Inactive),
        "a=sendrecv" => Some(MediaDirection::Bidirectional),
        _ => None,
    })
}

fn answer_has_apt(section: &[&str], rtx: u8, primary: u8) -> bool {
    section.iter().any(|line| {
        let Some(value) = line.strip_prefix("a=fmtp:") else {
            return false;
        };
        let Some((payload_type, parameters)) = value.split_once(char::is_whitespace) else {
            return false;
        };
        payload_type.parse::<u8>().ok() == Some(rtx)
            && parameters
                .split(';')
                .map(str::trim)
                .any(|parameter| parameter == format!("apt={primary}"))
    })
}

fn opposite_direction(direction: MediaDirection) -> MediaDirection {
    match direction {
        MediaDirection::SendOnly => MediaDirection::ReceiveOnly,
        MediaDirection::ReceiveOnly => MediaDirection::SendOnly,
        MediaDirection::Inactive => MediaDirection::Inactive,
        MediaDirection::Bidirectional => MediaDirection::Bidirectional,
    }
}

fn assert_answer_is_semantic_reparse(fixture: &Fixture, result: &RtcNegotiation) {
    let answer = result.answer();
    let reparsed = str0m::sdp::Sdp::parse(answer).expect("generated answer parses");
    reparsed
        .assert_consistency()
        .expect("generated answer is consistent");
    assert_eq!(
        answer.matches("c=IN IP4 0.0.0.0").count(),
        fixture.mids.len()
    );
    let sections = answer_sections(answer);
    assert_eq!(sections.len(), fixture.mids.len(), "{}", fixture.name);
    assert!(answer.lines().any(|line| line.trim() == "a=ice-lite"));
    let bundle = answer
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("a=group:BUNDLE "))
        .expect("answer must name one BUNDLE group");
    assert_eq!(bundle.split_whitespace().collect::<Vec<_>>(), fixture.mids);
    let fingerprints: Vec<_> = answer
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix("a=fingerprint:"))
        .collect();
    assert!(!fingerprints.is_empty());
    assert!(
        fingerprints
            .iter()
            .all(|fingerprint| *fingerprint == fingerprints[0])
    );

    for (index, media) in result.media().iter().enumerate() {
        let section = &sections[index];
        assert_eq!(media.mid(), fixture.mids[index], "{}", fixture.name);
        assert_eq!(
            media.direction(),
            opposite_direction(fixture.directions[index]),
            "{}",
            fixture.name
        );
        assert_eq!(
            section_attr(section, "a=mid:"),
            Some(fixture.mids[index]),
            "{}",
            fixture.name
        );
        assert_eq!(
            answer_direction(section),
            Some(media.direction()),
            "returned facts disagree with reparsed answer for {}",
            fixture.name
        );
        if media.kind() != MediaKind::Application {
            assert!(section.iter().any(|line| *line == "a=rtcp-mux"));
        }
        for codec in media.codecs() {
            assert!(
                section.iter().any(|line| {
                    line.starts_with(&format!("a=rtpmap:{} ", codec.payload_type()))
                        && line
                            .to_ascii_lowercase()
                            .contains(&codec.name().to_ascii_lowercase())
                }),
                "{} omits negotiated payload type {}",
                fixture.name,
                codec.payload_type()
            );
        }
    }

    let session = result.session();
    let parameters = parameters();
    assert_eq!(session.local_ice(), parameters.local_ice());
    assert_eq!(session.local_candidates(), parameters.local_candidates());
    assert_eq!(session.local_dtls_role(), DtlsRole::Active);
    assert!(answer.contains(&format!("a=ice-ufrag:{}", parameters.local_ice().ufrag())));
    assert!(answer.contains(&format!("a=ice-pwd:{}", parameters.local_ice().password())));
    assert!(answer.contains(&format!("a={}", parameters.local_candidates()[0].as_sdp())));
    assert!(!answer.contains("a=ssrc:"));
    assert!(!answer.contains("a=ssrc-group:"));
    assert!(!answer.contains("a=msid:"));
    let negotiated_fingerprint = session.local_fingerprint();
    assert_eq!(negotiated_fingerprint, parameters.local_fingerprint());
    let answer_fingerprint = fingerprints[0]
        .split_whitespace()
        .next()
        .expect("fingerprint has an algorithm");
    assert_eq!(
        negotiated_fingerprint.algorithm().to_ascii_lowercase(),
        answer_fingerprint.to_ascii_lowercase()
    );
    assert_eq!(negotiated_fingerprint.value().len(), 32);
    assert_eq!(session.media_sections().len(), fixture.mids.len());
    let remote_ufrag = lines_with_prefix(fixture.offer, "a=ice-ufrag:")
        .next()
        .expect("remote ICE ufrag")
        .strip_prefix("a=ice-ufrag:")
        .expect("remote ICE ufrag prefix");
    let remote_password = lines_with_prefix(fixture.offer, "a=ice-pwd:")
        .next()
        .expect("remote ICE password")
        .strip_prefix("a=ice-pwd:")
        .expect("remote ICE password prefix");
    assert_eq!(session.remote_ice().ufrag(), remote_ufrag);
    assert_eq!(session.remote_ice().password(), remote_password);
    let remote_fingerprint = lines_with_prefix(fixture.offer, "a=fingerprint:")
        .next()
        .expect("remote DTLS fingerprint")
        .strip_prefix("a=fingerprint:")
        .expect("remote DTLS fingerprint prefix");
    let (algorithm, value) = remote_fingerprint
        .split_once(' ')
        .expect("remote DTLS fingerprint fields");
    let value: Vec<_> = value
        .split(':')
        .map(|byte| u8::from_str_radix(byte, 16).expect("remote DTLS fingerprint byte"))
        .collect();
    assert_eq!(session.remote_fingerprint().algorithm(), algorithm);
    assert_eq!(session.remote_fingerprint().value(), value.as_slice());
    let remote_candidates: Vec<_> = lines_with_prefix(fixture.offer, "a=candidate:").collect();
    assert_eq!(session.remote_candidates().len(), remote_candidates.len());
    for (candidate, remote) in session.remote_candidates().iter().zip(remote_candidates) {
        let expected = remote
            .strip_prefix("a=")
            .expect("candidate attribute")
            .replacen("UDP", "udp", 1);
        assert_eq!(candidate.as_sdp(), expected);
    }
    for (index, section) in session.media_sections().iter().enumerate() {
        let media = &result.media()[index];
        assert_eq!(section.mid(), fixture.mids[index]);
        if section.kind() != MediaKind::Application {
            assert!(!section.header_extensions().is_empty());
        }
        for extension in section.header_extensions() {
            assert!(answer.contains(&format!("a=extmap:{} {}", extension.id(), extension.uri())));
        }
        if section.kind() == MediaKind::Video {
            assert_eq!(media.rids(), fixture.video_rids);
            for rid in fixture.video_rids {
                assert!(answer.contains(&format!("a=rid:{rid} ")));
            }
            if media.direction() == MediaDirection::ReceiveOnly {
                assert_eq!(section.receive_rids(), fixture.video_rids);
            }
            if fixture.name == "chrome-representative" {
                assert_eq!(section.ssrcs(), &[11111111, 22222222]);
                assert_eq!(section.ssrc_groups().len(), 1);
                assert_eq!(section.ssrc_groups()[0].semantics(), "FID");
                assert_eq!(section.ssrc_groups()[0].members(), &[11111111, 22222222]);
            }
        }
        if section.kind() == MediaKind::Application {
            assert_eq!(
                section
                    .data_channel()
                    .expect("SCTP association")
                    .sctp_port(),
                5000
            );
        }
    }

    let codec_names: Vec<_> = result
        .media()
        .iter()
        .flat_map(|media| media.codecs().iter().map(|codec| codec.name()))
        .collect();
    assert!(
        codec_names
            .iter()
            .all(|name| name.eq_ignore_ascii_case("opus") || name.eq_ignore_ascii_case("h264"))
    );
    for (index, media) in result.media().iter().enumerate() {
        if media.kind() == MediaKind::Video {
            let primary = media
                .codecs()
                .iter()
                .find(|codec| codec.name().eq_ignore_ascii_case("h264"))
                .expect("H264 is an immutable negotiated fact");
            let rtx = primary
                .retransmission_payload_type()
                .expect("RTX is tied to the primary payload type");
            assert!(answer_has_apt(
                &sections[index],
                rtx,
                primary.payload_type()
            ));
            assert!(answer.contains(&format!("a=rtpmap:{rtx} rtx/")));
            let h264 = primary.h264().expect("H264 parameters");
            assert_eq!(h264.packetization_mode(), Some(1));
            assert_eq!(h264.profile_level_id(), Some("42e01f"));
            assert_eq!(h264.level_asymmetry_allowed(), Some(true));
        }
    }
    assert_eq!(
        sections
            .iter()
            .filter(|section| section_attr(section, "a=sctp-port:").is_some())
            .count(),
        1
    );
}

#[test]
fn browser_surplus_codecs_and_extensions_are_filtered() -> Result<(), String> {
    let offer = FIXTURES[0]
        .offer
        .replace(
            "m=video 9 UDP/TLS/RTP/SAVPF 102 121",
            "m=video 9 UDP/TLS/RTP/SAVPF 96 97 102 121",
        )
        .replace(
            "a=mid:video\n",
            "a=mid:video\na=rtpmap:96 VP8/90000\na=rtpmap:97 rtx/90000\na=fmtp:97 apt=96\n",
        )
        .replace(
            "a=extmap:3 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
            "a=extmap:3 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\na=extmap:10 urn:unsupported:test",
        );
    let result = negotiate(&offer, &RtcConfiguration::default())?;
    assert!(!result.answer().contains("VP8"));
    assert!(!result.answer().contains("urn:unsupported:test"));
    assert!(!result.answer().contains("a=rtpmap:97 rtx/"));
    assert!(result.answer().contains("H264/90000"));
    Ok(())
}

#[test]
fn browser_ssrc_simulcast_and_fid_relationships_are_preserved() {
    let offer = FIXTURES[0]
        .offer
        .replace(
            "a=ssrc-group:FID 11111111 22222222",
            "a=ssrc:33333333 cname:chrome\na=ssrc:33333333 msid:- video\na=ssrc:44444444 cname:chrome\na=ssrc:44444444 msid:- video\na=ssrc-group:SIM 11111111 33333333\na=ssrc-group:FID 11111111 22222222\na=ssrc-group:FID 33333333 44444444",
        );
    let result = negotiate(&offer, &RtcConfiguration::default()).expect("valid SIM/FID groups");
    let section = &result.session().media_sections()[1];
    assert_eq!(section.ssrcs(), &[11111111, 22222222, 33333333, 44444444]);
    assert_eq!(section.ssrc_groups().len(), 3);
    assert_eq!(section.ssrc_groups()[0].semantics(), "SIM");
    assert_eq!(section.ssrc_groups()[0].members(), &[11111111, 33333333]);
    assert_eq!(section.ssrc_groups()[1].members(), &[11111111, 22222222]);
    assert_eq!(section.ssrc_groups()[2].members(), &[33333333, 44444444]);

    assert_rejected(
        offer.replace(
            "a=ssrc-group:SIM 11111111 33333333",
            "a=ssrc-group:SIM 11111111 33333333\na=ssrc-group:SIM 11111111 33333333",
        ),
        "ssrc",
    );
    assert_rejected(
        offer.replace(
            "a=ssrc-group:SIM 11111111 33333333",
            "a=ssrc-group:SIM 11111111 22222222",
        ),
        "ssrc",
    );
    assert_rejected(
        offer.replace(
            "a=ssrc-group:FID 33333333 44444444",
            "a=ssrc-group:FID 11111111 44444444",
        ),
        "ssrc",
    );
    assert_rejected(
        offer.replace(
            "a=ssrc-group:FID 33333333 44444444",
            "a=ssrc-group:FID 33333333 99999999",
        ),
        "ssrc",
    );
    assert_rejected(
        offer.replace(
            "a=ssrc-group:SIM 11111111 33333333",
            "a=ssrc-group:SIM 0 33333333",
        ),
        "ssrc",
    );
}

#[test]
fn malformed_codec_parameters_and_rejected_sections_are_safe() {
    assert_rejected(FIXTURES[0].offer.replace("apt=102", "apt=bad"), "codec");
    assert_rejected(
        FIXTURES[0]
            .offer
            .replace("profile-level-id=42e01f", "profile-level-id=bad"),
        "codec",
    );
    let offer = FIXTURES[0]
        .offer
        .replace("m=video 9 UDP/TLS/RTP/SAVPF", "m=video 0 UDP/TLS/RTP/SAVPF");
    let result = negotiate(&offer, &RtcConfiguration::default()).expect("port-zero is inactive");
    assert!(result.answer().contains("m=video 0 UDP/TLS/RTP/SAVPF"));
    assert!(
        result
            .answer()
            .contains("m=video 0 UDP/TLS/RTP/SAVPF 102 121")
    );
    assert!(result.media()[1].ingress().is_none());
    assert!(result.media()[1].egress().is_none());
    assert!(result.session().media_sections()[1].codecs().is_empty());
}

#[test]
fn bounded_offers_and_immutable_extension_probe_facts_are_enforced() {
    let extension_offer = FIXTURES[0].offer.replace(
        "a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level",
        "a=extmap:1/sendonly urn:ietf:params:rtp-hdrext:ssrc-audio-level",
    );
    let result = negotiate(&extension_offer, &RtcConfiguration::default()).expect("valid extmap");
    assert_eq!(
        result.session().media_sections()[0].header_extensions()[0].direction(),
        MediaDirection::ReceiveOnly
    );

    let probe_offer = FIXTURES[0]
        .offer
        .replace("a=ssrc:11111111 cname:chrome\n", "a=ssrc:0 cname:probe\n")
        .replace("a=ssrc:11111111 msid:- video\n", "")
        .replace("a=ssrc-group:FID 11111111 22222222\n", "")
        .replace("a=ssrc:22222222 cname:chrome\n", "")
        .replace("a=ssrc:22222222 msid:- video\n", "")
        .replace(
            "a=ssrc:0 cname:probe\n",
            "a=ssrc:0 cname:probe\na=ssrc:0 msid:- video\n",
        );
    let probe_result = negotiate(&probe_offer, &RtcConfiguration::default()).expect("probe fact");
    assert!(
        probe_result.session().media_sections()[1]
            .ssrcs()
            .is_empty()
    );
    assert!(probe_result.session().media_sections()[1].has_ssrc_zero_probe());

    let oversized = format!("{}{}", FIXTURES[0].offer, "x".repeat(300_000));
    assert_rejected(oversized, "size");
    assert_rejected(
        FIXTURES[0]
            .offer
            .replace("a=rid:q send", &format!("a=rid:{} send", "q".repeat(64))),
        "rid",
    );
    assert_rejected(
        FIXTURES[0]
            .offer
            .replace("a=simulcast:send q;h;f", "a=simulcast:send q;;h;f"),
        "simulcast",
    );
}

#[test]
fn negotiation_rejects_ambiguous_transport_and_media_facts() {
    let direction_ext = FIXTURES[0].offer.replace(
        "a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level",
        "a=extmap:1/sendonly urn:ietf:params:rtp-hdrext:ssrc-audio-level",
    );
    let result = negotiate(&direction_ext, &RtcConfiguration::default()).expect("extmap direction");
    assert!(
        result
            .answer()
            .contains("a=extmap:1/recvonly urn:ietf:params:rtp-hdrext:ssrc-audio-level")
    );
    let mixed = direction_ext
        .replace("a=group:BUNDLE", "a=extmap-allow-mixed\na=group:BUNDLE")
        .replace("a=extmap:1/sendonly", "a=extmap:15/sendonly");
    let mixed_result = negotiate(&mixed, &RtcConfiguration::default()).expect("mixed extmap IDs");
    assert!(mixed_result.answer().contains("a=extmap:15/recvonly"));
    assert_rejected(
        FIXTURES[0].offer.replace("a=rid:q send", "a=rid:q recv"),
        "rid",
    );
    assert_rejected(
        FIXTURES[0].offer.replace("a=rid:q send", "a=rid:q! send"),
        "rid",
    );
    assert_rejected(
        FIXTURES[0].offer.replace("a=rid:q send", "a=rid:é send"),
        "rid",
    );
    assert_rejected(
        FIXTURES[0].offer.replace(
            "a=simulcast:send q;h;f",
            "a=simulcast:send q;h;f\na=simulcast:send q;h;f",
        ),
        "simulcast",
    );
    assert_rejected(
        FIXTURES[0]
            .offer
            .replace("a=mid:audio\n", "a=mid:audio\na=rid:audio send\n"),
        "rid",
    );
    assert_rejected(
        FIXTURES[0]
            .offer
            .replace("a=mid:video\n", "a=mid:video\na=ice-ufrag:otherufrag\n"),
        "conflicting",
    );
    assert_rejected(
        FIXTURES[0].offer.replace(
            "a=mid:video\n",
            "a=mid:video\na=fingerprint:sha-256 21:22:23:24:25:26:27:28:29:2a:2b:2c:2d:2e:2f:30:31:32:33:34:35:36:37:38:39:3a:3b:3c:3d:3e:3f:40\n",
        ),
        "conflicting",
    );
    assert_rejected(
        FIXTURES[0]
            .offer
            .replace("a=mid:video\n", "a=mid:video\na=setup:active\n"),
        "conflicting",
    );
    assert_rejected(
        FIXTURES[0]
            .offer
            .replace("m=audio 9 UDP/TLS/RTP/SAVPF", "m=audio 9 UDP/DTLS/SCTP"),
        "srtp",
    );
    assert_rejected(
        direction_ext.replace("a=extmap:1/sendonly", "a=extmap:1/bogus"),
        "extension",
    );
    assert_rejected(
        FIXTURES[0]
            .offer
            .replace("a=setup:actpass", "a=setup:actpass\na=ice-lite"),
        "ice-lite",
    );
    assert_rejected(
        FIXTURES[0].offer.replace(
            "a=ssrc:11111111 cname:chrome",
            "a=ssrc:11111111 cname:one\na=ssrc:11111111 cname:two",
        ),
        "ssrc",
    );
    assert_rejected(
        FIXTURES[0].offer.replace(
            "a=ssrc-group:FID 11111111 22222222",
            "a=ssrc-group:FID 11111111 11111111",
        ),
        "ssrc",
    );
    assert_rejected(
        FIXTURES[0]
            .offer
            .replace("a=sctp-port:5000", "a=sctp-port:5000\na=sctp-port:5000"),
        "sctp",
    );
    let duplicate_candidate = FIXTURES[0].offer.replace(
        "a=candidate:1 1 UDP 2130706431 192.0.2.1 50000 typ host",
        "a=candidate:1 1 UDP 2130706431 192.0.2.1 50000 typ host\na=candidate:1 1 UDP 2130706431 192.0.2.1 50000 typ host",
    );
    let duplicate_result = negotiate(&duplicate_candidate, &RtcConfiguration::default())
        .expect("duplicate candidates are stably deduplicated");
    assert_eq!(duplicate_result.session().remote_candidates().len(), 1);
    assert_rejected(
        FIXTURES[0]
            .offer
            .replace("a=mid:data\n", "a=mid:data\na=sendonly\n"),
        "sctp",
    );
    assert_rejected(
        FIXTURES[0].offer.replace(
            "m=application 9 UDP/DTLS/SCTP webrtc-datachannel",
            "m=application 9 UDP/TLS/RTP/SAVPF webrtc-datachannel",
        ),
        "invalid",
    );
    assert!(
        IceCredentials::new("safe\nufrag".to_owned(), "passwordlongenough22".to_owned()).is_none()
    );
    assert!(
        IceCredentials::new("safeufrag".to_owned(), "passwordlongenough22\r".to_owned()).is_none()
    );
    assert!(DtlsFingerprint::new("sha-256\n".to_owned(), Box::new([9; 32])).is_none());
    assert!(IceCandidate::new("candidate:1\r\n".to_owned()).is_none());
}

#[test]
fn zero_ssrc_is_preserved_without_answer_ownership() {
    let offer = FIXTURES[0]
        .offer
        .replace("a=ssrc:11111111 cname:chrome\n", "a=ssrc:0 cname:probe\n")
        .replace("a=ssrc:11111111 msid:- video\n", "")
        .replace("a=ssrc-group:FID 11111111 22222222\n", "")
        .replace("a=ssrc:22222222 cname:chrome\n", "")
        .replace("a=ssrc:22222222 msid:- video\n", "");
    let result =
        negotiate(&offer, &RtcConfiguration::default()).expect("SSRC zero is a probe fact");
    assert!(result.session().media_sections()[1].ssrcs().is_empty());
    assert!(result.session().media_sections()[1].has_ssrc_zero_probe());
    assert!(!result.answer().contains("a=ssrc:"));
}

#[test]
fn representative_browser_offers_preserve_semantic_facts() -> Result<(), String> {
    for fixture in FIXTURES {
        let result = negotiate(fixture.offer, &RtcConfiguration::default())
            .map_err(|error| format!("{}: {error}", fixture.name))?;
        assert_answer_is_semantic_reparse(fixture, &result);
    }
    Ok(())
}

#[test]
fn answers_keep_only_supported_codecs_and_one_sctp_association() -> Result<(), String> {
    for fixture in FIXTURES {
        let result = negotiate(fixture.offer, &RtcConfiguration::default())?;
        let answer = result.answer();
        assert!(answer.contains("opus/48000/2"), "{}", fixture.name);
        assert!(answer.contains("H264/90000"), "{}", fixture.name);
        assert!(answer.contains("apt="), "{}", fixture.name);
        assert!(answer.contains("nack"), "{}", fixture.name);
        assert!(answer.contains("transport-wide-cc"), "{}", fixture.name);
        assert!(
            answer.contains("video-dependency-descriptor"),
            "{}",
            fixture.name
        );
        assert!(
            answer.contains("video-layers-allocation"),
            "{}",
            fixture.name
        );
        assert!(answer.contains("abs-capture-time"), "{}", fixture.name);
        assert!(answer.contains("playout-delay"), "{}", fixture.name);
        assert!(answer.contains("a=rid:"), "{}", fixture.name);
        assert!(answer.contains("a=simulcast:"), "{}", fixture.name);
        assert_eq!(
            answer.matches("a=sctp-port:").count(),
            1,
            "{}",
            fixture.name
        );
        assert_eq!(
            result
                .media()
                .iter()
                .filter(|media| media.kind() == MediaKind::Application)
                .count(),
            1,
            "{}",
            fixture.name
        );
    }
    Ok(())
}

fn assert_rejected(offer: String, expected: &str) {
    let result = catch_unwind(AssertUnwindSafe(|| {
        negotiate(&offer, &RtcConfiguration::default())
    }));
    let result = result.expect("arbitrary offer text must not panic");
    let error = result.expect_err("malformed or ambiguous SDP must be rejected");
    assert!(
        error
            .to_ascii_lowercase()
            .contains(&expected.to_ascii_lowercase()),
        "expected {expected} error, got {error}"
    );
}

#[test]
fn unsupported_codecs_and_directions_are_explicitly_rejected() {
    assert_rejected(
        FIXTURES[0].offer.replace("H264/90000", "VP8/90000"),
        "codec",
    );
    assert_rejected(
        FIXTURES[0].offer.replace("a=sendonly\n", "a=sendrecv\n"),
        "direction",
    );
}

#[test]
fn ownership_and_mapping_ambiguities_are_explicitly_rejected() {
    assert_rejected(
        FIXTURES[0].offer.replace(
            "a=group:BUNDLE audio video data",
            "a=group:BUNDLE audio video audio",
        ),
        "bundle",
    );
    assert_rejected(
        FIXTURES[0].offer.replace("a=mid:data", "a=mid:video"),
        "duplicate",
    );
    assert_rejected(
        FIXTURES[0].offer.replace(
            "a=rtpmap:111 opus/48000/2",
            "a=rtpmap:111 opus/48000/2\na=rtpmap:111 PCMU/8000",
        ),
        "payload",
    );
    assert_rejected(
        FIXTURES[0].offer.replace(
            "a=extmap:8 http://www.webrtc.org/experiments/rtp-hdrext/playout-delay",
            "a=extmap:8 http://www.webrtc.org/experiments/rtp-hdrext/playout-delay\na=extmap:8 urn:duplicate",
        ),
        "extension",
    );
}

#[test]
fn configured_slot_capacity_and_transport_parameters_are_bounded() {
    let capacity = RtcConfiguration::new(1, 1, 1, 1).expect("positive bounds are valid");
    let result = catch_unwind(AssertUnwindSafe(|| negotiate(FIXTURES[0].offer, &capacity)));
    let result = result.expect("capacity rejection must not panic");
    let error = result.expect_err("three offered sections exceed one configured slot");
    assert!(error.to_ascii_lowercase().contains("slot"));

    assert_rejected(
        FIXTURES[0]
            .offer
            .replace(
                "sha-256 01:02:03:04:05:06:07:08:09:0a:0b:0c:0d:0e:0f:10:11:12:13:14:15:16:17:18:19:1a:1b:1c:1d:1e:1f:20",
                "sha-256 01:02:03",
            ),
        "fingerprint",
    );
    assert_rejected(
        FIXTURES[0]
            .offer
            .replace("a=sctp-port:5000", "a=sctp-port:0"),
        "sctp",
    );
}

#[test]
fn arbitrary_offer_text_never_panics_or_creates_ambiguous_ownership() {
    for offer in ["", "v=0\n", "not SDP", "\0\u{1}\u{7f}", "m=audio"] {
        let result = catch_unwind(AssertUnwindSafe(|| {
            negotiate(offer, &RtcConfiguration::default())
        }));
        assert!(result.is_ok(), "offer {:?} panicked", offer);
        assert!(
            result.expect("checked above").is_err(),
            "offer {:?} accepted",
            offer
        );
    }
}

proptest! {
    #[test]
    fn bounded_arbitrary_offer_bytes_never_panic(
        bytes in prop::collection::vec(any::<u8>(), 0..=512)
    ) {
        let offer = String::from_utf8_lossy(&bytes);
        let result = catch_unwind(AssertUnwindSafe(|| {
            negotiate(offer.as_ref(), &RtcConfiguration::default())
        }));
        prop_assert!(result.is_ok());
        if let Ok(Ok(negotiation)) = result {
            let mut mids = HashSet::new();
            let mut ingress = HashSet::new();
            let mut egress = HashSet::new();
            let mut applications = 0;
            for media in negotiation.media() {
                prop_assert!(mids.insert(media.mid()));
                if let Some(stream) = media.ingress() {
                    prop_assert!(ingress.insert(stream));
                }
                if let Some(slot) = media.egress() {
                    prop_assert!(egress.insert(slot));
                }
                if media.kind() == MediaKind::Application {
                    applications += 1;
                }
                let mut rids = HashSet::new();
                for rid in media.rids() {
                    prop_assert!(rids.insert(rid));
                }
            }
            prop_assert!(applications <= 1);
            for section in negotiation.session().media_sections() {
                let mut payload_types = HashSet::new();
                for codec in section.codecs() {
                    prop_assert!(payload_types.insert(codec.payload_type()));
                }
                let mut extension_ids = HashSet::new();
                for extension in section.header_extensions() {
                    prop_assert!(extension_ids.insert(extension.id()));
                }
            }
        }
    }
}

#[test]
fn supported_extension_facts_are_present_in_each_offer_fixture() {
    for fixture in FIXTURES {
        for required in [
            "rtcp-mux",
            "transport-wide-cc",
            "video-dependency-descriptor",
            "video-layers-allocation",
            "abs-capture-time",
            "playout-delay",
            "ssrc-audio-level",
            "a=simulcast:",
            "a=sctp-port:5000",
        ] {
            assert!(
                fixture.offer.contains(required),
                "{} lacks {required}",
                fixture.name
            );
        }
        assert_eq!(lines_with_prefix(fixture.offer, "a=mid:").count(), 3);
    }
}

#[test]
fn chrome_application_without_direction_is_bidirectional() {
    assert!(!FIXTURES[0].offer.contains("a=mid:data\na=sendrecv"));
    let result = negotiate(FIXTURES[0].offer, &RtcConfiguration::default())
        .expect("browser-shaped data m-line");
    assert_eq!(result.media()[2].direction(), MediaDirection::Bidirectional);
    assert!(result.media()[2].ingress().is_none());
    assert!(result.media()[2].egress().is_none());
    assert!(result.answer().contains("a=sendrecv"));
}

#[test]
fn application_direction_and_protocol_are_strict() {
    assert_rejected(
        FIXTURES[0]
            .offer
            .replace("a=mid:data\n", "a=mid:data\na=sendonly\n"),
        "sctp",
    );
    assert_rejected(
        FIXTURES[0]
            .offer
            .replace("a=mid:data\n", "a=mid:data\na=inactive\n"),
        "sctp",
    );
    assert_rejected(
        FIXTURES[0]
            .offer
            .replace("a=mid:data\n", "a=mid:data\na=sendrecv\na=recvonly\n"),
        "sctp",
    );
    assert_rejected(
        FIXTURES[0].offer.replace(
            "m=application 9 UDP/DTLS/SCTP webrtc-datachannel",
            "m=application 9 DTLS/SCTP 5000",
        ),
        "protocol",
    );
}

#[test]
fn max_message_size_distinguishes_default_finite_and_unlimited() {
    let absent_offer = FIXTURES[0].offer.replace("a=max-message-size:262144\n", "");
    let absent = negotiate(&absent_offer, &RtcConfiguration::default()).expect("absent");
    assert_eq!(
        absent.session().media_sections()[2]
            .data_channel()
            .expect("data channel")
            .max_message_size(),
        pulsebeam_rtc::MaxMessageSize::Default
    );
    let finite_offer = FIXTURES[0]
        .offer
        .replace("a=max-message-size:262144", "a=max-message-size:65536");
    assert_eq!(
        negotiate(&finite_offer, &RtcConfiguration::default())
            .expect("finite")
            .session()
            .media_sections()[2]
            .data_channel()
            .expect("data channel")
            .max_message_size(),
        pulsebeam_rtc::MaxMessageSize::finite(65536).expect("finite size")
    );
    let unlimited_offer = FIXTURES[0]
        .offer
        .replace("a=max-message-size:262144", "a=max-message-size:0");
    assert_eq!(
        negotiate(&unlimited_offer, &RtcConfiguration::default())
            .expect("unlimited")
            .session()
            .media_sections()[2]
            .data_channel()
            .expect("data channel")
            .max_message_size(),
        pulsebeam_rtc::MaxMessageSize::Unlimited
    );
}

#[test]
fn rtcp_mux_only_is_answered_as_rtcp_mux() {
    let offer = FIXTURES[0]
        .offer
        .replace("a=rtcp-mux\n", "a=rtcp-mux-only\n");
    let result = negotiate(&offer, &RtcConfiguration::default()).expect("rtcp-mux-only");
    assert!(result.answer().contains("a=rtcp-mux\r\n"));
    assert!(!result.answer().contains("a=rtcp-mux-only"));
    str0m::sdp::Sdp::parse(result.answer())
        .expect("answer reparses")
        .assert_consistency()
        .expect("answer consistent");
}

#[test]
fn absent_payload_maps_do_not_enter_facts_or_answers() {
    let offer = FIXTURES[0].offer.replace(
        "a=rtpmap:111 opus/48000/2",
        "a=rtpmap:111 opus/48000/2\na=rtpmap:112 opus/48000/2",
    );
    let result = negotiate(&offer, &RtcConfiguration::default()).expect("ignored absent map");
    assert!(!result.answer().contains("rtpmap:112"));
    assert!(
        result.media()[0]
            .codecs()
            .iter()
            .all(|codec| codec.payload_type() != 112)
    );
}

#[test]
fn bundled_rtp_namespaces_are_coherent() {
    let missing_mid = FIXTURES[0]
        .offer
        .replace("a=extmap:9 urn:ietf:params:rtp-hdrext:sdes:mid\n", "");
    assert_rejected(missing_mid, "mid");
    let conflicting_id = FIXTURES[0].offer.replace(
        "a=extmap:9 urn:ietf:params:rtp-hdrext:sdes:mid\n",
        "a=extmap:9 urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id\n",
    );
    assert_rejected(conflicting_id, "extension");
    let conflicting_pt = FIXTURES[0]
        .offer
        .replace("a=rtpmap:102 H264/90000", "a=rtpmap:111 H264/90000");
    assert_rejected(conflicting_pt, "payload");
}

#[test]
fn ice_credentials_use_ice_characters() {
    assert!(
        IceCredentials::new("u+/4".to_owned(), "p+/abcdefghijklmnopqrstu".to_owned()).is_some()
    );
    assert!(IceCredentials::new("u!34".to_owned(), "passwordlongenough22".to_owned()).is_none());
    assert!(IceCredentials::new("u234".to_owned(), "passwordlongenough-22".to_owned()).is_none());
}
