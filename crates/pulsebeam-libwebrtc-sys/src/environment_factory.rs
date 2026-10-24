//! Reusable owning SDK environment composition.
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sdk_factory_defaults_are_fresh_but_selected_utilities_preserve_identity() {
        let factory = EnvironmentFactory::new();
        let first = factory.create().unwrap();
        let second = factory.create().unwrap();
        assert!(!ffi::environment_queue_factory_identity_matches(
            first.native(),
            second.native()
        ));
        let selected = EnvironmentFactory::from_environment(&first).unwrap();
        let copy = selected.clone().create().unwrap();
        assert!(ffi::environment_queue_factory_identity_matches(
            first.native(),
            copy.native()
        ));
        assert!(ffi::environment_clock_identity_matches(
            first.native(),
            copy.native()
        ));
        fn shared<T: Send + Sync>() {}
        shared::<EnvironmentFactory>();
    }
}
use super::*;

fn bounds(value: crate::ClockTimeBounds, monotonic: bool) -> ffi::FfiClockTimeBounds {
    ffi::FfiClockTimeBounds {
        known: true,
        minimum_us: value.minimum().microseconds(),
        maximum_us: value.maximum().microseconds(),
        monotonic,
    }
}

// Mirrors SDK storage branches: replacing an input does not release dependencies
// of an earlier input still owned by the native factory or an environment copy.
enum Input {
    Environment { _owner: Environment },
    Clock { _owner: crate::Clock },
    Manual { _owner: ManualClock },
    Queues { _owner: TaskQueueFactory },
}
struct Resources {
    _input: Input,
    _parent: Option<Arc<Resources>>,
}
pub(crate) struct RustEnvironmentFactoryRetention {
    _resources: Option<Arc<Resources>>,
    // Drop after native storage AND all Rust input owners, including on errors.
    _randomness: Option<Arc<RandomnessState>>,
}

// A logger can outlive its containing SDK storage when stock destruction has
// to leave its own output queue. This guard has no Environment back-reference.
pub(crate) struct RustEnvironmentFactoryRandomness {
    _randomness: Arc<RandomnessState>,
}
pub(crate) fn environment_factory_has_randomness(
    retention: &RustEnvironmentFactoryRetention,
) -> bool {
    retention._randomness.is_some()
}
pub(crate) fn environment_factory_randomness(
    retention: &RustEnvironmentFactoryRetention,
) -> Box<RustEnvironmentFactoryRandomness> {
    Box::new(RustEnvironmentFactoryRandomness {
        _randomness: retention
            ._randomness
            .as_ref()
            .expect("selected lease")
            .clone(),
    })
}

