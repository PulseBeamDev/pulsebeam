use alloc::string::String;

pub struct WebAgent {
    core: agent_core::Agent,
}

pub struct WebAgentConfig {
    pub endpoint: String,
}

impl WebAgent {
    pub fn new(_config: WebAgentConfig) -> Self {
        todo!()
    }

    /// Replace the complete browser-side desired state.
    pub fn set_state(&mut self, state: WebAgentState) {
        self.core.set_state(state.into());
    }

    /// Current externally observable state.
    pub fn connection_state(&self) -> ConnectionState {
        todo!()
    }

    /// Application-facing notifications.
    pub fn next_event(&mut self) -> Option<WebEvent> {
        todo!()
    }
}

pub enum WebEvent {}

pub struct WebAgentState {
    pub connection: ConnectionState,
    pub client: WebClientState,
}

impl From<WebAgentState> for agent_core::ClientState {
    fn from(val: WebAgentState) -> Self {
        let connection = match val.connection {
            // TODO:
            ConnectionState::Connected { .. } => agent_core::ClientConnectionState::Connected,
            ConnectionState::Disconnected => agent_core::ClientConnectionState::Disconnected,
        };
        agent_core::ClientState { connection }
    }
}

pub enum ConnectionState {
    Disconnected,

    Connected { room: String, token: Option<String> },
}

pub struct WebClientState {
    // pub upstream: WebUpstreamState,
    // pub downstream: DownstreamState,
}
//
// pub struct WebUpstreamState {
//     pub tracks: Vec<WebUpstreamTrack>,
// }
//
// pub enum WebUpstreamTrack {
//     Audio {
//         key: String,
//         track: web_sys::MediaStreamTrack,
//         config: AudioSendConfig,
//     },
//
//     Video {
//         key: String,
//         track: web_sys::MediaStreamTrack,
//         config: VideoSendConfig,
//     },
//
//     Data {
//         key: String,
//         config: DataSendConfig,
//     },
// }
