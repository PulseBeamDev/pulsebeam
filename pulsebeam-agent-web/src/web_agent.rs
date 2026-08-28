use alloc::string::String;

pub struct WebAgent {
    // private:
    // core: agent::Agent
    // browser transport/resources
    // pending browser operations
}

pub struct WebAgentConfig {
    pub endpoint: String,
}

// impl WebAgent {
//     pub fn new(config: WebAgentConfig) -> Result<Self, WebError> {
//         todo!()
//     }
//
//     /// Replace the complete browser-side desired state.
//     pub fn set_state(&mut self, state: WebAgentState) -> Result<(), WebError> {
//         todo!()
//     }
//
//     /// Current externally observable state.
//     pub fn connection_state(&self) -> ConnectionState {
//         todo!()
//     }
//
//     /// Application-facing notifications.
//     pub fn next_event(&mut self) -> Option<WebEvent> {
//         todo!()
//     }
// }
//
// pub struct WebAgentState {
//     pub connection: WebConnectionState,
//     pub client: WebClientState,
// }
//
// pub enum WebConnectionState {
//     Disconnected,
//
//     Connected { room: String, token: Option<String> },
// }
//
// pub struct WebClientState {
//     pub upstream: WebUpstreamState,
//     pub downstream: DownstreamState,
//     pub session: SessionState,
// }
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