/// Owning, reusable native `EnvironmentFactory`. Clones share installed utility
/// ownership, but later setters affect only that factory. `create` invokes the
/// SDK every time; absent utilities retain their native default semantics.
/// Setters retain their inputs independently. Closed factories reject use.
pub struct EnvironmentFactory {
    native: cxx::UniquePtr<ffi::NativeEnvironmentFactory>,
    resources: Option<Arc<Resources>>,
    clock: Option<ManualClock>,
    clock_handle: Option<crate::Clock>,
    custom_clock: bool,
    queues: Option<TaskQueueFactory>,
    default_queues: bool,
    randomness: Option<Arc<RandomnessState>>,
}
// SAFETY: native setters require exclusive Rust access. SDK const Create and
// copies use immutable selections, thread-safe shared storage and providers.
// Cooperative creation still checks the original scheduler creator sequence.
unsafe impl Send for EnvironmentFactory {}
unsafe impl Sync for EnvironmentFactory {}
impl Default for EnvironmentFactory {
    fn default() -> Self {
        Self::new()
    }
}
impl Clone for EnvironmentFactory {
    fn clone(&self) -> Self {
        Self {
            native: self
                .native
                .as_ref()
                .map_or_else(cxx::UniquePtr::null, ffi::clone_environment_factory),
            resources: self.resources.clone(),
            clock: self.clock.clone(),
            clock_handle: self.clock_handle.clone(),
            custom_clock: self.custom_clock,
            queues: self.queues.clone(),
            default_queues: self.default_queues,
            randomness: self.randomness.clone(),
        }
    }
}
impl EnvironmentFactory {
    pub fn new() -> Self {
        Self {
            native: ffi::new_environment_factory(bounds(
                crate::ClockTimeBounds::system_time(),
                false,
            )),
            resources: None,
            clock: None,
            clock_handle: None,
            custom_clock: false,
            queues: None,
            default_queues: true,
            randomness: None,
        }
    }
    /// Use the actual SDK EnvironmentFactory(Environment) constructor, without
    /// sampling time, copying trial values or substituting a logger/factory.
    pub fn from_environment(environment: &Environment) -> Result<Self, BuildEnvironmentError> {
        let native = environment
            .native
            .as_ref()
            .ok_or(BuildEnvironmentError::Closed)?;
        Ok(Self {
            native: ffi::environment_factory_from_environment(native),
            resources: Some(Arc::new(Resources {
                _input: Input::Environment {
                    _owner: environment.clone(),
                },
                _parent: None,
            })),
            clock: environment._clock.clone(),
            clock_handle: environment._clock_handle.clone(),
            custom_clock: environment._custom_clock,
            queues: environment._task_queue_factory.clone(),
            default_queues: false,
            randomness: environment._randomness.clone(),
        })
    }
    fn check_open(&self) -> Result<(), BuildEnvironmentError> {
        if self.native.is_null() {
            Err(BuildEnvironmentError::Closed)
        } else {
            Ok(())
        }
    }
    fn retain(&mut self, input: Input) {
        self.resources = Some(Arc::new(Resources {
            _input: input,
            _parent: self.resources.take(),
        }));
    }
    pub fn set_field_trials(
        &mut self,
        view: impl crate::FieldTrialsView,
    ) -> Result<(), BuildEnvironmentError> {
        self.check_open()?;
        ffi::environment_factory_set_trials(
            self.native.pin_mut(),
            Box::new(crate::RustFieldTrials(Some(Arc::new(view)))),
        );
        Ok(())
    }
    pub fn set_clock(&mut self, clock: &ManualClock) -> Result<(), BuildEnvironmentError> {
        self.check_open()?;
        ffi::environment_factory_set_manual_clock(
            self.native.pin_mut(),
            clock.native(),
            bounds(clock.time_bounds(), true),
        );
        self.retain(Input::Manual {
            _owner: clock.clone(),
        });
        self.clock = Some(clock.clone());
        self.clock_handle = None;
        self.custom_clock = false;
        Ok(())
    }
    pub fn set_clock_handle(&mut self, clock: &crate::Clock) -> Result<(), BuildEnvironmentError> {
        self.check_open()?;
        ffi::environment_factory_set_clock(
            self.native.pin_mut(),
            clock.native(),
            bounds(clock.time_bounds(), clock.is_monotonic()),
        );
        self.retain(Input::Clock {
            _owner: clock.clone(),
        });
        self.clock = clock.manual_source().cloned();
        self.clock_handle = Some(clock.clone());
        self.custom_clock = true;
        Ok(())
    }
    pub fn set_task_queue_factory(
        &mut self,
        queues: &TaskQueueFactory,
    ) -> Result<(), BuildEnvironmentError> {
        self.check_open()?;
        ffi::environment_factory_set_queues(self.native.pin_mut(), queues.native());
        self.retain(Input::Queues {
            _owner: queues.clone(),
        });
        self.queues = Some(queues.clone());
        self.default_queues = false;
        Ok(())
    }
    pub fn set_event_log(
        &mut self,
        logger: impl crate::RtcEventLogger,
    ) -> Result<(), BuildEnvironmentError> {
        self.check_open()?;
        ffi::environment_factory_set_log(
            self.native.pin_mut(),
            crate::RustRtcEventLog::from_logger(Some(Box::new(logger))),
        );
        Ok(())
    }
    /// Move an existing native product without stopping or reconstructing it.
    /// Its pending stop and all SDK/utility aliases share one admission state.
    /// Stock control affinity survives installation; moving ownership does not
    /// authorize control on another sequence.
    pub fn set_event_log_product(
        &mut self,
        logger: crate::RtcEventLogHandle,
    ) -> Result<(), crate::EventLogError> {
        if self.native.is_null() {
            return Err(crate::EventLogError::Closed);
        }
        crate::event_log_product::status(ffi::environment_factory_set_log_product(
            self.native.pin_mut(),
            logger.into_native(),
        ))
    }
    pub fn set_randomness(&mut self, lease: &RandomnessLease) -> Result<(), BuildEnvironmentError> {
        self.check_open()?;
        self.randomness = Some(lease.0.clone());
        Ok(())
    }
    /// Defaults are resolved by the SDK on every call. Unlike the convenience
    /// EnvironmentBuilder, this factory does not infer a clock from queues:
    /// select a matching clock explicitly before using cooperative queues.
    pub fn create(&self) -> Result<Environment, BuildEnvironmentError> {
        self.check_open()?;
        if let Some(queues) = &self.queues {
            queues.check_current()?;
            if queues.is_cooperative()
                && self.clock.as_ref().is_none_or(|clock| {
                    !ffi::task_queue_factory_uses_clock(queues.native(), clock.native())
                })
            {
                return Err(BuildEnvironmentError::MismatchedCooperativeClock);
            }
        }
        let native = ffi::environment_factory_create(
            self.native.as_ref().unwrap(),
            Box::new(RustEnvironmentFactoryRetention {
                _resources: self.resources.clone(),
                _randomness: self.randomness.clone(),
            }),
        );
        let mut environment = Environment::from_sdk(native, None);
        environment._clock = self.clock.clone();
        environment._clock_handle = self.clock_handle.clone();
        environment._custom_clock = self.custom_clock;
        environment._randomness = self.randomness.clone();
        environment._task_queue_factory = self.queues.clone();
        if self.default_queues {
            // This metadata describes the SDK-created default. It never creates
            // a replacement factory or reserves/starts a production executor.
            let native = ffi::environment_default_queue_metadata(environment.native());
            let base = environment.clone();
            environment._task_queue_factory =
                Some(TaskQueueFactory(Arc::new(TaskQueueFactoryInner {
                    native,
                    clock: None,
                    provider: None,
                    _environment: Some(Box::new(base)),
                })));
        }
        Ok(environment)
    }
    pub fn close(&mut self) {
        drop(std::mem::replace(&mut self.native, cxx::UniquePtr::null()));
        self.resources.take();
        self.clock.take();
        self.clock_handle.take();
        self.queues.take();
        self.randomness.take();
    }
}
