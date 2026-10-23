use std::{fmt, ops::Deref, str::FromStr};

pub use pulsebeam_rtc::{
    DataChannelConfig as ChannelConfig, DataChannelId as ChannelId, DataReliability as Reliability,
    MediaKind,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    SendOnly,
    RecvOnly,
    SendRecv,
    Inactive,
}

impl From<pulsebeam_rtc::MediaDirection> for Direction {
    fn from(value: pulsebeam_rtc::MediaDirection) -> Self {
        match value {
            pulsebeam_rtc::MediaDirection::SendOnly => Self::SendOnly,
            pulsebeam_rtc::MediaDirection::ReceiveOnly => Self::RecvOnly,
            pulsebeam_rtc::MediaDirection::Bidirectional => Self::SendRecv,
            pulsebeam_rtc::MediaDirection::Inactive => Self::Inactive,
        }
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::SendOnly => "sendonly",
            Self::RecvOnly => "recvonly",
            Self::SendRecv => "sendrecv",
            Self::Inactive => "inactive",
        })
    }
}

#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("invalid negotiated media token")]
pub struct MediaTokenError;

macro_rules! media_token {
    ($name:ident, $capacity:expr) => {
        #[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name([u8; $capacity]);

        impl $name {
            pub fn new(value: &str) -> Result<Self, MediaTokenError> {
                if value.is_empty()
                    || value.len() > $capacity
                    || !value.bytes().all(|byte| byte.is_ascii_graphic())
                {
                    return Err(MediaTokenError);
                }
                let mut bytes = [0; $capacity];
                bytes
                    .get_mut(..value.len())
                    .ok_or(MediaTokenError)?
                    .copy_from_slice(value.as_bytes());
                Ok(Self(bytes))
            }

            pub fn as_str(&self) -> &str {
                let end = self
                    .0
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or($capacity);
                std::str::from_utf8(self.0.get(..end).unwrap_or_default()).unwrap_or_default()
            }
        }

        impl Deref for $name {
            type Target = str;
            fn deref(&self) -> &str {
                self.as_str()
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl FromStr for $name {
            type Err = MediaTokenError;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.as_str().fmt(f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple(stringify!($name))
                    .field(&self.as_str())
                    .finish()
            }
        }

        #[cfg(test)]
        #[allow(
            clippy::expect_used,
            reason = "test-authored media tokens must be valid"
        )]
        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value).expect("test media token")
            }
        }
    };
}

media_token!(Mid, 32);
media_token!(Rid, 32);

macro_rules! media_number {
    ($name:ident, $value:ty) => {
        #[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name($value);

        impl From<$value> for $name {
            fn from(value: $value) -> Self {
                Self(value)
            }
        }

        impl From<$name> for $value {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl Deref for $name {
            type Target = $value;
            fn deref(&self) -> &$value {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

media_number!(SeqNo, u64);
media_number!(Ssrc, u32);
media_number!(Pt, u8);

impl Pt {
    pub const fn new_with_value(value: u8) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyframeRequestKind {
    Pli,
    Fir,
}

#[derive(Clone, Copy, Debug)]
pub struct KeyframeRequest {
    pub mid: Mid,
    pub rid: Option<Rid>,
    pub kind: KeyframeRequestKind,
}

#[derive(Clone, Copy, Debug)]
pub struct SimulcastLayer {
    pub rid: Rid,
}

impl SimulcastLayer {
    #[cfg(test)]
    pub fn new(rid: &str) -> Self {
        Self {
            rid: Rid::from(rid),
        }
    }
}
