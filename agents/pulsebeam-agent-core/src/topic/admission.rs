#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, uniffi::Enum)]
pub enum DataSendFailure {
    #[error("data send was disconnected")]
    Disconnected,
    #[error("data send failed with the connection")]
    TerminalFailure { error: crate::ffi::ErrorInfo },
    #[error("data send admission timed out")]
    TimedOut,
    #[error("data send generation or authoritative binding was replaced")]
    TransportReplaced,
    #[error("data send channel failed")]
    ChannelFailed,
    #[error("data send was superseded by a newer value")]
    Superseded,
    #[error("data send was rejected by the host")]
    HostRejected,
    #[error("data send queue is full")]
    QueueFull,
    #[error("data send sequence space is exhausted")]
    SequenceExhausted,
}
