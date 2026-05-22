pub mod signaling;

pub mod namespace {
    pub enum Signaling {
        Reliable,
    }

    impl Signaling {
        pub fn as_str(&self) -> &str {
            match self {
                Self::Reliable => "__internal/v1/signaling",
            }
        }
    }
}

pub mod prelude {
    pub use prost::Message;
}
