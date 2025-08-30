use futures_lite::FutureExt;
use std::any::Any;
use std::fmt::{Debug, Display, Formatter};
use std::panic::{AssertUnwindSafe, RefUnwindSafe, UnwindSafe};
use std::sync::Arc;
use thiserror::Error;
use tracing::Instrument;

use crate::mailbox;

#[derive(Error, Debug)]
pub enum ActorError {
    #[error("Actor logic encountered an error: {0}")]
    LogicError(String),
    #[error("Custom actor error: {0}")]
    Custom(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorStatus {
    Starting,
    Running,
    ExitedGracefully, // run_actor_logic returned Ok
    ExitedWithError,  // run_actor_logic returned Err
    Panicked,
    ShutDown, // Successfully completed all stages it attempted
}

impl Display for ActorStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                ActorStatus::Starting => "starting",
                ActorStatus::Running => "running",
                ActorStatus::ExitedGracefully => "exited_gracefully",
                ActorStatus::ExitedWithError => "exited_with_error",
                ActorStatus::Panicked => "panicked",
                ActorStatus::ShutDown => "shut_down",
            }
        )
    }
}

// High-performance interceptor traits designed for production use
pub trait MessageInterceptor<A: Actor>: Send + Sync + 'static {
    /// Intercept high priority messages. Return Continue for normal processing.
    /// This method should be extremely fast as it's on the critical path.
    #[inline]
    fn intercept_hi_message(
        &self,
        actor_id: &A::ID,
        message: &A::HighPriorityMessage,
    ) -> InterceptAction {
        let _ = (actor_id, message); // Avoid unused parameter warnings
        InterceptAction::Continue
    }

    /// Intercept low priority messages. Return Continue for normal processing.
    /// This method should be extremely fast as it's on the critical path.
    #[inline]
    fn intercept_lo_message(
        &self,
        actor_id: &A::ID,
        message: &A::LowPriorityMessage,
    ) -> InterceptAction {
        let _ = (actor_id, message); // Avoid unused parameter warnings
        InterceptAction::Continue
    }
}

pub trait ActorInterceptor<A: Actor>: Send + Sync + 'static {
    /// Called before actor starts running. Fast path - should return quickly.
    #[inline]
    fn before_run(&self, actor_id: &A::ID) {
        let _ = actor_id; // Avoid unused parameter warnings
    }

    /// Called after actor finishes. Fast path - should return quickly.
    #[inline]
    fn after_run(&self, actor_id: &A::ID, status: ActorStatus) {
        let _ = (actor_id, status); // Avoid unused parameter warnings
    }

    /// Called when actor encounters an error. Fast path - should return quickly.
    #[inline]
    fn on_error(&self, actor_id: &A::ID, error: &ActorError) {
        let _ = (actor_id, error); // Avoid unused parameter warnings
    }

    /// Called when actor panics. Fast path - should return quickly.
    #[inline]
    fn on_panic(&self, actor_id: &A::ID, panic_msg: &str) {
        let _ = (actor_id, panic_msg); // Avoid unused parameter warnings
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterceptAction {
    Continue,   // Let the message through normally
    Drop,       // Drop the message silently
    Delay(u64), // Delay message by N milliseconds
    Transform,  // For future extension - transform the message
}

// Zero-overhead interceptor registry with fast paths
pub struct InterceptorRegistry<A: Actor> {
    message_interceptors: Vec<Arc<dyn MessageInterceptor<A>>>,
    actor_interceptors: Vec<Arc<dyn ActorInterceptor<A>>>,
}

impl<A: Actor> InterceptorRegistry<A> {
    /// Create a new empty registry
    #[inline]
    pub const fn new() -> Self {
        Self {
            message_interceptors: Vec::new(),
            actor_interceptors: Vec::new(),
        }
    }

    /// Add a message interceptor
    pub fn add_message_interceptor(&mut self, interceptor: Arc<dyn MessageInterceptor<A>>) {
        self.message_interceptors.push(interceptor);
    }

    /// Add an actor lifecycle interceptor
    pub fn add_actor_interceptor(&mut self, interceptor: Arc<dyn ActorInterceptor<A>>) {
        self.actor_interceptors.push(interceptor);
    }

    /// Fast path check - inlined and optimized away when empty
    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.message_interceptors.is_empty() && self.actor_interceptors.is_empty()
    }

    /// Fast path for message interception - inlined for performance
    #[inline]
    fn intercept_hi_message(
        &self,
        actor_id: &A::ID,
        message: &A::HighPriorityMessage,
    ) -> InterceptAction {
        // Fast path: no interceptors
        if self.message_interceptors.is_empty() {
            return InterceptAction::Continue;
        }

        // Process interceptors - early exit on non-Continue
        for interceptor in &self.message_interceptors {
            match interceptor.intercept_hi_message(actor_id, message) {
                InterceptAction::Continue => continue,
                other => return other,
            }
        }
        InterceptAction::Continue
    }

    /// Fast path for message interception - inlined for performance  
    #[inline]
    fn intercept_lo_message(
        &self,
        actor_id: &A::ID,
        message: &A::LowPriorityMessage,
    ) -> InterceptAction {
        // Fast path: no interceptors
        if self.message_interceptors.is_empty() {
            return InterceptAction::Continue;
        }

        // Process interceptors - early exit on non-Continue
        for interceptor in &self.message_interceptors {
            match interceptor.intercept_lo_message(actor_id, message) {
                InterceptAction::Continue => continue,
                other => return other,
            }
        }
        InterceptAction::Continue
    }

    /// Fast path for lifecycle events - inlined for performance
    #[inline]
    fn before_run(&self, actor_id: &A::ID) {
        if !self.actor_interceptors.is_empty() {
            for interceptor in &self.actor_interceptors {
                interceptor.before_run(actor_id);
            }
        }
    }

    #[inline]
    fn after_run(&self, actor_id: &A::ID, status: ActorStatus) {
        if !self.actor_interceptors.is_empty() {
            for interceptor in &self.actor_interceptors {
                interceptor.after_run(actor_id, status);
            }
        }
    }

    #[inline]
    fn on_error(&self, actor_id: &A::ID, error: &ActorError) {
        if !self.actor_interceptors.is_empty() {
            for interceptor in &self.actor_interceptors {
                interceptor.on_error(actor_id, error);
            }
        }
    }

    #[inline]
    fn on_panic(&self, actor_id: &A::ID, panic_msg: &str) {
        if !self.actor_interceptors.is_empty() {
            for interceptor in &self.actor_interceptors {
                interceptor.on_panic(actor_id, panic_msg);
            }
        }
    }
}

