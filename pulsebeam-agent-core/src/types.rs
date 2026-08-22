use std::fmt;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TransportGeneration(pub u64);

impl TransportGeneration {
    pub const INITIAL: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u64 {
        self.0
    }

    pub fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

impl From<u64> for TransportGeneration {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RequestId(pub u64);

impl RequestId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u64 {
        self.0
    }
}

impl From<u64> for RequestId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MediaSlotId(pub u64);

impl MediaSlotId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u64 {
        self.0
    }
}

impl From<u64> for MediaSlotId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

macro_rules! owned_identifier {
    ($name:ident) => {
        #[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

owned_identifier!(ChannelKey);
owned_identifier!(ParticipantId);
owned_identifier!(TrackId);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaKind {
    Audio,
    Video,
    Data,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ConnectionState {
    #[default]
    Idle,
    Connecting,
    Connected,
    Reconnecting,
    Closing,
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconnectPolicy {
    pub max_attempts: u32,
    pub initial_delay: Duration,
    pub max_delay: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
        }
    }
}

impl ReconnectPolicy {
    pub fn delay_for(&self, attempt: u32) -> Duration {
        debug_assert!(attempt > 0);
        let exponent = attempt.saturating_sub(1).min(31);
        let delay = self.initial_delay.saturating_mul(1_u32 << exponent);
        delay.min(self.max_delay)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoreConfig {
    pub reconnect_policy: ReconnectPolicy,
    pub connect_timeout: Duration,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            reconnect_policy: ReconnectPolicy::default(),
            connect_timeout: Duration::from_secs(10),
        }
    }
}
