//! Transport spellings at the SDP adapter boundary.

use alloc::{collections::BTreeMap, string::String};

use super::{
    CodecFormat,
    validation::{self, ValidationError},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodecParameters {
    pub name: String,
    pub clock_rate: u32,
    pub channels: Option<u16>,
    pub fmtp: BTreeMap<String, String>,
}

pub fn codec_parameters(format: &CodecFormat) -> Result<CodecParameters, ValidationError> {
    validation::validate_format(format)?;
    let (_, name) = format
        .mime_type
        .split_once('/')
        .ok_or(ValidationError::Format)?;
    let mut fmtp = super::format::canonical_parameters(format);
    match format.known_codec() {
        Some(super::codec_format::Codec::H264(_)) => {
            fmtp.entry("profile-level-id".into())
                .or_insert_with(|| "42e01f".into());
            fmtp.entry("level-asymmetry-allowed".into())
                .or_insert_with(|| "0".into());
        }
        Some(super::codec_format::Codec::Vp9(_)) => {
            fmtp.entry("profile-id".into())
                .or_insert_with(|| "0".into());
        }
        _ => {}
    }
    Ok(CodecParameters {
        name: name.into(),
        clock_rate: format.clock_rate,
        channels: format
            .channels
            .map(u16::try_from)
            .transpose()
            .map_err(|_| ValidationError::Format)?,
        fmtp,
    })
}

pub fn feedback_name(value: &str) -> Result<&str, ValidationError> {
    if value.is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
        return Err(ValidationError::Capability);
    }
    Ok(value)
}

pub fn extension_uri(value: &str) -> Result<&str, ValidationError> {
    if value.is_empty()
        || value.len() > 256
        || !value.contains(':')
        || value.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(ValidationError::Capability);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{OpusParameters, codec_format};

    #[test]
    fn optional_false_and_absence_keep_distinct_sdp_spellings() -> Result<(), ValidationError> {
        let mut opus = OpusParameters::default();
        let format = |opus| CodecFormat::from(codec_format::Codec::Opus(opus));
        let absent = codec_parameters(&format(opus))?;
        opus.stereo = Some(false);
        opus.in_band_fec = Some(true);
        let explicit = codec_parameters(&format(opus))?;
        assert!(!absent.fmtp.contains_key("stereo"));
        assert_eq!(explicit.fmtp.get("stereo").map(String::as_str), Some("0"));
        assert_eq!(
            explicit.fmtp.get("useinbandfec").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            (
                explicit.name.as_str(),
                explicit.clock_rate,
                explicit.channels
            ),
            ("opus", 48000, Some(2))
        );
        assert!(codec_parameters(&CodecFormat::default()).is_err());
        Ok(())
    }
}