impl<A: Actor> Default for InterceptorRegistry<A> {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ActorContext<A: Actor> {
    pub hi_rx: mailbox::Receiver<A::HighPriorityMessage>,
    pub lo_rx: mailbox::Receiver<A::LowPriorityMessage>,
    pub handle: LocalActorHandle<A>,
}

pub trait Actor: Send + Sized + 'static {
    type HighPriorityMessage: Send + 'static;
    type LowPriorityMessage: Send + 'static;
    type ID: Eq
        + std::hash::Hash
        + Display
        + Debug
        + Clone
        + Send
        + Sync
        + UnwindSafe
        + RefUnwindSafe
        + 'static;

    fn kind(&self) -> &'static str;
    fn id(&self) -> Self::ID;

    fn run(
        &mut self,
        ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = Result<(), ActorError>> + Send + Sync;
}

pub trait ActorHandle<A: Actor>: Clone + Send + Sync + 'static {
    fn lo_send(
        &self,
        message: A::LowPriorityMessage,
    ) -> impl Future<Output = Result<(), mailbox::SendError<A::LowPriorityMessage>>>;

    fn lo_try_send(
        &self,
        message: A::LowPriorityMessage,
    ) -> Result<(), mailbox::TrySendError<A::LowPriorityMessage>>;

    fn hi_send(
        &self,
        message: A::HighPriorityMessage,
    ) -> impl Future<Output = Result<(), mailbox::SendError<A::HighPriorityMessage>>>;

    fn hi_try_send(
        &self,
        message: A::HighPriorityMessage,
    ) -> Result<(), mailbox::TrySendError<A::HighPriorityMessage>>;
}

// High-performance actor factory trait
pub trait ActorFactory<A: Actor>: Send + Sync + 'static {
    fn create_actor(&self, actor: A, config: RunnerConfig) -> (LocalActorHandle<A>, Runner<A>);
}

// Zero-overhead factory for actors without interceptors
pub struct NoInterceptorFactory;

impl<A: Actor> ActorFactory<A> for NoInterceptorFactory {
    #[inline(always)]
    fn create_actor(&self, actor: A, config: RunnerConfig) -> (LocalActorHandle<A>, Runner<A>) {
        LocalActorHandle::new(actor, config)
    }
}

