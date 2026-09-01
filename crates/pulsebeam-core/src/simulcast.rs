#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum LayerQuality {
    Low = 1,
    Medium = 2,
    High = 3,
}

impl std::fmt::Debug for LayerQuality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let txt = match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        };
        f.write_str(txt)
    }
}

impl LayerQuality {
    pub fn from_rid(rid: Option<&str>) -> Self {
        match rid {
            Some("f") => LayerQuality::High,
            Some("h") => LayerQuality::Medium,
            Some("q") => LayerQuality::Low,
            other => {
                debug_assert!(
                    other.is_none(),
                    "unrecognized simulcast rid {other:?}, defaulting to Low"
                );
                LayerQuality::Low
            }
        }
    }

    pub fn seed_bitrate_bps(self) -> u64 {
        match self {
            LayerQuality::High => 1_250_000,
            LayerQuality::Medium => 400_000,
            LayerQuality::Low => 150_000,
        }
    }

    pub fn fallback_height(self) -> u32 {
        match self {
            LayerQuality::High => 720,
            LayerQuality::Medium => 360,
            LayerQuality::Low => 180,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_rid_is_order_independent() {
        assert_eq!(LayerQuality::from_rid(Some("f")), LayerQuality::High);
        assert_eq!(LayerQuality::from_rid(Some("h")), LayerQuality::Medium);
        assert_eq!(LayerQuality::from_rid(Some("q")), LayerQuality::Low);
    }

    #[test]
    fn from_rid_none_defaults_to_low() {
        assert_eq!(LayerQuality::from_rid(None), LayerQuality::Low);
    }
}
