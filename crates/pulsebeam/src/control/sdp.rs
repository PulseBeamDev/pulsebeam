use crate::entity::{ParticipantId, RoomId, TrackId};
use pulsebeam_rtc::MediaKind;

#[derive(Clone)]
pub(crate) struct MediaTracks<T> {
    audio: Option<T>,
    video: Option<T>,
}

impl<T> Default for MediaTracks<T> {
    fn default() -> Self {
        Self {
            audio: None,
            video: None,
        }
    }
}

impl<T> MediaTracks<T> {
    pub(super) fn get(&self, kind: MediaKind) -> Option<&T> {
        match kind {
            MediaKind::Audio => self.audio.as_ref(),
            MediaKind::Video => self.video.as_ref(),
        }
    }

    pub(super) fn insert(&mut self, kind: MediaKind, track: T) -> Result<(), InvalidQuery> {
        let slot = match kind {
            MediaKind::Audio => &mut self.audio,
            MediaKind::Video => &mut self.video,
        };
        if slot.is_some() {
            return Err(InvalidQuery);
        }
        *slot = Some(track);
        Ok(())
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (MediaKind, &T)> {
        [MediaKind::Audio, MediaKind::Video]
            .into_iter()
            .filter_map(|kind| self.get(kind).map(|track| (kind, track)))
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SourceAddress {
    pub(super) room: RoomId,
    pub(super) participant: ParticipantId,
    pub(super) track: TrackId,
}

#[derive(Clone)]
pub(crate) enum Selection {
    Native,
    Whip(MediaTracks<TrackId>),
    Whep(MediaTracks<SourceAddress>),
}

impl std::fmt::Debug for Selection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Native => "Native",
            Self::Whip(_) => "Whip([redacted])",
            Self::Whep(_) => "Whep([redacted])",
        })
    }
}

pub(crate) fn codec_format(
    codec: pulsebeam_rtc::CodecInfo,
) -> Option<pulsebeam_proto::state::CodecFormat> {
    use pulsebeam_proto::state::{
        CodecFormat, H264Parameters, OpusParameters, Vp8Parameters, Vp9Parameters,
        codec_format::Codec,
    };
    use pulsebeam_rtc::CodecProfile;
    let codec = match codec.profile {
        CodecProfile::Opus if codec.clock_rate == 48_000 && codec.channels == Some(2) => {
            Codec::Opus(OpusParameters::default())
        }
        CodecProfile::Vp8 if codec.clock_rate == 90_000 && codec.channels.is_none() => {
            Codec::Vp8(Vp8Parameters {})
        }
        CodecProfile::Vp9 { profile_id: 0 }
            if codec.clock_rate == 90_000 && codec.channels.is_none() =>
        {
            Codec::Vp9(Vp9Parameters {
                profile_id: Some(0),
            })
        }
        CodecProfile::H264 {
            profile_level_id,
            packetization_mode: 1,
            level_asymmetry_allowed,
        } if codec.clock_rate == 90_000 => Codec::H264(H264Parameters {
            profile_level_id,
            level_asymmetry_allowed: Some(level_asymmetry_allowed),
        }),
        _ => return None,
    };
    Some(CodecFormat::from(codec))
}

pub(super) fn codec_compatible(
    source: &pulsebeam_proto::state::CodecFormat,
    receiving: pulsebeam_rtc::CodecInfo,
) -> bool {
    use pulsebeam_proto::state::codec_format::Codec;
    let Some(receiving) = codec_format(receiving).and_then(|format| format.known_codec()) else {
        return false;
    };
    match (source.known_codec(), receiving) {
        (Some(Codec::Opus(_)), Codec::Opus(_)) | (Some(Codec::Vp8(_)), Codec::Vp8(_)) => true,
        (Some(Codec::Vp9(source)), Codec::Vp9(receiver)) => {
            source.profile_id.unwrap_or(0) == receiver.profile_id.unwrap_or(0)
        }
        (Some(Codec::H264(source)), Codec::H264(receiver)) => {
            source.profile_level_id.unwrap_or(0x42e01f)
                == receiver.profile_level_id.unwrap_or(0x42e01f)
        }
        _ => false,
    }
}

#[derive(Debug)]
pub(super) struct InvalidQuery;

const MAX_QUERY_PARAMETERS: usize = 3;
const MAX_QUERY_BYTES: usize = 1024;

fn parameters(
    query: Option<&str>,
) -> Result<impl Iterator<Item = Result<(String, String), InvalidQuery>>, InvalidQuery> {
    let query = query.ok_or(InvalidQuery)?;
    if query.is_empty() || query.len() > MAX_QUERY_BYTES {
        return Err(InvalidQuery);
    }
    Ok(query.split('&').enumerate().map(|(index, parameter)| {
        if index >= MAX_QUERY_PARAMETERS {
            return Err(InvalidQuery);
        }
        let (key, value) = parameter.split_once('=').ok_or(InvalidQuery)?;
        Ok((decode(key)?, decode(value)?))
    }))
}

fn decode(value: &str) -> Result<String, InvalidQuery> {
    let mut decoded = Vec::with_capacity(value.len());
    let mut bytes = value.bytes();
    let hex = |byte: u8| {
        char::from(byte)
            .to_digit(16)
            .and_then(|digit| u8::try_from(digit).ok())
    };
    while let Some(byte) = bytes.next() {
        decoded.push(match byte {
            b'%' => {
                let high = bytes.next().and_then(hex).ok_or(InvalidQuery)?;
                let low = bytes.next().and_then(hex).ok_or(InvalidQuery)?;
                high << 4 | low
            }
            b'+' => b' ',
            _ => byte,
        });
    }
    String::from_utf8(decoded).map_err(|_| InvalidQuery)
}