// Factory with interceptors - still high performance
pub struct InterceptorFactory<A: Actor> {
    interceptors: Arc<InterceptorRegistry<A>>,
}

impl<A: Actor> InterceptorFactory<A> {
    pub fn new(interceptors: Arc<InterceptorRegistry<A>>) -> Self {
        Self { interceptors }
    }
}

impl<A: Actor> ActorFactory<A> for InterceptorFactory<A> {
    #[inline]
    fn create_actor(&self, actor: A, config: RunnerConfig) -> (LocalActorHandle<A>, Runner<A>) {
        LocalActorHandle::with_interceptors(actor, config, self.interceptors.clone())
    }
}

pub struct LocalActorHandle<A: Actor> {
    hi_tx: mailbox::Sender<A::HighPriorityMessage>,
    lo_tx: mailbox::Sender<A::LowPriorityMessage>,
    actor_id: A::ID,
    interceptors: Arc<InterceptorRegistry<A>>,
}

impl<A: Actor> LocalActorHandle<A> {
    /// Create a new actor handle with no interceptors - zero overhead
    #[inline]
    pub fn new(actor: A, config: RunnerConfig) -> (Self, Runner<A>) {
        Self::with_interceptors(actor, config, Arc::new(InterceptorRegistry::new()))
    }

    /// Create a new actor handle with interceptors - optimized for performance
    pub fn with_interceptors(
        actor: A,
        config: RunnerConfig,
        interceptors: Arc<InterceptorRegistry<A>>,
    ) -> (Self, Runner<A>) {
        let (lo_tx, lo_rx) = mailbox::new(config.lo_cap);
        let (hi_tx, hi_rx) = mailbox::new(config.hi_cap);

        let actor_id = actor.id();

        let handle = LocalActorHandle {
            lo_tx,
            hi_tx,
            actor_id: actor_id.clone(),
            interceptors: interceptors.clone(),
        };

        let ctx = ActorContext {
            hi_rx,
            lo_rx,
            handle: handle.clone(),
        };

        let runner = Runner {
            actor,
            ctx,
            interceptors,
        };

        (handle, runner)
    }
}

impl<A: Actor> Clone for LocalActorHandle<A> {
    fn clone(&self) -> Self {
        Self {
            hi_tx: self.hi_tx.clone(),
            lo_tx: self.lo_tx.clone(),
            actor_id: self.actor_id.clone(),
            interceptors: self.interceptors.clone(),
        }
    }
}

impl<A: Actor> ActorHandle<A> for LocalActorHandle<A> {
    #[inline]
    async fn lo_send(
        &self,
        message: A::LowPriorityMessage,
    ) -> Result<(), mailbox::SendError<A::LowPriorityMessage>> {
        match self
            .interceptors
            .intercept_lo_message(&self.actor_id, &message)
        {
            InterceptAction::Drop => Ok(()),
            InterceptAction::Delay(ms) => {
                tokio::time::sleep(tokio::time::Duration::from_millis(ms)).await;
                self.lo_tx.send(message).await
            }
            InterceptAction::Continue | InterceptAction::Transform => {
                self.lo_tx.send(message).await
            }
        }
    }

    #[inline]
    fn lo_try_send(
        &self,
        message: A::LowPriorityMessage,
    ) -> Result<(), mailbox::TrySendError<A::LowPriorityMessage>> {
        match self
            .interceptors
            .intercept_lo_message(&self.actor_id, &message)
        {
            InterceptAction::Drop => Ok(()),
            InterceptAction::Delay(_) => {
                // For try_send, we can't delay, so we continue
                self.lo_tx.try_send(message)
            }
            InterceptAction::Continue | InterceptAction::Transform => self.lo_tx.try_send(message),
        }
    }

    #[inline]
    async fn hi_send(
        &self,
        message: A::HighPriorityMessage,
    ) -> Result<(), mailbox::SendError<A::HighPriorityMessage>> {
        match self
            .interceptors
            .intercept_hi_message(&self.actor_id, &message)
        {
            InterceptAction::Drop => Ok(()),
            InterceptAction::Delay(ms) => {
                tokio::time::sleep(tokio::time::Duration::from_millis(ms)).await;
                self.hi_tx.send(message).await
            }
            InterceptAction::Continue | InterceptAction::Transform => {
                self.hi_tx.send(message).await
            }
        }
    }

