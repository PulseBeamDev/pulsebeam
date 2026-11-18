use super::*;
use crate::{CredentialOutcome, CredentialStatus};

fn authorization_etag(response: &HttpResponse) -> Option<u64> {
    let mut headers = response
        .headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("etag"));
    let value = &headers.next()?.value;
    if headers.next().is_some() {
        return None;
    }
    let value = value.strip_prefix("\"auth-")?.strip_suffix('"')?;
    let revision = value.parse::<u64>().ok()?;
    (value == revision.to_string()).then_some(revision)
}

pub(super) struct AdmissionDenial {
    revision: u64,
    join_revision: u64,
    mode: AttemptMode,
}

impl Agent {
    pub(super) fn reject_admission_credential(
        &mut self,
        revision: u64,
        mode: AttemptMode,
        failure: Failure,
    ) {
        self.admission_denial = Some(AdmissionDenial {
            revision,
            join_revision: self.snapshot.join_revision,
            mode,
        });
        self.credential_outcome(revision, CredentialStatus::Rejected, Some(failure.clone()));
        self.fail_attempt(failure);
    }

    pub(super) fn fail_uncertain_admission(&mut self, revision: u64, failure: Failure) {
        self.admission_denial = None;
        self.credential_outcome(revision, CredentialStatus::Uncertain, Some(failure.clone()));
        self.fail_attempt(failure);
    }

    pub(super) fn retry_denied_admission(&mut self) -> bool {
        if !self.desired.connected
            || self.attempt.is_some()
            || self.closing.is_some()
            || self
                .snapshot
                .terminal_failure
                .as_ref()
                .is_some_and(|failure| failure.class != FailureClass::Authorization)
        {
            return false;
        }
        let Some(denial) = self.admission_denial.as_ref().filter(|denial| {
            denial.revision < self.config_credential_revision
                && denial.join_revision == self.snapshot.join_revision
        }) else {
            return false;
        };
        let mode = denial.mode;
        self.admission_denial = None;
        self.snapshot.terminal_failure = None;
        self.retry_attempts = 0;
        self.start_attempt(mode);
        true
    }

    pub(super) fn renew_authorization(&mut self, token: String) -> Result<(), AgentError> {
        if self.participant_superseded() {
            return Err(AgentError::Replaced);
        }
        crate::model::validate_authorization_token(&token)
            .map_err(|_| AgentError::InvalidAuthorizationToken)?;
        let pending = self
            .active
            .as_ref()
            .and_then(|active| active.pending_renewal.as_ref());
        let retry_settled = pending.is_none()
            && self
                .snapshot
                .credential_outcome
                .as_ref()
                .is_some_and(|outcome| {
                    outcome.revision == self.config_credential_revision
                        && outcome.status != CredentialStatus::Accepted
                });
        if self.config.token != token || retry_settled {
            self.config_credential_revision = self.advance_credential_revision()?;
            self.config.token = token;
        }
        if let Some(pending) = self
            .active
            .as_mut()
            .and_then(|active| active.pending_renewal.as_mut())
            && pending.operation.is_none()
            && !pending.accepted
            && pending.attempts >= self.config.retry.maximum_attempts
        {
            pending.attempts = 0;
        }
        self.retry_denied_admission();
        self.request_reconciliation();
        Ok(())
    }

    pub(super) fn credential_outcome(
        &mut self,
        revision: u64,
        status: CredentialStatus,
        error: Option<Failure>,
    ) {
        if self
            .snapshot
            .credential_outcome
            .as_ref()
            .is_some_and(|current| current.revision > revision)
        {
            return;
        }
        let outcome = CredentialOutcome {
            revision,
            status,
            error,
        };
        if self.snapshot.credential_outcome.as_ref() != Some(&outcome) {
            self.snapshot.credential_outcome = Some(outcome);
            self.bump_snapshot();
            self.notifications.push_back(Notification::SnapshotChanged);
        }
    }

    pub(super) fn credential_accepted(&mut self, revision: u64) {
        self.credential_outcome(revision, CredentialStatus::Accepted, None);
    }

    pub(super) fn reconcile_credentials(&mut self) {
        if !self.desired.connected {
            return;
        }
        let Some(active) = self
            .active
            .as_mut()
            .filter(|active| !active.signaling_terminal)
        else {
            return;
        };
        if active.pending_renewal.is_none() {
            if active.credential.revision == self.config_credential_revision
                || self
                    .snapshot
                    .credential_outcome
                    .as_ref()
                    .is_some_and(|outcome| outcome.revision == self.config_credential_revision)
            {
                return;
            }
            let Some(grant) = active
                .state
                .session()
                .and_then(|session| session.authorization.as_ref())
            else {
                return;
            };
            let Some(expected_revision) = grant
                .revision
                .max(active.authorization_revision)
                .checked_add(1)
            else {
                self.credential_outcome(
                    self.config_credential_revision,
                    CredentialStatus::Uncertain,
                    Some(Failure {
                        class: FailureClass::RetryExhausted,
                        message: "authorization revision space is exhausted".into(),
                    }),
                );
                return;
            };
            active.pending_renewal = Some(native::Renewal {
                credential: Credential {
                    token: self.config.token.clone(),
                    revision: self.config_credential_revision,
                },
                operation: None,
                expected_revision,
                accepted: false,
                retry: None,
                attempts: 0,
                uncertain: false,
                issued: false,
            });
        }
        let Some(pending) = active.pending_renewal.as_mut() else {
            return;
        };
        if !pending.issued && pending.credential.revision != self.config_credential_revision {
            pending.credential = Credential {
                token: self.config.token.clone(),
                revision: self.config_credential_revision,
            };
            pending.uncertain = false;
            pending.attempts = 0;
        }
        if pending.accepted
            || pending.operation.is_some()
            || pending.retry.is_some()
            || pending.attempts >= self.config.retry.maximum_attempts
        {
            return;
        }
        let operation = self.ids.operation();
        pending.operation = Some(operation);
        pending.issued = true;
        pending.attempts = pending.attempts.saturating_add(1);
        self.effects.push_back(Effect::Http(HttpEffect::Request {
            operation,
            generation: Some(active.generation),
            request: HttpRequest {
                method: HttpMethod::Patch,
                uri: active.resource_uri.clone(),
                headers: vec![
                    HttpHeader {
                        name: "Authorization".into(),
                        value: format!("Bearer {}", pending.credential.token),
                    },
                    HttpHeader {
                        name: "If-Match".into(),
                        value: format!("\"auth-{}\"", pending.expected_revision.saturating_sub(1)),
                    },
                ],
                body: Vec::new(),
            },
        }));
    }

