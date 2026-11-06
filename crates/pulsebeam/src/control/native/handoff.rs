use core::num::NonZeroUsize;
use std::sync::atomic::Ordering;

use pulsebeam_queue as mailbox;
use pulsebeam_runtime::time::Instant;

use super::{ExecutionCommand, ExecutionCompletion, ExecutionDeferral};
use crate::entity::ConnectionId;
use crate::media::ChannelId;
use crate::participant::policy::ResourceObservation;
use crate::participant::signaling::{SignalingOutput, SignalingProgress};

const INPUT_FRAMES: usize = 32;
const FACTS: usize = 256;
const COMMANDS: usize = 32;
const READY: u8 = 1;
const FAILED: u8 = 2;
const ENDPOINT_READY: u8 = 4;

pub(crate) fn describe_metrics() {
    metrics::describe_counter!(
        "native_signaling_handoff_pressure",
        "native signaling send attempts blocked by capacity, by lane"
    );
    metrics::describe_counter!(
        "native_signaling_handoff_failure",
        "terminal native signaling failure events, by lane"
    );
    metrics::describe_histogram!(
        "native_signaling_execution_observation_us",
        metrics::Unit::Microseconds,
        "time per native preparation attempt from start to endpoint completion or deferral observation; excludes unobserved attempts"
    );
    metrics::describe_histogram!(
        "native_signaling_output_admission_us",
        metrics::Unit::Microseconds,
        "time from selecting a native signaling output to RTC send admission"
    );
}

#[cfg(test)]
mod tests;

pub(crate) enum Fact {
    Completion(ExecutionCompletion),
    Deferral(ExecutionDeferral),
    Resource(ResourceObservation),
    ReconstructionRequired,
}

#[derive(Clone, Copy)]
pub(crate) struct Opened {
    pub channel: ChannelId,
    pub at: Instant,
}

// One control-rate wake/failure word per connection, never touched by media or topic forwarding.
#[expect(
    clippy::disallowed_types,
    reason = "Independent per-connection native control readiness state, not packet state or a multi-atomic snapshot"
)]
struct Readiness {
    state: std::sync::atomic::AtomicU8,
    connection: ConnectionId,
    sender: mailbox::Sender<ConnectionId>,
}

#[expect(
    clippy::disallowed_types,
    reason = "Two lifetime owners of one connection-local control wake word; cloned only at connection construction, never per message or packet"
)]
type Signal = std::sync::Arc<Readiness>;

impl Readiness {
    fn notify(&self) -> bool {
        let previous = self.state.fetch_or(READY, Ordering::AcqRel);
        if previous & FAILED != 0 {
            return false;
        }
        if previous & READY != 0 {
            return true;
        }
        if self.sender.try_send(self.connection).is_err() {
            if self.state.fetch_or(FAILED, Ordering::AcqRel) & FAILED == 0 {
                metrics::counter!("native_signaling_handoff_failure", "lane" => "readiness")
                    .increment(1);
            }
            return false;
        }
        true
    }

    fn fail(&self) {
        let previous = self.state.fetch_or(READY | FAILED, Ordering::AcqRel);
        if previous & READY == 0 {
            let _ = self.sender.try_send(self.connection);
        }
    }

    fn fail_lane(&self, lane: &'static str) {
        let previous = self.state.fetch_or(READY | FAILED, Ordering::AcqRel);
        if previous & FAILED == 0 {
            metrics::counter!("native_signaling_handoff_failure", "lane" => lane).increment(1);
        }
        if previous & READY == 0 {
            let _ = self.sender.try_send(self.connection);
        }
    }

    fn failed(&self) -> bool {
        self.state.load(Ordering::Acquire) & FAILED != 0
    }
}

pub(crate) struct ControllerHandoff {
    input: mailbox::Receiver<Vec<u8>>,
    facts: mailbox::Receiver<Fact>,
    opened: mailbox::Receiver<Opened>,
    progress: mailbox::Receiver<SignalingProgress>,
    commands: mailbox::Sender<ExecutionCommand>,
    output: mailbox::Sender<SignalingOutput>,
    next_input: Option<Vec<u8>>,
    next_fact: Option<Fact>,
    next_opened: Option<Opened>,
    next_progress: Option<SignalingProgress>,
    signal: Signal,
}

