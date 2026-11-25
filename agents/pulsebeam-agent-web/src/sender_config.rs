use agent_core::MediaKind;
use serde::Deserialize;

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct SenderConfig {
    pub(super) content_hint: String,
    pub(super) degradation_preference: Option<String>,
    #[serde(default)]
    pub(super) encodings: Vec<EncodingConfig>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct EncodingConfig {
    pub(super) rid: Option<String>,
    pub(super) active: bool,
    pub(super) scale_resolution_down_by: Option<f64>,
    pub(super) max_bitrate: Option<f64>,
    pub(super) max_framerate: Option<f64>,
    pub(super) scalability_mode: Option<String>,
    pub(super) dtx: Option<String>,
}

pub(super) fn normalize_sender_config(
    kind: MediaKind,
    mut config: SenderConfig,
) -> Result<SenderConfig, String> {
    if config.encodings.is_empty() {
        config.encodings.push(EncodingConfig {
            rid: None,
            active: true,
            scale_resolution_down_by: None,
            max_bitrate: None,
            max_framerate: None,
            scalability_mode: None,
            dtx: None,
        });
    }
    validate_sender_config(kind, &config)?;
    Ok(config)
}

fn validate_sender_config(kind: MediaKind, config: &SenderConfig) -> Result<(), String> {
    let valid_hint = match kind {
        MediaKind::Video => matches!(config.content_hint.as_str(), "motion" | "detail" | "text"),
        MediaKind::Audio => matches!(config.content_hint.as_str(), "speech" | "music"),
    };
    if !valid_hint {
        return Err(format!(
            "invalid {} content hint: {}",
            media_kind_name(kind),
            config.content_hint
        ));
    }
    let [encoding] = config.encodings.as_slice() else {
        return Err(format!(
            "{} publications require one encoding; publish separate tracks for independent streams",
            media_kind_name(kind)
        ));
    };
    if encoding.rid.is_some() {
        return Err("sender encoding must not name a simulcast RID".into());
    }
    for value in [
        encoding.scale_resolution_down_by,
        encoding.max_bitrate,
        encoding.max_framerate,
    ]
    .into_iter()
    .flatten()
    {
        if !value.is_finite() || value <= 0.0 {
            return Err("sender encoding values must be finite and positive".into());
        }
    }
    if encoding
        .scalability_mode
        .as_deref()
        .is_some_and(|mode| !matches!(mode, "L1T1" | "L1T2" | "L1T3"))
    {
        return Err("sender scalability mode must be L1T1, L1T2, or L1T3".into());
    }
    if encoding
        .dtx
        .as_deref()
        .is_some_and(|dtx| !matches!(dtx, "enabled" | "disabled"))
    {
        return Err("audio DTX must be enabled or disabled".into());
    }
    Ok(())
}

pub(super) fn media_kind_name(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Video => "video",
        MediaKind::Audio => "audio",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_video_has_one_active_encoding_without_forcing_scalability() {
        let config = normalize_sender_config(
            MediaKind::Video,
            SenderConfig {
                content_hint: "motion".into(),
                degradation_preference: None,
                encodings: Vec::new(),
            },
        )
        .unwrap();
        let [encoding] = config.encodings.as_slice() else {
            panic!("one video encoding");
        };
        assert!(encoding.active && encoding.rid.is_none() && encoding.scalability_mode.is_none());
        assert!(config.degradation_preference.is_none());
    }

    #[test]
    fn video_keeps_temporal_and_rate_options_but_rejects_multiple_encodings() {
        let encoding = EncodingConfig {
            rid: None,
            active: true,
            scale_resolution_down_by: None,
            max_bitrate: Some(300_000.0),
            max_framerate: Some(30.0),
            scalability_mode: Some("L1T3".into()),
            dtx: None,
        };
        let config = SenderConfig {
            content_hint: "motion".into(),
            degradation_preference: None,
            encodings: vec![encoding.clone()],
        };
        let mut accepted = normalize_sender_config(MediaKind::Video, config).unwrap();
        assert_eq!(accepted.encodings[0].max_bitrate, Some(300_000.0));
        assert_eq!(accepted.encodings[0].max_framerate, Some(30.0));
        assert_eq!(
            accepted.encodings[0].scalability_mode.as_deref(),
            Some("L1T3")
        );
        accepted.encodings.push(encoding);
        assert!(
            matches!(normalize_sender_config(MediaKind::Video, accepted), Err(error)
            if error.contains("require one encoding"))
        );
    }
}