    pub(super) fn credential_http_failed(&mut self, operation: OperationId) -> bool {
        let Some(pending) = self
            .active
            .as_mut()
            .and_then(|active| active.pending_renewal.as_mut())
            .filter(|pending| pending.operation == Some(operation))
        else {
            return false;
        };
        pending.operation = None;
        pending.uncertain = true;
        self.retry_credential();
        true
    }

    pub(super) fn credential_http_response(
        &mut self,
        operation: OperationId,
        response: &HttpResponse,
    ) -> bool {
        let Some(active) = self.active.as_mut() else {
            return false;
        };
        let Some(pending) = active
            .pending_renewal
            .as_mut()
            .filter(|pending| pending.operation == Some(operation))
        else {
            return false;
        };
        pending.operation = None;
        if response.status == 204
            && response.body.is_empty()
            && authorization_etag(response) == Some(pending.expected_revision)
        {
            pending.accepted = true;
            self.commit_renewal();
        } else if response.status == 412 {
            // Only the controller's monotonic precondition fence permits rebasing an unknown request.
            let current = authorization_etag(response)
                .filter(|revision| *revision >= pending.expected_revision)
                .map(|revision| revision.max(active.authorization_revision));
            if let Some(current) = current.and_then(|current| current.checked_add(1)) {
                active.authorization_revision =
                    active.authorization_revision.max(current.saturating_sub(1));
                let revision = pending.credential.revision;
                self.credential_outcome(
                    revision,
                    CredentialStatus::Uncertain,
                    Some(Failure::transient(
                        "authorization precondition changed; credential acceptance is uncertain",
                    )),
                );
                if let Some(pending) = self
                    .active
                    .as_mut()
                    .and_then(|active| active.pending_renewal.as_mut())
                {
                    pending.expected_revision = current;
                    pending.issued = false;
                    if pending.credential.revision != self.config_credential_revision {
                        pending.credential = Credential {
                            token: self.config.token.clone(),
                            revision: self.config_credential_revision,
                        };
                        pending.uncertain = false;
                        pending.attempts = 0;
                    }
                }
            } else {
                pending.uncertain = true;
            }
            self.retry_credential();
        } else if (400..500).contains(&response.status) && !matches!(response.status, 408 | 429) {
            let revision = pending.credential.revision;
            let status = if pending.uncertain {
                CredentialStatus::Uncertain
            } else {
                CredentialStatus::Rejected
            };
            active.pending_renewal = None;
            self.credential_outcome(
                revision,
                status,
                Some(Failure {
                    class: FailureClass::Authorization,
                    message: "authorization replacement rejected".into(),
                }),
            );
            self.request_reconciliation();
        } else {
            pending.uncertain = true;
            self.retry_credential();
        }
        true
    }

    fn retry_credential(&mut self) {
        let Some(pending) = self
            .active
            .as_mut()
            .and_then(|active| active.pending_renewal.as_mut())
        else {
            return;
        };
        let revision = pending.credential.revision;
        let exhausted = pending.attempts >= self.config.retry.maximum_attempts;
        if !exhausted && pending.retry.is_none() {
            let timer = self.ids.timer();
            pending.retry = Some(timer);
            let multiplier = 1u32
                .checked_shl(u32::from(pending.attempts.saturating_sub(1).min(10)))
                .unwrap_or(u32::MAX);
            let after = self
                .config
                .retry
                .initial_delay
                .checked_mul(multiplier)
                .unwrap_or(self.config.retry.maximum_delay)
                .min(self.config.retry.maximum_delay);
            self.effects
                .push_back(Effect::Timer(TimerEffect::Schedule { timer, after }));
        }
        if !pending.uncertain && !exhausted {
            return;
        }
        self.credential_outcome(
            revision,
            CredentialStatus::Uncertain,
            Some(Failure {
                class: if exhausted {
                    FailureClass::RetryExhausted
                } else {
                    FailureClass::Transient
                },
                message: if exhausted {
                    "authorization replacement retry budget exhausted; outcome is uncertain"
                } else {
                    "authorization replacement outcome is uncertain"
                }
                .into(),
            }),
        );
    }

    pub(super) fn credential_timer(&mut self, timer: TimerId) -> bool {
        let Some(pending) = self
            .active
            .as_mut()
            .and_then(|active| active.pending_renewal.as_mut())
            .filter(|pending| pending.retry == Some(timer))
        else {
            return false;
        };
        pending.retry = None;
        self.request_reconciliation();
        true
    }

    pub(super) fn cancel_credential_work(&mut self, pending: native::Renewal) {
        if let Some(operation) = pending.operation {
            self.effects
                .push_back(Effect::Http(HttpEffect::Cancel { operation }));
        }
        if let Some(timer) = pending.retry {
            self.effects
                .push_back(Effect::Timer(TimerEffect::Cancel { timer }));
        }
    }
}
