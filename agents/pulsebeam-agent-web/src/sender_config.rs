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
        config.encodings = match kind {
            MediaKind::Video => ["q", "h", "f"]
                .into_iter()
                .map(|rid| EncodingConfig {
                    rid: Some(rid.to_owned()),
                    active: true,
                    scale_resolution_down_by: None,
                    max_bitrate: None,
                    max_framerate: None,
                    scalability_mode: None,
                    dtx: None,
                })
                .collect(),
            MediaKind::Audio => vec![EncodingConfig {
                rid: None,
                active: true,
                scale_resolution_down_by: None,
                max_bitrate: None,
                max_framerate: None,
                scalability_mode: None,
                dtx: None,
            }],
        };
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
    let expected = match kind {
        MediaKind::Video => 3,
        MediaKind::Audio => 1,
    };
    if config.encodings.len() != expected {
        return Err(format!(
            "{} sender configuration requires {expected} encoding entries",
            media_kind_name(kind)
        ));
    }
    for encoding in &config.encodings {
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
    fn default_video_preserves_three_active_rids_without_forcing_scalability() {
        let config = normalize_sender_config(
            MediaKind::Video,
            SenderConfig {
                content_hint: "motion".into(),
                degradation_preference: None,
                encodings: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(
            config
                .encodings
                .iter()
                .map(|encoding| encoding.rid.as_deref())
                .collect::<Vec<_>>(),
            [Some("q"), Some("h"), Some("f")],
        );
        assert!(config.degradation_preference.is_none());
        assert!(
            config
                .encodings
                .iter()
                .all(|encoding| { encoding.active && encoding.scalability_mode.is_none() })
        );
    }
}