fn media_kind(value: &str) -> Result<MediaKind, InvalidQuery> {
    match value {
        "audio" => Ok(MediaKind::Audio),
        "video" => Ok(MediaKind::Video),
        _ => Err(InvalidQuery),
    }
}

pub(super) fn whip_query(
    query: Option<&str>,
) -> Result<(RoomId, MediaTracks<TrackId>), InvalidQuery> {
    let mut room = None;
    let mut tracks = MediaTracks::default();
    for parameter in parameters(query)? {
        let (key, value) = parameter?;
        match key.as_str() {
            "room" if room.is_none() => room = Some(RoomId::new(&value).map_err(|_| InvalidQuery)?),
            "track" => {
                let (kind, track) = value.split_once('.').ok_or(InvalidQuery)?;
                tracks.insert(
                    media_kind(kind)?,
                    TrackId::new(track).map_err(|_| InvalidQuery)?,
                )?;
            }
            _ => return Err(InvalidQuery),
        }
    }
    Ok((room.ok_or(InvalidQuery)?, tracks))
}

pub(super) fn validate_mids(offer: &str) -> Result<(), InvalidQuery> {
    let mut media = false;
    let mut has_mid = false;
    let mut seen = std::collections::HashSet::new();
    for line in offer.lines() {
        if line.starts_with("m=") {
            if media && !has_mid {
                return Err(InvalidQuery);
            }
            media = true;
            has_mid = false;
        } else if let Some(mid) = line.strip_prefix("a=mid:") {
            if !media || has_mid || mid.is_empty() || !mid.bytes().all(|byte| matches!(byte,
                0x21 | 0x23..=0x27 | 0x2a..=0x2b | 0x2d..=0x2e | 0x30..=0x39 | 0x41..=0x5a | 0x5e..=0x7e
            )) || !seen.insert(mid) {
                return Err(InvalidQuery);
            }
            has_mid = true;
        }
    }
    if !media || !has_mid {
        return Err(InvalidQuery);
    }
    Ok(())
}

pub(super) fn whep_query(query: Option<&str>) -> Result<MediaTracks<SourceAddress>, InvalidQuery> {
    let mut tracks = MediaTracks::default();
    for parameter in parameters(query)? {
        let (key, value) = parameter?;
        if key != "track" {
            return Err(InvalidQuery);
        }
        let mut components = value.split('.');
        let [Some(room), Some(participant), Some(kind), Some(track), None] =
            std::array::from_fn(|_| components.next())
        else {
            return Err(InvalidQuery);
        };
        tracks.insert(
            media_kind(kind)?,
            SourceAddress {
                room: RoomId::new(room).map_err(|_| InvalidQuery)?,
                participant: ParticipantId::new(participant).map_err(|_| InvalidQuery)?,
                track: TrackId::new(track).map_err(|_| InvalidQuery)?,
            },
        )?;
    }
    Ok(tracks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors_are_bounded_exact_and_decoded_once() {
        let (room, tracks) =
            whip_query(Some("room=Stage&track=video%2ECamera&track=audio.mic")).unwrap();
        assert_eq!(room.as_str(), "Stage");
        assert_eq!(tracks.get(MediaKind::Video).unwrap().as_str(), "Camera");
        assert_eq!(tracks.get(MediaKind::Audio).unwrap().as_str(), "mic");
        let tracks = whep_query(Some(
            "track=Stage.alice.video.Camera&track=Studio.bob.audio.mic",
        ))
        .unwrap();
        let video = tracks.get(MediaKind::Video).unwrap();
        assert_eq!(video.room.as_str(), "Stage");
        assert_eq!(video.participant.as_str(), "alice");
        assert_eq!(video.track.as_str(), "Camera");
        for query in [
            "room=Stage",
            "track=Stage.alice.video",
            "track=project.Stage.alice.video.Camera",
            "track=Stage.alice.video.Camera&track=Stage.bob.video.Other",
            "track=Stage.alice.video.%2543amera",
            "track=Stage.alice.video.Cam+Era",
            "track=Stage.alice.video.Camera&something=else",
        ] {
            assert!(whep_query(Some(query)).is_err(), "{query}");
        }
        for query in [
            "track=video.Camera",
            "room=Stage&track=video.Camera&track=video.Other",
            "room=Stage&track=screen.Camera",
            "room=Stage&track=video.Camera&track=audio.mic&unknown=1",
        ] {
            assert!(whip_query(Some(query)).is_err(), "{query}");
        }
        assert!(whip_query(Some(&"a".repeat(MAX_QUERY_BYTES + 1))).is_err());
    }

    #[test]
    fn each_media_section_requires_one_distinct_mid() {
        assert!(validate_mids("m=audio\r\na=mid:Mic\r\nm=video\r\na=mid:Camera\r\n").is_ok());
        for offer in [
            "m=audio\na=mid:Mic\nm=video\na=mid:Mic\n",
            "m=audio\na=mid:Mic\na=mid:Second\n",
            "m=audio\na=mid:two words\n",
            "m=audio\na=mid:\n",
            "m=audio\n",
        ] {
            assert!(validate_mids(offer).is_err(), "{offer}");
        }
    }
}
