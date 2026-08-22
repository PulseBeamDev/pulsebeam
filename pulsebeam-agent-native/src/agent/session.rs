use std::collections::BTreeMap;

use pulsebeam_agent_core::{
    AudioIntent, IntentError, IntentState, LatencyLock, LayerOption, SessionError, SessionEvent,
    SessionReducer, SessionSnapshot, StickyAllocation, StickyAllocator, TrackId, VideoIntent,
};

pub struct NativeSession {
    reducer: SessionReducer,
    intents: IntentState,
    latency: LatencyLock,
    allocator: StickyAllocator,
}

impl NativeSession {
    pub fn new() -> Self {
        Self {
            reducer: SessionReducer::new(),
            intents: IntentState::default(),
            latency: LatencyLock::default(),
            allocator: StickyAllocator::new(),
        }
    }

    pub fn apply_server_message(
        &mut self,
        bytes: &[u8],
    ) -> Result<Vec<SessionEvent>, SessionError> {
        self.reducer.apply_message(bytes)
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        self.reducer.snapshot()
    }

    pub fn set_video_intent(&mut self, intent: VideoIntent) {
        self.intents.set_video(intent);
    }

    pub fn set_audio_intent(&mut self, intent: AudioIntent) {
        self.intents.set_audio(intent);
    }

    pub fn set_publish_intent(
        &mut self,
        mid: impl Into<String>,
        active: bool,
    ) -> Result<(), IntentError> {
        self.intents.set_publish(mid, active)
    }

    pub fn lock_latency(&mut self, min_ms: u32, max_ms: u32) -> Result<(), IntentError> {
        self.intents.set_latency(&mut self.latency, min_ms, max_ms)
    }

    pub fn latency(&self) -> LatencyLock {
        self.latency
    }

    pub fn client_intent(&self) -> pulsebeam_proto::signaling::ClientIntent {
        self.intents.to_proto(self.latency.bounds())
    }

    pub fn allocate(
        &mut self,
        layers: &BTreeMap<TrackId, Vec<LayerOption>>,
        budget_bps: u64,
    ) -> Result<Vec<StickyAllocation>, IntentError> {
        let intents: Vec<VideoIntent> = self.intents.video().cloned().collect();
        self.allocator.allocate(&intents, layers, budget_bps)
    }
}

impl Default for NativeSession {
    fn default() -> Self {
        Self::new()
    }
}