pub(crate) struct EndpointHandoff {
    input: mailbox::Sender<Vec<u8>>,
    facts: mailbox::Sender<Fact>,
    opened: mailbox::Sender<Opened>,
    progress: mailbox::Sender<SignalingProgress>,
    commands: mailbox::Receiver<ExecutionCommand>,
    output: mailbox::Receiver<SignalingOutput>,
    first_opened: Option<ChannelId>,
    next_command: Option<ExecutionCommand>,
    signal: Signal,
}

pub(crate) fn pair(
    connection: ConnectionId,
    readiness: mailbox::Sender<ConnectionId>,
) -> (ControllerHandoff, EndpointHandoff) {
    let (input_tx, input_rx) = mailbox::bounded(const { NonZeroUsize::new(INPUT_FRAMES).unwrap() });
    let (facts_tx, facts_rx) = mailbox::bounded(const { NonZeroUsize::new(FACTS).unwrap() });
    let (opened_tx, opened_rx) = mailbox::bounded(NonZeroUsize::MIN);
    let (progress_tx, progress_rx) = mailbox::bounded(NonZeroUsize::MIN);
    let (commands_tx, commands_rx) =
        mailbox::bounded(const { NonZeroUsize::new(COMMANDS).unwrap() });
    let (output_tx, output_rx) = mailbox::bounded(NonZeroUsize::MIN);
    let signal = Signal::new(Readiness {
        state: 0.into(),
        connection,
        sender: readiness,
    });
    let controller = ControllerHandoff {
        input: input_rx,
        facts: facts_rx,
        opened: opened_rx,
        progress: progress_rx,
        commands: commands_tx,
        output: output_tx,
        next_input: None,
        next_fact: None,
        next_opened: None,
        next_progress: None,
        signal: signal.clone(),
    };
    let endpoint = EndpointHandoff {
        input: input_tx,
        facts: facts_tx,
        opened: opened_tx,
        progress: progress_tx,
        commands: commands_rx,
        output: output_rx,
        first_opened: None,
        next_command: None,
        signal,
    };
    (controller, endpoint)
}

impl ControllerHandoff {
    pub(crate) fn stop(&self) {
        self.signal.fail();
    }

    pub(crate) fn schedule(&self) -> bool {
        self.signal.notify()
    }

    // One metadata-only shard wake may be outstanding, including in controller egress.
    pub(crate) fn wake_endpoint(&self) -> bool {
        self.signal.state.fetch_or(ENDPOINT_READY, Ordering::AcqRel) & ENDPOINT_READY == 0
    }

    pub(crate) fn input(&mut self) -> Option<Vec<u8>> {
        self.next_input
            .take()
            .or_else(|| self.input.try_recv().ok())
    }

    pub(crate) fn fact(&mut self) -> Option<Fact> {
        self.next_fact.take().or_else(|| self.facts.try_recv().ok())
    }

    pub(crate) fn opened(&mut self) -> Option<Opened> {
        self.next_opened
            .take()
            .or_else(|| self.opened.try_recv().ok())
    }

    pub(crate) fn progress(&mut self) -> Option<SignalingProgress> {
        self.next_progress
            .take()
            .or_else(|| self.progress.try_recv().ok())
    }

