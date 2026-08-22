use std::collections::VecDeque;
use std::fmt;

use crate::time::MonotonicTime;
use crate::types::{
    ChannelKey, ConnectionState, CoreConfig, ReconnectPolicy, RequestId, TransportGeneration,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CoreInput {
    Start,
    TransportConnected {
        generation: TransportGeneration,
    },
    TransportFailed {
        generation: TransportGeneration,
        reason: String,
    },
    TransportClosed {
        generation: TransportGeneration,
    },
    Send {
        generation: TransportGeneration,
        request_id: RequestId,
        channel: ChannelKey,
        payload: Vec<u8>,
    },
    Timer,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CoreEffect {
    Connect {
        generation: TransportGeneration,
    },
    Send {
        generation: TransportGeneration,
        request_id: RequestId,
        channel: ChannelKey,
        payload: Vec<u8>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CoreEvent {
    StateChanged {
        state: ConnectionState,
        generation: TransportGeneration,
    },
    ReconnectScheduled {
        generation: TransportGeneration,
        attempt: u32,
        deadline: MonotonicTime,
    },
    RequestAccepted {
        generation: TransportGeneration,
        request_id: RequestId,
    },
    TransportFailed {
        generation: TransportGeneration,
        reason: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CoreError {
    StaleGeneration {
        expected: TransportGeneration,
        received: TransportGeneration,
    },
    InvalidState {
        state: ConnectionState,
        operation: &'static str,
    },
    GenerationExhausted,
}

impl fmt::Display for CoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleGeneration { expected, received } => write!(
                formatter,
                "stale transport generation: expected {}, received {}",
                expected.0, received.0
            ),
            Self::InvalidState { state, operation } => {
                write!(
                    formatter,
                    "cannot {operation} while connection is {state:?}"
                )
            }
            Self::GenerationExhausted => formatter.write_str("transport generation exhausted"),
        }
    }
}

impl std::error::Error for CoreError {}

pub struct AgentCore {
    config: CoreConfig,
    state: ConnectionState,
    generation: TransportGeneration,
    reconnect_attempt: u32,
    deadline: Option<MonotonicTime>,
    effects: VecDeque<CoreEffect>,
    events: VecDeque<CoreEvent>,
}

impl AgentCore {
    pub fn new(config: CoreConfig) -> Self {
        Self {
            config,
            state: ConnectionState::Idle,
            generation: TransportGeneration::INITIAL,
            reconnect_attempt: 0,
            deadline: None,
            effects: VecDeque::new(),
            events: VecDeque::new(),
        }
    }

    pub fn state(&self) -> ConnectionState {
        self.state
    }

    pub fn generation(&self) -> TransportGeneration {
        self.generation
    }

    pub fn handle(&mut self, now: MonotonicTime, input: CoreInput) -> Result<(), CoreError> {
        match input {
            CoreInput::Start => self.start(),
            CoreInput::TransportConnected { generation } => {
                self.require_generation(generation)?;
                if self.state != ConnectionState::Connecting {
                    return Err(CoreError::InvalidState {
                        state: self.state,
                        operation: "accept a transport connection",
                    });
                }
                self.state = ConnectionState::Connected;
                self.reconnect_attempt = 0;
                self.deadline = None;
                self.emit_state_change();
                Ok(())
            }
            CoreInput::TransportFailed { generation, reason } => {
                self.require_generation(generation)?;
                self.transport_failed(now, reason)
            }
            CoreInput::TransportClosed { generation } => {
                self.require_generation(generation)?;
                self.transport_failed(now, String::from("transport closed"))
            }
            CoreInput::Send {
                generation,
                request_id,
                channel,
                payload,
            } => {
                self.require_generation(generation)?;
                if self.state != ConnectionState::Connected {
                    return Err(CoreError::InvalidState {
                        state: self.state,
                        operation: "send a request",
                    });
                }
                debug_assert!(!channel.as_str().is_empty());
                self.effects.push_back(CoreEffect::Send {
                    generation,
                    request_id,
                    channel,
                    payload,
                });
                self.events.push_back(CoreEvent::RequestAccepted {
                    generation,
                    request_id,
                });
                Ok(())
            }
            CoreInput::Timer => self.on_timer(now),
        }
    }

    pub fn poll_effect(&mut self) -> Option<CoreEffect> {
        self.effects.pop_front()
    }

    pub fn poll_event(&mut self) -> Option<CoreEvent> {
        self.events.pop_front()
    }

    pub fn next_deadline(&self) -> Option<MonotonicTime> {
        self.deadline
    }

    pub fn reconnect_policy(&self) -> &ReconnectPolicy {
        &self.config.reconnect_policy
    }

    fn start(&mut self) -> Result<(), CoreError> {
        if self.state != ConnectionState::Idle && self.state != ConnectionState::Closed {
            return Err(CoreError::InvalidState {
                state: self.state,
                operation: "start the transport",
            });
        }
        self.reconnect_attempt = 0;
        self.deadline = None;
        self.begin_connect()
            .inspect(|_| debug_assert!(self.next_deadline().is_none()))?;
        Ok(())
    }

    fn begin_connect(&mut self) -> Result<(), CoreError> {
        let generation = self
            .generation
            .next()
            .ok_or(CoreError::GenerationExhausted)?;
        self.generation = generation;
        self.state = ConnectionState::Connecting;
        self.deadline = None;
        self.effects.push_back(CoreEffect::Connect { generation });
        self.emit_state_change();
        Ok(())
    }

    fn transport_failed(&mut self, now: MonotonicTime, reason: String) -> Result<(), CoreError> {
        self.events.push_back(CoreEvent::TransportFailed {
            generation: self.generation,
            reason,
        });
        if self.reconnect_attempt >= self.config.reconnect_policy.max_attempts {
            self.deadline = None;
            self.state = ConnectionState::Closed;
            self.emit_state_change();
            return Ok(());
        }
        self.reconnect_attempt = self.reconnect_attempt.saturating_add(1);
        let delay = self
            .config
            .reconnect_policy
            .delay_for(self.reconnect_attempt);
        let deadline = now.saturating_add(delay);
        self.deadline = Some(deadline);
        self.state = ConnectionState::Reconnecting;
        self.events.push_back(CoreEvent::ReconnectScheduled {
            generation: self.generation,
            attempt: self.reconnect_attempt,
            deadline,
        });
        self.emit_state_change();
        Ok(())
    }

    fn on_timer(&mut self, now: MonotonicTime) -> Result<(), CoreError> {
        let Some(deadline) = self.deadline else {
            return Ok(());
        };
        if now < deadline {
            return Ok(());
        }
        debug_assert!(self.state == ConnectionState::Reconnecting);
        self.begin_connect()
    }

    fn require_generation(&self, received: TransportGeneration) -> Result<(), CoreError> {
        if received != self.generation {
            debug_assert_ne!(received, self.generation);
            return Err(CoreError::StaleGeneration {
                expected: self.generation,
                received,
            });
        }
        Ok(())
    }

    fn emit_state_change(&mut self) {
        self.events.push_back(CoreEvent::StateChanged {
            state: self.state,
            generation: self.generation,
        });
    }
}

impl Default for AgentCore {
    fn default() -> Self {
        Self::new(CoreConfig::default())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::test_utils::{channel, generation, request, time};

    #[test]
    fn effects_are_fifo() {
        let mut core = AgentCore::default();
        core.handle(time(0), CoreInput::Start).unwrap();
        let current = generation(1);
        assert_eq!(
            core.poll_effect(),
            Some(CoreEffect::Connect {
                generation: current
            })
        );
        core.handle(
            time(1),
            CoreInput::TransportConnected {
                generation: current,
            },
        )
        .unwrap();
        core.handle(
            time(2),
            CoreInput::Send {
                generation: current,
                request_id: request(1),
                channel: channel("control"),
                payload: vec![1],
            },
        )
        .unwrap();
        core.handle(
            time(3),
            CoreInput::Send {
                generation: current,
                request_id: request(2),
                channel: channel("control"),
                payload: vec![2],
            },
        )
        .unwrap();
        assert_eq!(
            core.poll_effect(),
            Some(CoreEffect::Send {
                generation: current,
                request_id: request(1),
                channel: channel("control"),
                payload: vec![1],
            })
        );
        assert_eq!(
            core.poll_effect(),
            Some(CoreEffect::Send {
                generation: current,
                request_id: request(2),
                channel: channel("control"),
                payload: vec![2],
            })
        );
        assert_eq!(core.poll_effect(), None);
    }

    #[test]
    fn reconnect_assigns_a_new_generation() {
        let mut core = AgentCore::default();
        core.handle(time(0), CoreInput::Start).unwrap();
        let first = generation(1);
        core.poll_effect();
        core.handle(
            time(1),
            CoreInput::TransportFailed {
                generation: first,
                reason: String::from("offline"),
            },
        )
        .unwrap();
        assert_eq!(core.state(), ConnectionState::Reconnecting);
        let deadline = core.next_deadline().unwrap();
        core.handle(deadline, CoreInput::Timer).unwrap();
        assert_eq!(
            core.poll_effect(),
            Some(CoreEffect::Connect {
                generation: generation(2)
            })
        );
        assert_eq!(core.generation(), generation(2));
    }

    #[test]
    fn stale_generation_is_rejected_without_side_effects() {
        let mut core = AgentCore::default();
        core.handle(time(0), CoreInput::Start).unwrap();
        core.poll_effect();
        core.handle(
            time(1),
            CoreInput::TransportFailed {
                generation: generation(1),
                reason: String::from("offline"),
            },
        )
        .unwrap();
        let deadline = core.next_deadline().unwrap();
        core.handle(deadline, CoreInput::Timer).unwrap();
        core.poll_effect();
        let result = core.handle(
            time(3),
            CoreInput::TransportConnected {
                generation: generation(1),
            },
        );
        assert_eq!(
            result,
            Err(CoreError::StaleGeneration {
                expected: generation(2),
                received: generation(1),
            })
        );
        assert_eq!(core.state(), ConnectionState::Connecting);
        assert_eq!(core.poll_effect(), None);
    }

    #[test]
    fn reconnect_deadline_is_explicit_and_due_only_once() {
        let mut core = AgentCore::new(CoreConfig {
            reconnect_policy: ReconnectPolicy {
                max_attempts: 2,
                initial_delay: Duration::from_millis(10),
                max_delay: Duration::from_millis(20),
            },
        });
        core.handle(time(100), CoreInput::Start).unwrap();
        core.poll_effect();
        core.handle(
            time(200),
            CoreInput::TransportClosed {
                generation: generation(1),
            },
        )
        .unwrap();
        assert_eq!(core.next_deadline(), Some(time(210)));
        core.handle(time(209), CoreInput::Timer).unwrap();
        assert_eq!(core.next_deadline(), Some(time(210)));
        core.handle(time(210), CoreInput::Timer).unwrap();
        assert_eq!(core.next_deadline(), None);
        assert_eq!(
            core.poll_effect(),
            Some(CoreEffect::Connect {
                generation: generation(2)
            })
        );
        core.handle(time(211), CoreInput::Timer).unwrap();
        assert_eq!(core.poll_effect(), None);
    }
}
