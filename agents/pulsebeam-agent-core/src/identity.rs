use alloc::string::String;

macro_rules! opaque_id {
    ($(#[$attribute:meta])* $name:ident) => {
        $(#[$attribute])*
        #[derive(Clone, Hash, PartialEq, Eq)]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }

            #[allow(
                dead_code,
                reason = "TrackId is reserved for IDs forwarded from later signaling messages"
            )]
            pub(crate) fn from_server(value: String) -> Self {
                Self(value)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }
    };
}

opaque_id!(
    /// An opaque room identifier returned by the server.
    ///
    /// Canonical parsing is deliberately unavailable:
    ///
    /// ```compile_fail
    /// use pulsebeam_agent_core::RoomId;
    ///
    /// let _: RoomId = "rm_00000000000000000000000000".parse().unwrap();
    /// ```
    RoomId
);
opaque_id!(ParticipantId);
opaque_id!(ConnectionId);
opaque_id!(TrackId);

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use super::*;

    #[test]
    fn server_values_are_preserved_exactly() {
        let value = "not/a canonical id".to_string();
        let id = ConnectionId::from_server(value.clone());

        assert_eq!(id.as_str(), value);
        assert!(id == id.clone());
    }
}