    #[inline]
    fn hi_try_send(
        &self,
        message: A::HighPriorityMessage,
    ) -> Result<(), mailbox::TrySendError<A::HighPriorityMessage>> {
        match self
            .interceptors
            .intercept_hi_message(&self.actor_id, &message)
        {
            InterceptAction::Drop => Ok(()),
            InterceptAction::Delay(_) => {
                // For try_send, we can't delay, so we continue
                self.hi_tx.try_send(message)
            }
            InterceptAction::Continue | InterceptAction::Transform => self.hi_tx.try_send(message),
        }
    }
}

pub struct RunnerConfig {
    pub lo_cap: usize,
    pub hi_cap: usize,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            lo_cap: 1,
            hi_cap: 1,
        }
    }
}

impl RunnerConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_lo(mut self, cap: usize) -> Self {
        self.lo_cap = cap;
        self
    }

    pub fn with_hi(mut self, cap: usize) -> Self {
        self.hi_cap = cap;
        self
    }
}

pub struct Runner<A: Actor> {
    actor: A,
    ctx: ActorContext<A>,
    interceptors: Arc<InterceptorRegistry<A>>,
}

impl<A: Actor> Runner<A> {
    pub async fn run(self) -> (A::ID, ActorStatus) {
        let actor_id = self.actor.id();

        let fut = async move {
            let status = self.run_instrumented().await;
            (actor_id, status)
        }
        .in_current_span();
        fut.await
    }

    async fn run_instrumented(mut self) -> ActorStatus {
        let actor_id = self.actor.id();

        // Fast path - inlined and optimized away if no interceptors
        self.interceptors.before_run(&actor_id);

        tracing::debug!("Starting actor...");

        let run_result = AssertUnwindSafe(self.actor.run(&mut self.ctx))
            .catch_unwind()
            .await;

        let status_after_run = match run_result {
            Ok(Ok(())) => {
                tracing::debug!("Main logic exited gracefully.");
                ActorStatus::ExitedGracefully
            }
            Ok(Err(err)) => {
                // Fast path - inlined and optimized away if no interceptors
                self.interceptors.on_error(&actor_id, &err);

                tracing::warn!(error = %err, "Main logic exited with an error.");
                ActorStatus::ExitedWithError
            }
            Err(panic_payload) => {
                let panic_msg = extract_panic_message(&panic_payload);

                // Fast path - inlined and optimized away if no interceptors
                self.interceptors.on_panic(&actor_id, &panic_msg);

                tracing::error!(panic.message = %panic_msg, "Actor panicked!");
                ActorStatus::Panicked
            }
        };

        // Fast path - inlined and optimized away if no interceptors
        self.interceptors.after_run(&actor_id, status_after_run);

        tracing::debug!("post_stop successful.");
        tracing::info!(status = %status_after_run, "Actor fully shut down.");

        status_after_run
    }
}

fn extract_panic_message(payload: &Box<dyn Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        format!("{:?}", payload)
    }
}

// Production-ready interceptors
pub mod interceptors {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    /// High-performance metrics interceptor - zero allocation in fast path
    pub struct MetricsInterceptor<A: Actor> {
        hi_message_count: AtomicU64,
        lo_message_count: AtomicU64,
        actor_starts: AtomicU64,
        actor_errors: AtomicU64,
        actor_panics: AtomicU64,
        _phantom: std::marker::PhantomData<A>,
    }

    impl<A: Actor> MetricsInterceptor<A> {
        pub fn new() -> Self {
            Self {
                hi_message_count: AtomicU64::new(0),
                lo_message_count: AtomicU64::new(0),
                actor_starts: AtomicU64::new(0),
                actor_errors: AtomicU64::new(0),
                actor_panics: AtomicU64::new(0),
                _phantom: std::marker::PhantomData,
            }
        }

        pub fn get_hi_message_count(&self) -> u64 {
            self.hi_message_count.load(Ordering::Relaxed)
        }

        pub fn get_lo_message_count(&self) -> u64 {
            self.lo_message_count.load(Ordering::Relaxed)
        }

        pub fn get_total_message_count(&self) -> u64 {
            self.get_hi_message_count() + self.get_lo_message_count()
        }

        pub fn get_actor_starts(&self) -> u64 {
            self.actor_starts.load(Ordering::Relaxed)
        }

        pub fn get_error_count(&self) -> u64 {
            self.actor_errors.load(Ordering::Relaxed)
        }

        pub fn get_panic_count(&self) -> u64 {
            self.actor_panics.load(Ordering::Relaxed)
        }
    }

