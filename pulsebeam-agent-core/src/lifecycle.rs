use std::collections::VecDeque;
use std::fmt;

use crate::http::{HttpRequest, HttpResponse};
use crate::time::MonotonicTime;
use crate::types::{CoreConfig, ReconnectPolicy, RequestId, TransportGeneration};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LifecycleState {
    #[default]
    Idle,
    Joining,
    Connected,
    Reconnecting,
    Closing,
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LifecycleInput {
    Start {
        request: HttpRequest,
    },
    HttpResponse {
        generation: TransportGeneration,
        request_id: RequestId,
        response: HttpResponse,
    },
    TransportClosed {
        generation: TransportGeneration,
    },
    Timer,
    Close,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LifecycleEffect {
    HttpRequest {
        generation: TransportGeneration,
        request_id: RequestId,
        request: HttpRequest,
    },
    CloseTransport {
        generation: TransportGeneration,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LifecycleEvent {
    StateChanged {
        state: LifecycleState,
        generation: TransportGeneration,
    },
    Connected {
        generation: TransportGeneration,
        response: HttpResponse,
    },
    RetryScheduled {
        generation: TransportGeneration,
        attempt: u32,
        deadline: MonotonicTime,
    },
    RequestRejected {
        generation: TransportGeneration,
        request_id: RequestId,
        status: u16,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LifecycleError {
    StaleGeneration {
        expected: TransportGeneration,
        received: TransportGeneration,
    },
    StaleRequest {
        expected: RequestId,
        received: RequestId,
    },
    InvalidState {
        state: LifecycleState,
        operation: &'static str,
    },
    GenerationExhausted,
    RequestExhausted,
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleGeneration { expected, received } => write!(
                formatter,
                "stale lifecycle generation: expected {}, received {}",
                expected.0, received.0
            ),
            Self::StaleRequest { expected, received } => write!(
                formatter,
                "stale lifecycle request: expected {}, received {}",
                expected.0, received.0
            ),
            Self::InvalidState { state, operation } => {
                write!(formatter, "cannot {operation} while lifecycle is {state:?}")
            }
            Self::GenerationExhausted => formatter.write_str("lifecycle generation exhausted"),
            Self::RequestExhausted => formatter.write_str("lifecycle request id exhausted"),
        }
    }
}

impl std::error::Error for LifecycleError {}

pub struct Lifecycle {
    config: CoreConfig,
    state: LifecycleState,
    generation: TransportGeneration,
    request_id: Option<RequestId>,
    next_request_id: u64,
    reconnect_attempt: u32,
    deadline: Option<MonotonicTime>,
    request: Option<HttpRequest>,
    effects: VecDeque<LifecycleEffect>,
    events: VecDeque<LifecycleEvent>,
}

impl Lifecycle {
    pub fn new(config: CoreConfig) -> Self {
        Self {
            config,
            state: LifecycleState::Idle,
            generation: TransportGeneration::INITIAL,
            request_id: None,
            next_request_id: 1,
            reconnect_attempt: 0,
            deadline: None,
            request: None,
            effects: VecDeque::new(),
            events: VecDeque::new(),
        }
    }

    pub fn state(&self) -> LifecycleState {
        self.state
    }

    pub fn generation(&self) -> TransportGeneration {
        self.generation
    }

    pub fn next_deadline(&self) -> Option<MonotonicTime> {
        self.deadline
    }

    pub fn reconnect_policy(&self) -> &ReconnectPolicy {
        &self.config.reconnect_policy
    }

    pub fn handle(
        &mut self,
        now: MonotonicTime,
        input: LifecycleInput,
    ) -> Result<(), LifecycleError> {
        match input {
            LifecycleInput::Start { request } => self.start(request),
            LifecycleInput::HttpResponse {
                generation,
                request_id,
                response,
            } => {
                self.require_generation(generation)?;
                self.require_request(request_id)?;
                if self.state != LifecycleState::Joining
                    && self.state != LifecycleState::Reconnecting
                {
                    return Err(LifecycleError::InvalidState {
                        state: self.state,
                        operation: "accept an HTTP response",
                    });
                }
                if response.is_success() {
                    self.state = LifecycleState::Connected;
                    self.reconnect_attempt = 0;
                    self.deadline = None;
                    self.events.push_back(LifecycleEvent::Connected {
                        generation,
                        response,
                    });
                    self.emit_state();
                    return Ok(());
                }
                let status = response.status;
                self.events.push_back(LifecycleEvent::RequestRejected {
                    generation,
                    request_id,
                    status,
                });
                self.schedule_retry(now)
            }
            LifecycleInput::TransportClosed { generation } => {
                self.require_generation(generation)?;
                if self.state == LifecycleState::Closing {
                    self.state = LifecycleState::Closed;
                    self.deadline = None;
                    self.emit_state();
                    return Ok(());
                }
                if self.state != LifecycleState::Connected {
                    return Err(LifecycleError::InvalidState {
                        state: self.state,
                        operation: "handle transport closure",
                    });
                }
                self.schedule_retry(now)
            }
            LifecycleInput::Timer => self.on_timer(now),
            LifecycleInput::Close => {
                if self.state == LifecycleState::Closed || self.state == LifecycleState::Idle {
                    return Ok(());
                }
                self.state = LifecycleState::Closing;
                self.deadline = None;
                self.effects.push_back(LifecycleEffect::CloseTransport {
                    generation: self.generation,
                });
                self.emit_state();
                Ok(())
            }
        }
    }

    pub fn poll_effect(&mut self) -> Option<LifecycleEffect> {
        self.effects.pop_front()
    }

    pub fn poll_event(&mut self) -> Option<LifecycleEvent> {
        self.events.pop_front()
    }

    fn start(&mut self, request: HttpRequest) -> Result<(), LifecycleError> {
        if self.state != LifecycleState::Idle && self.state != LifecycleState::Closed {
            return Err(LifecycleError::InvalidState {
                state: self.state,
                operation: "start lifecycle",
            });
        }
        debug_assert!(!request.uri.is_empty());
        self.request = Some(request);
        self.reconnect_attempt = 0;
        self.deadline = None;
        self.begin_request(LifecycleState::Joining)
    }

    fn begin_request(&mut self, state: LifecycleState) -> Result<(), LifecycleError> {
        let generation = self
            .generation
            .next()
            .ok_or(LifecycleError::GenerationExhausted)?;
        let request_id = RequestId::new(self.next_request_id);
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or(LifecycleError::RequestExhausted)?;
        let Some(request) = self.request.clone() else {
            debug_assert!(
                false,
                "lifecycle request must exist before a transport request"
            );
            return Err(LifecycleError::InvalidState {
                state: self.state,
                operation: "create an HTTP request",
            });
        };
        self.generation = generation;
        self.request_id = Some(request_id);
        self.state = state;
        self.deadline = None;
        self.effects.push_back(LifecycleEffect::HttpRequest {
            generation,
            request_id,
            request,
        });
        self.emit_state();
        Ok(())
    }

    fn schedule_retry(&mut self, now: MonotonicTime) -> Result<(), LifecycleError> {
        if self.reconnect_attempt >= self.config.reconnect_policy.max_attempts {
            self.state = LifecycleState::Closed;
            self.deadline = None;
            self.emit_state();
            return Ok(());
        }
        self.reconnect_attempt = self.reconnect_attempt.saturating_add(1);
        let deadline = now.saturating_add(
            self.config
                .reconnect_policy
                .delay_for(self.reconnect_attempt),
        );
        self.deadline = Some(deadline);
        self.state = LifecycleState::Reconnecting;
        self.events.push_back(LifecycleEvent::RetryScheduled {
            generation: self.generation,
            attempt: self.reconnect_attempt,
            deadline,
        });
        self.emit_state();
        Ok(())
    }

    fn on_timer(&mut self, now: MonotonicTime) -> Result<(), LifecycleError> {
        let Some(deadline) = self.deadline else {
            return Ok(());
        };
        if now < deadline {
            return Ok(());
        }
        debug_assert_eq!(self.state, LifecycleState::Reconnecting);
        self.begin_request(LifecycleState::Reconnecting)
    }

    fn require_generation(&self, received: TransportGeneration) -> Result<(), LifecycleError> {
        if received != self.generation {
            debug_assert_ne!(received, self.generation);
            return Err(LifecycleError::StaleGeneration {
                expected: self.generation,
                received,
            });
        }
        Ok(())
    }

    fn require_request(&self, received: RequestId) -> Result<(), LifecycleError> {
        let Some(expected) = self.request_id else {
            return Err(LifecycleError::StaleRequest {
                expected: RequestId::new(0),
                received,
            });
        };
        if received != expected {
            debug_assert_ne!(received, expected);
            return Err(LifecycleError::StaleRequest { expected, received });
        }
        Ok(())
    }

    fn emit_state(&mut self) {
        self.events.push_back(LifecycleEvent::StateChanged {
            state: self.state,
            generation: self.generation,
        });
    }
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self::new(CoreConfig::default())
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::http::{HttpMethod, HttpRequest};
    use crate::test_utils::time;
    use crate::types::ReconnectPolicy;

    fn request() -> HttpRequest {
        HttpRequest::new(HttpMethod::Post, "/join", vec![1])
    }

    #[test]
    fn stale_response_cannot_connect_a_new_generation() {
        let mut lifecycle = Lifecycle::default();
        lifecycle
            .handle(time(0), LifecycleInput::Start { request: request() })
            .unwrap();
        let effect = lifecycle.poll_effect().unwrap();
        let LifecycleEffect::HttpRequest {
            generation,
            request_id,
            ..
        } = effect
        else {
            panic!("expected HTTP request")
        };
        lifecycle
            .handle(
                time(1),
                LifecycleInput::HttpResponse {
                    generation,
                    request_id,
                    response: HttpResponse::new(503, vec![]),
                },
            )
            .unwrap();
        let deadline = lifecycle.next_deadline().unwrap();
        lifecycle.handle(deadline, LifecycleInput::Timer).unwrap();
        assert_eq!(lifecycle.generation(), TransportGeneration::new(2));
        let result = lifecycle.handle(
            time(2),
            LifecycleInput::HttpResponse {
                generation,
                request_id,
                response: HttpResponse::new(200, vec![]),
            },
        );
        assert!(matches!(
            result,
            Err(LifecycleError::StaleGeneration { .. })
        ));
        assert_eq!(lifecycle.state(), LifecycleState::Reconnecting);
    }

    #[test]
    fn backoff_deadline_is_deterministic() {
        let mut lifecycle = Lifecycle::new(CoreConfig {
            reconnect_policy: ReconnectPolicy {
                max_attempts: 2,
                initial_delay: Duration::from_millis(5),
                max_delay: Duration::from_millis(20),
            },
        });
        lifecycle
            .handle(time(100), LifecycleInput::Start { request: request() })
            .unwrap();
        let LifecycleEffect::HttpRequest {
            generation,
            request_id,
            ..
        } = lifecycle.poll_effect().unwrap()
        else {
            panic!("expected HTTP request")
        };
        lifecycle
            .handle(
                time(200),
                LifecycleInput::HttpResponse {
                    generation,
                    request_id,
                    response: HttpResponse::new(500, vec![]),
                },
            )
            .unwrap();
        assert_eq!(lifecycle.next_deadline(), Some(time(205)));
    }
}