    pub(crate) fn command(&self, command: ExecutionCommand) -> Result<(), ExecutionCommand> {
        if self.failed() {
            return Err(command);
        }
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mailbox::TrySendError::Full(command) => {
                    metrics::counter!("native_signaling_handoff_pressure", "lane" => "command")
                        .increment(1);
                    command
                }
                mailbox::TrySendError::Closed(command) => command,
            })
    }

    pub(crate) fn output(&self, output: SignalingOutput) -> Result<(), SignalingOutput> {
        if self.failed() || output.bytes.len() > pulsebeam_proto::state::codec::MAX_FRAME_SIZE {
            return Err(output);
        }
        self.output.try_send(output).map_err(|error| match error {
            mailbox::TrySendError::Full(output) => {
                metrics::counter!("native_signaling_handoff_pressure", "lane" => "output")
                    .increment(1);
                output
            }
            mailbox::TrySendError::Closed(output) => output,
        })
    }

    pub(crate) fn failed(&self) -> bool {
        self.signal.failed()
    }

    // Clear before peeking: a concurrent producer either wakes us or leaves work we reschedule.
    pub(crate) fn rearm(&mut self) -> bool {
        self.rearm_for(true)
    }

    pub(crate) fn rearm_for(&mut self, input_ready: bool) -> bool {
        self.signal.state.fetch_and(!READY, Ordering::AcqRel);
        self.next_opened = self.opened();
        self.next_progress = self.progress();
        self.next_fact = self.fact();
        self.next_input = self.input();
        if self.next_opened.is_some()
            || self.next_progress.is_some()
            || self.next_fact.is_some()
            || (input_ready && self.next_input.is_some())
        {
            self.signal.notify()
        } else {
            !self.failed()
        }
    }
}

impl std::fmt::Debug for EndpointHandoff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndpointHandoff").finish_non_exhaustive()
    }
}

impl EndpointHandoff {
    pub(crate) fn acknowledge_wakeup(&self) {
        self.signal
            .state
            .fetch_and(!ENDPOINT_READY, Ordering::AcqRel);
    }

    pub(crate) fn commands_pending(&mut self) -> bool {
        if self.next_command.is_none() {
            self.next_command = self.receive_command();
        }
        self.next_command.is_some()
    }

    // False is terminal for this connection, never permission to discard and continue.
    #[must_use]
    pub(crate) fn input(&self, bytes: impl AsRef<[u8]>) -> bool {
        let bytes = bytes.as_ref();
        // Bound before copying; RTC's refcounted receive allocation stays on its owning core.
        if bytes.len() > pulsebeam_proto::state::codec::MAX_FRAME_SIZE
            || self.input.try_send(bytes.to_vec()).is_err()
        {
            self.signal.fail_lane("input");
            return false;
        }
        #[cfg(feature = "sim")]
        crate::sim_metrics::record_stalled_native_input();
        self.signal.notify()
    }

    #[must_use]
    pub(crate) fn fact(&self, fact: Fact) -> bool {
        if self.facts.try_send(fact).is_err() {
            self.signal.fail_lane("fact");
            return false;
        }
        self.signal.notify()
    }

    #[must_use]
    pub(crate) fn opened(&mut self, opened: Opened) -> bool {
        if let Some(channel) = self.first_opened {
            if channel != opened.channel {
                self.signal.fail_lane("opened");
                return false;
            }
            return !self.failed();
        }
        if self.opened.try_send(opened).is_err() {
            self.signal.fail_lane("opened");
            return false;
        }
        self.first_opened = Some(opened.channel);
        self.signal.notify()
    }

    #[must_use]
    pub(crate) fn progress(&self, progress: SignalingProgress) -> bool {
        if self.progress.try_send(progress).is_err() {
            self.signal.fail_lane("progress");
            return false;
        }
        self.signal.notify()
    }

    pub(crate) fn command(&mut self) -> Option<ExecutionCommand> {
        if self.failed() {
            return None;
        }
        self.next_command.take().or_else(|| self.receive_command())
    }

    fn receive_command(&mut self) -> Option<ExecutionCommand> {
        let command = self.commands.try_recv().ok()?;
        // Capacity readiness, not an execution acknowledgement or success observation.
        self.signal.notify();
        Some(command)
    }

    pub(crate) fn output(&mut self) -> Option<SignalingOutput> {
        if self.failed() {
            return None;
        }
        self.output.try_recv().ok()
    }

    pub(crate) fn failed(&self) -> bool {
        self.signal.failed()
    }
}