    impl<A: Actor> MessageInterceptor<A> for MetricsInterceptor<A> {
        #[inline(always)]
        fn intercept_hi_message(
            &self,
            _actor_id: &A::ID,
            _message: &A::HighPriorityMessage,
        ) -> InterceptAction {
            self.hi_message_count.fetch_add(1, Ordering::Relaxed);
            InterceptAction::Continue
        }

        #[inline(always)]
        fn intercept_lo_message(
            &self,
            _actor_id: &A::ID,
            _message: &A::LowPriorityMessage,
        ) -> InterceptAction {
            self.lo_message_count.fetch_add(1, Ordering::Relaxed);
            InterceptAction::Continue
        }
    }

    impl<A: Actor> ActorInterceptor<A> for MetricsInterceptor<A> {
        #[inline(always)]
        fn before_run(&self, _actor_id: &A::ID) {
            self.actor_starts.fetch_add(1, Ordering::Relaxed);
        }

        #[inline(always)]
        fn on_error(&self, _actor_id: &A::ID, _error: &ActorError) {
            self.actor_errors.fetch_add(1, Ordering::Relaxed);
        }

        #[inline(always)]
        fn on_panic(&self, _actor_id: &A::ID, _panic_msg: &str) {
            self.actor_panics.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Circuit breaker interceptor for production resilience
    pub struct CircuitBreakerInterceptor<A: Actor> {
        failure_count: AtomicU64,
        last_failure_time: Mutex<Option<Instant>>,
        failure_threshold: u64,
        timeout_duration: Duration,
        _phantom: std::marker::PhantomData<A>,
    }

    impl<A: Actor> CircuitBreakerInterceptor<A> {
        pub fn new(failure_threshold: u64, timeout_duration: Duration) -> Self {
            Self {
                failure_count: AtomicU64::new(0),
                last_failure_time: Mutex::new(None),
                failure_threshold,
                timeout_duration,
                _phantom: std::marker::PhantomData,
            }
        }

        #[inline]
        fn should_block(&self) -> bool {
            let failures = self.failure_count.load(Ordering::Relaxed);
            if failures < self.failure_threshold {
                return false;
            }

            // Check if timeout has passed
            if let Ok(last_failure) = self.last_failure_time.try_lock() {
                if let Some(last_time) = *last_failure {
                    if last_time.elapsed() > self.timeout_duration {
                        // Reset circuit breaker
                        drop(last_failure);
                        self.failure_count.store(0, Ordering::Relaxed);
                        return false;
                    }
                }
            }

            true
        }
    }

    impl<A: Actor> MessageInterceptor<A> for CircuitBreakerInterceptor<A> {
        #[inline]
        fn intercept_hi_message(
            &self,
            _actor_id: &A::ID,
            _message: &A::HighPriorityMessage,
        ) -> InterceptAction {
            if self.should_block() {
                InterceptAction::Drop
            } else {
                InterceptAction::Continue
            }
        }

        #[inline]
        fn intercept_lo_message(
            &self,
            _actor_id: &A::ID,
            _message: &A::LowPriorityMessage,
        ) -> InterceptAction {
            if self.should_block() {
                InterceptAction::Drop
            } else {
                InterceptAction::Continue
            }
        }
    }

    impl<A: Actor> ActorInterceptor<A> for CircuitBreakerInterceptor<A> {
        #[inline]
        fn on_error(&self, _actor_id: &A::ID, _error: &ActorError) {
            self.failure_count.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut last_failure) = self.last_failure_time.try_lock() {
                *last_failure = Some(Instant::now());
            }
        }

        #[inline]
        fn on_panic(&self, _actor_id: &A::ID, _panic_msg: &str) {
            self.failure_count.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut last_failure) = self.last_failure_time.try_lock() {
                *last_failure = Some(Instant::now());
            }
        }
    }

    /// Rate limiting interceptor for production load management
    pub struct RateLimitInterceptor<A: Actor> {
        tokens: AtomicU64,
        last_refill: Mutex<Instant>,
        max_tokens: u64,
        refill_rate: u64, // tokens per second
        _phantom: std::marker::PhantomData<A>,
    }

    impl<A: Actor> RateLimitInterceptor<A> {
        pub fn new(max_tokens: u64, refill_rate: u64) -> Self {
            Self {
                tokens: AtomicU64::new(max_tokens),
                last_refill: Mutex::new(Instant::now()),
                max_tokens,
                refill_rate,
                _phantom: std::marker::PhantomData,
            }
        }

