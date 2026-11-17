use super::*;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum NativeController {
    #[default]
    Gcc,
    ScreamV2,
}
impl NativeController {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Gcc => "native_gcc",
            Self::ScreamV2 => "native_screamv2",
        }
    }
}

#[cfg(test)]
pub(super) struct NativeOptions {
    pub(super) decode_video: bool,
    pub(super) raw_video: bool,
    pub(super) disable_frame_dropper: bool,
    pub(super) controller: NativeController,
    pub(super) initial_bps: u32,
    pub(super) minimum_bps: Option<u32>,
}
#[cfg(test)]
impl Default for NativeOptions {
    fn default() -> Self {
        Self {
            decode_video: false,
            raw_video: false,
            disable_frame_dropper: false,
            controller: NativeController::Gcc,
            initial_bps: 2_000_000,
            minimum_bps: None,
        }
    }
}

#[derive(Clone)]
pub(super) struct NativeTrials {
    pub(super) base: FieldTrialsSnapshot,
    pub(super) controller: NativeController,
    pub(super) disable_frame_dropper: bool,
}
impl FieldTrialsView for NativeTrials {
    fn lookup(&self, key: &str) -> String {
        match key {
            "WebRTC-Bwe-ScreamV2" => match self.controller {
                NativeController::Gcc => "Disabled".to_owned(),
                NativeController::ScreamV2 => "mode:always".to_owned(),
            },
            "WebRTC-FrameDropper" if self.disable_frame_dropper => "Disabled".to_owned(),
            _ => self.base.lookup(key),
        }
    }
    #[allow(
        clippy::disallowed_types,
        reason = "SDK trial-view contract requires Arc for this immutable setup-only snapshot; no packet state or per-packet ownership churn"
    )]
    fn create_copy(&self) -> std::sync::Arc<dyn FieldTrialsView> {
        std::sync::Arc::new(self.clone())
    }
}

#[derive(Clone)]
pub(super) struct ControllerLog(SyncSender<()>);
impl ControllerLog {
    pub(super) fn channel() -> (Self, Receiver<()>) {
        let (send, receive) = sync_channel(1);
        (Self(send), receive)
    }
}
impl RtcEventLogFactory for ControllerLog {
    fn create(&self, _: EventLogEnvironment) -> Box<dyn RtcEventLogger> {
        Box::new(Self(self.0.clone()))
    }
}
impl RtcEventLogger for ControllerLog {
    fn start(&self, _: EventLogOutput, _: EventLogOutputPeriod) -> bool {
        false
    }
    fn stop(&self) {}
    fn log(&self, event: RtcEvent) {
        if event.kind() == RtcEventKind::BweUpdateScream {
            let _ = self.0.try_send(());
        }
    }
}
