use pulsebeam_core::h264;
use std::fmt::Debug;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketSemantic {
    Sps,
    Pps,
    Idr,
    FuAFragment,
    Delta,
    Marker,
    Rtx,
    Padding,
    Twcc,
}

#[derive(Clone, Copy, Debug)]
pub struct PacketFacts<'a> {
    h264: Option<&'a [u8]>,
    marker: bool,
    rtx: bool,
    padding: bool,
    twcc: bool,
}

impl<'a> PacketFacts<'a> {
    pub fn h264(payload: &'a [u8], marker: bool) -> Self {
        debug_assert!(!payload.is_empty(), "H.264 RTP payloads are non-empty");
        Self {
            h264: Some(payload),
            marker,
            rtx: false,
            padding: false,
            twcc: false,
        }
    }

    pub const fn rtx() -> Self {
        Self {
            h264: None,
            marker: false,
            rtx: true,
            padding: false,
            twcc: false,
        }
    }

    pub const fn padding() -> Self {
        Self {
            h264: None,
            marker: false,
            rtx: false,
            padding: true,
            twcc: false,
        }
    }

    pub const fn twcc() -> Self {
        Self {
            h264: None,
            marker: false,
            rtx: false,
            padding: false,
            twcc: true,
        }
    }

    fn matches(self, semantic: PacketSemantic) -> bool {
        let flags = self.h264.map(h264::classify).unwrap_or_default();
        match semantic {
            PacketSemantic::Sps => flags.sps(),
            PacketSemantic::Pps => flags.pps(),
            PacketSemantic::Idr => flags.idr(),
            PacketSemantic::FuAFragment => self.h264.is_some_and(is_fua),
            PacketSemantic::Delta => self.h264.is_some_and(is_delta),
            PacketSemantic::Marker => self.marker,
            PacketSemantic::Rtx => self.rtx,
            PacketSemantic::Padding => self.padding,
            PacketSemantic::Twcc => self.twcc,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectedSemanticEvent<T> {
    pub semantic: PacketSemantic,
    pub identity: T,
    pub matching_ordinal: usize,
    pub matching_packets: usize,
}

pub fn select_semantic_event<'a, T: Copy + Debug>(
    seed: u64,
    semantic: PacketSemantic,
    packets: impl IntoIterator<Item = (T, PacketFacts<'a>)>,
) -> Option<SelectedSemanticEvent<T>> {
    let matching: Vec<T> = packets
        .into_iter()
        .filter_map(|(identity, facts)| facts.matches(semantic).then_some(identity))
        .collect();
    if matching.is_empty() {
        return None;
    }
    let mixed = splitmix(seed ^ semantic as u64);
    let ordinal = usize::try_from(mixed % u64::try_from(matching.len()).ok()?).ok()?;
    let event = SelectedSemanticEvent {
        semantic,
        identity: *matching.get(ordinal)?,
        matching_ordinal: ordinal,
        matching_packets: matching.len(),
    };
    tracing::info!(seed, ?event, "selected semantic impairment event");
    Some(event)
}

fn splitmix(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

fn is_fua(payload: &[u8]) -> bool {
    payload.len() >= 2 && payload[0] & 0x1f == 28
}

fn is_delta(payload: &[u8]) -> bool {
    let Some(&first) = payload.first() else {
        return false;
    };
    match first & 0x1f {
        1..=4 => true,
        24 => stapa_contains_delta(payload),
        28 | 29 => payload
            .get(1)
            .is_some_and(|header| (1..=4).contains(&(header & 0x1f))),
        _ => false,
    }
}

fn stapa_contains_delta(payload: &[u8]) -> bool {
    let mut offset = 1usize;
    while let Some(length) = payload
        .get(offset..offset.saturating_add(2))
        .and_then(|bytes| <[u8; 2]>::try_from(bytes).ok())
        .map(u16::from_be_bytes)
        .map(usize::from)
    {
        offset = offset.saturating_add(2);
        let Some(nalu) = payload.get(offset..offset.saturating_add(length)) else {
            return false;
        };
        if nalu
            .first()
            .is_some_and(|header| (1..=4).contains(&(header & 0x1f)))
        {
            return true;
        }
        if length == 0 {
            return false;
        }
        offset = offset.saturating_add(length);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_core::h264::Packetizer;
    use pulsebeam_testdata::{QualityVideoLayer, QualityVideoSource, quality_corpus_video};

    fn corpus_packets() -> Vec<((usize, usize), Vec<u8>, bool)> {
        let fixture = quality_corpus_video(QualityVideoSource::Zero, QualityVideoLayer::P720);
        let packetizer = Packetizer::new(1_100);
        (0..fixture.len())
            .map(|index| fixture.frame(index).expect("declared fixture frame"))
            .flat_map(|frame| {
                packetizer
                    .packetize(frame.encoded)
                    .into_iter()
                    .enumerate()
                    .map(move |(packet, chunk)| {
                        ((frame.index, packet), chunk.payload, chunk.end_of_frame)
                    })
            })
            .collect()
    }

    #[test]
    fn every_media_selector_matches_the_engineered_corpus() {
        let packets = corpus_packets();
        for semantic in [
            PacketSemantic::Sps,
            PacketSemantic::Pps,
            PacketSemantic::Idr,
            PacketSemantic::FuAFragment,
            PacketSemantic::Delta,
            PacketSemantic::Marker,
        ] {
            let event = select_semantic_event(
                41,
                semantic,
                packets.iter().map(|(identity, payload, marker)| {
                    (*identity, PacketFacts::h264(payload, *marker))
                }),
            )
            .unwrap_or_else(|| panic!("{semantic:?} selector did not match the corpus"));
            assert!(event.matching_packets > 0);
        }
    }

    #[test]
    fn transport_selectors_match_only_their_semantics() {
        let packets = [
            (10, PacketFacts::rtx()),
            (11, PacketFacts::padding()),
            (12, PacketFacts::twcc()),
        ];
        assert_eq!(
            select_semantic_event(9, PacketSemantic::Rtx, packets)
                .expect("RTX event")
                .identity,
            10
        );
        assert_eq!(
            select_semantic_event(9, PacketSemantic::Padding, packets)
                .expect("padding event")
                .identity,
            11
        );
        assert_eq!(
            select_semantic_event(9, PacketSemantic::Twcc, packets)
                .expect("TWCC event")
                .identity,
            12
        );
    }

    #[test]
    fn seed_replay_selects_the_same_semantic_event() {
        let packets = corpus_packets();
        let select = || {
            select_semantic_event(
                0x1234_5678,
                PacketSemantic::FuAFragment,
                packets.iter().map(|(identity, payload, marker)| {
                    (*identity, PacketFacts::h264(payload, *marker))
                }),
            )
            .expect("fragment event")
        };
        assert_eq!(select(), select());
    }
}