        #[inline]
        fn try_consume_token(&self) -> bool {
            // Try to refill tokens
            if let Ok(mut last_refill) = self.last_refill.try_lock() {
                let now = Instant::now();
                let elapsed = now.duration_since(*last_refill);
                let tokens_to_add = (elapsed.as_secs() * self.refill_rate).min(self.max_tokens);

                if tokens_to_add > 0 {
                    let current = self.tokens.load(Ordering::Relaxed);
                    let new_tokens = (current + tokens_to_add).min(self.max_tokens);
                    self.tokens.store(new_tokens, Ordering::Relaxed);
                    *last_refill = now;
                }
            }

            // Try to consume a token
            loop {
                let current = self.tokens.load(Ordering::Relaxed);
                if current == 0 {
                    return false;
                }

                if self
                    .tokens
                    .compare_exchange_weak(
                        current,
                        current - 1,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    return true;
                }
            }
        }
    }

    impl<A: Actor> MessageInterceptor<A> for RateLimitInterceptor<A> {
        #[inline]
        fn intercept_hi_message(
            &self,
            _actor_id: &A::ID,
            _message: &A::HighPriorityMessage,
        ) -> InterceptAction {
            if self.try_consume_token() {
                InterceptAction::Continue
            } else {
                InterceptAction::Drop
            }
        }

        #[inline]
        fn intercept_lo_message(
            &self,
            _actor_id: &A::ID,
            _message: &A::LowPriorityMessage,
        ) -> InterceptAction {
            if self.try_consume_token() {
                InterceptAction::Continue
            } else {
                InterceptAction::Drop
            }
        }
    }

    /// Tracing interceptor for distributed tracing
    pub struct TracingInterceptor<A: Actor> {
        _phantom: std::marker::PhantomData<A>,
    }

    impl<A: Actor> TracingInterceptor<A> {
        pub fn new() -> Self {
            Self {
                _phantom: std::marker::PhantomData,
            }
        }
    }

    impl<A: Actor> MessageInterceptor<A> for TracingInterceptor<A> {
        #[inline]
        fn intercept_hi_message(
            &self,
            actor_id: &A::ID,
            _message: &A::HighPriorityMessage,
        ) -> InterceptAction {
            tracing::trace!(actor_id = %actor_id, "High priority message received");
            InterceptAction::Continue
        }

        #[inline]
        fn intercept_lo_message(
            &self,
            actor_id: &A::ID,
            _message: &A::LowPriorityMessage,
        ) -> InterceptAction {
            tracing::trace!(actor_id = %actor_id, "Low priority message received");
            InterceptAction::Continue
        }
    }

    impl<A: Actor> ActorInterceptor<A> for TracingInterceptor<A> {
        #[inline]
        fn before_run(&self, actor_id: &A::ID) {
            tracing::info!(actor_id = %actor_id, "Actor starting");
        }

        #[inline]
        fn after_run(&self, actor_id: &A::ID, status: ActorStatus) {
            tracing::info!(actor_id = %actor_id, status = %status, "Actor finished");
        }

        #[inline]
        fn on_error(&self, actor_id: &A::ID, error: &ActorError) {
            tracing::error!(actor_id = %actor_id, error = %error, "Actor error");
        }

        #[inline]
        fn on_panic(&self, actor_id: &A::ID, panic_msg: &str) {
            tracing::error!(actor_id = %actor_id, panic_msg = %panic_msg, "Actor panic");
        }
    }
}

// Test utilities - built on the same high-performance foundation
pub mod test_utils {
    use super::*;
    use std::sync::{Arc, Mutex};

    // Test helper for capturing messages - same performance as production interceptors
    pub struct MessageCapture<A: Actor> {
        pub hi_messages: Arc<Mutex<Vec<A::HighPriorityMessage>>>,
        pub lo_messages: Arc<Mutex<Vec<A::LowPriorityMessage>>>,
    }

    impl<A: Actor> MessageCapture<A> {
        pub fn new() -> Self {
            Self {
                hi_messages: Arc::new(Mutex::new(Vec::new())),
                lo_messages: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub fn get_hi_messages(&self) -> Vec<A::HighPriorityMessage>
        where
            A::HighPriorityMessage: Clone,
        {
            self.hi_messages.lock().unwrap().clone()
        }

        pub fn get_lo_messages(&self) -> Vec<A::LowPriorityMessage>
        where
            A::LowPriorityMessage: Clone,
        {
            self.lo_messages.lock().unwrap().clone()
        }

        pub fn clear(&self) {
            self.hi_messages.lock().unwrap().clear();
            self.lo_messages.lock().unwrap().clear();
        }

        pub fn hi_count(&self) -> usize {
            self.hi_messages.lock().unwrap().len()
        }

        pub fn lo_count(&self) -> usize {
            self.lo_messages.lock().unwrap().len()
        }

        pub fn total_count(&self) -> usize {
            self.hi_count() + self.lo_count()
        }
    }

    impl<A: Actor> MessageInterceptor<A> for MessageCapture<A>
    where
        A::HighPriorityMessage: Clone,
        A::LowPriorityMessage: Clone,
    {
        #[inline]
        fn intercept_hi_message(
            &self,
            _actor_id: &A::ID,
            message: &A::HighPriorityMessage,
        ) -> InterceptAction {
            if let Ok(mut messages) = self.hi_messages.try_lock() {
                messages.push(message.clone());
            }
            InterceptAction::Continue
        }

        #[inline]
        fn intercept_lo_message(
            &self,
            _actor_id: &A::ID,
            message: &A::LowPriorityMessage,
        ) -> InterceptAction {
            if let Ok(mut messages) = self.lo_messages.try_lock() {
                messages.push(message.clone());
            }
            InterceptAction::Continue
        }
    }

    // Test helper for actor lifecycle events
    pub struct ActorLifecycleCapture<A: Actor> {
        pub events: Arc<Mutex<Vec<LifecycleEvent<A::ID>>>>,
    }

    #[derive(Debug, Clone)]
    pub enum LifecycleEvent<ID> {
        BeforeRun(ID),
        AfterRun(ID, ActorStatus),
        OnError(ID, String),
        OnPanic(ID, String),
    }

    impl<A: Actor> ActorLifecycleCapture<A> {
        pub fn new() -> Self {
            Self {
                events: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub fn get_events(&self) -> Vec<LifecycleEvent<A::ID>> {
            self.events.lock().unwrap().clone()
        }

        pub fn clear(&self) {
            self.events.lock().unwrap().clear();
        }
    }

    impl<A: Actor> ActorInterceptor<A> for ActorLifecycleCapture<A> {
        #[inline]
        fn before_run(&self, actor_id: &A::ID) {
            if let Ok(mut events) = self.events.try_lock() {
                events.push(LifecycleEvent::BeforeRun(actor_id.clone()));
            }
        }

        #[inline]
        fn after_run(&self, actor_id: &A::ID, status: ActorStatus) {
            if let Ok(mut events) = self.events.try_lock() {
                events.push(LifecycleEvent::AfterRun(actor_id.clone(), status));
            }
        }

        #[inline]
        fn on_error(&self, actor_id: &A::ID, error: &ActorError) {
            if let Ok(mut events) = self.events.try_lock() {
                events.push(LifecycleEvent::OnError(actor_id.clone(), error.to_string()));
            }
        }

        #[inline]
        fn on_panic(&self, actor_id: &A::ID, panic_msg: &str) {
            if let Ok(mut events) = self.events.try_lock() {
                events.push(LifecycleEvent::OnPanic(
                    actor_id.clone(),
                    panic_msg.to_string(),
                ));
            }
        }
    }

    // Builder pattern for easy setup - works for both test and production
    pub struct ActorBuilder<A: Actor> {
        actor: Option<A>,
        config: RunnerConfig,
        interceptors: InterceptorRegistry<A>,
    }

    impl<A: Actor> ActorBuilder<A> {
        pub fn new(actor: A) -> Self {
            Self {
                actor: Some(actor),
                config: RunnerConfig::default(),
                interceptors: InterceptorRegistry::new(),
            }
        }

        pub fn with_config(mut self, config: RunnerConfig) -> Self {
            self.config = config;
            self
        }

        pub fn with_lo_capacity(mut self, cap: usize) -> Self {
            self.config.lo_cap = cap;
            self
        }

        pub fn with_hi_capacity(mut self, cap: usize) -> Self {
            self.config.hi_cap = cap;
            self
        }

        pub fn with_message_interceptor(
            mut self,
            interceptor: Arc<dyn MessageInterceptor<A>>,
        ) -> Self {
            self.interceptors.add_message_interceptor(interceptor);
            self
        }

        pub fn with_actor_interceptor(mut self, interceptor: Arc<dyn ActorInterceptor<A>>) -> Self {
            self.interceptors.add_actor_interceptor(interceptor);
            self
        }

        // Convenient methods for common interceptors
        pub fn with_metrics(self) -> (Self, Arc<interceptors::MetricsInterceptor<A>>) {
            let metrics = Arc::new(interceptors::MetricsInterceptor::new());
            let new_self = self
                .with_message_interceptor(metrics.clone())
                .with_actor_interceptor(metrics.clone());
            (new_self, metrics)
        }

        pub fn with_tracing(self) -> Self {
            let tracing_interceptor = Arc::new(interceptors::TracingInterceptor::new());
            self.with_message_interceptor(tracing_interceptor.clone())
                .with_actor_interceptor(tracing_interceptor)
        }

        pub fn with_circuit_breaker(
            self,
            failure_threshold: u64,
            timeout: std::time::Duration,
        ) -> (Self, Arc<interceptors::CircuitBreakerInterceptor<A>>) {
            let circuit_breaker = Arc::new(interceptors::CircuitBreakerInterceptor::new(
                failure_threshold,
                timeout,
            ));
            let new_self = self
                .with_message_interceptor(circuit_breaker.clone())
                .with_actor_interceptor(circuit_breaker.clone());
            (new_self, circuit_breaker)
        }

        pub fn with_rate_limiting(self, max_tokens: u64, refill_rate: u64) -> Self {
            let rate_limiter = Arc::new(interceptors::RateLimitInterceptor::new(
                max_tokens,
                refill_rate,
            ));
            self.with_message_interceptor(rate_limiter)
        }

        // Test-specific convenience methods
        pub fn capture_messages(self) -> (Self, Arc<MessageCapture<A>>)
        where
            A::HighPriorityMessage: Clone,
            A::LowPriorityMessage: Clone,
        {
            let capture = Arc::new(MessageCapture::new());
            let new_self = self.with_message_interceptor(capture.clone());
            (new_self, capture)
        }

        pub fn capture_lifecycle(self) -> (Self, Arc<ActorLifecycleCapture<A>>) {
            let capture = Arc::new(ActorLifecycleCapture::new());
            let new_self = self.with_actor_interceptor(capture.clone());
            (new_self, capture)
        }

        pub fn build(mut self) -> (LocalActorHandle<A>, Runner<A>) {
            let actor = self.actor.take().expect("Actor was already consumed");
            if self.interceptors.is_empty() {
                LocalActorHandle::new(actor, self.config)
            } else {
                LocalActorHandle::with_interceptors(actor, self.config, Arc::new(self.interceptors))
            }
        }
    }

    // Utility interceptors for testing
    pub struct DelayInterceptor<A: Actor> {
        pub delay_ms: u64,
        _phantom: std::marker::PhantomData<A>,
    }

    impl<A: Actor> DelayInterceptor<A> {
        pub fn new(delay_ms: u64) -> Self {
            Self {
                delay_ms,
                _phantom: std::marker::PhantomData,
            }
        }
    }

    impl<A: Actor> MessageInterceptor<A> for DelayInterceptor<A> {
        #[inline]
        fn intercept_hi_message(
            &self,
            _actor_id: &A::ID,
            _message: &A::HighPriorityMessage,
        ) -> InterceptAction {
            InterceptAction::Delay(self.delay_ms)
        }

        #[inline]
        fn intercept_lo_message(
            &self,
            _actor_id: &A::ID,
            _message: &A::LowPriorityMessage,
        ) -> InterceptAction {
            InterceptAction::Delay(self.delay_ms)
        }
    }

    pub struct DropInterceptor<A: Actor> {
        drop_probability: f32,
        _phantom: std::marker::PhantomData<A>,
    }

    impl<A: Actor> DropInterceptor<A> {
        pub fn new(drop_probability: f32) -> Self {
            Self {
                drop_probability: drop_probability.clamp(0.0, 1.0),
                _phantom: std::marker::PhantomData,
            }
        }
    }

    impl<A: Actor> MessageInterceptor<A> for DropInterceptor<A> {
        #[inline]
        fn intercept_hi_message(
            &self,
            _actor_id: &A::ID,
            _message: &A::HighPriorityMessage,
        ) -> InterceptAction {
            if rand::random::<f32>() < self.drop_probability {
                InterceptAction::Drop
            } else {
                InterceptAction::Continue
            }
        }

        #[inline]
        fn intercept_lo_message(
            &self,
            _actor_id: &A::ID,
            _message: &A::LowPriorityMessage,
        ) -> InterceptAction {
            if rand::random::<f32>() < self.drop_probability {
                InterceptAction::Drop
            } else {
                InterceptAction::Continue
            }
        }
    }
}
