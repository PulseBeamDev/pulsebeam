//! Actual SDK echo creation inputs and unique product handoff.
use crate::audio_processor_custom::{Provider, RustApmCustomModule};
use crate::{
    AudioProcessingModule, AudioProcessorError, EchoControl, EchoControlFactory, Environment, ffi,
};
use std::{marker::PhantomData, pin::Pin, rc::Rc};

pub(crate) struct RustEchoEnvironment(pub(crate) Environment);
pub(crate) fn apm_echo_environment_native(
    source: &RustEchoEnvironment,
) -> cxx::UniquePtr<ffi::NativeEnvironment> {
    ffi::clone_environment(source.0.native())
}

/// Serialized owning factory. Both native overloads remain distinct. The native
/// default five-argument overload ignores the estimator and invokes Create.
/// Never wait for or reenter the owning peer/actor from a callback or destructor.
pub trait EchoControlFactoryProvider: Send + 'static {
    fn create<'a>(
        &mut self,
        context: EchoControlCreationContext<'a>,
    ) -> Result<EchoControl<'a>, String>;
    fn create_with_neural<'a>(
        &mut self,
        mut context: EchoControlCreationContext<'a>,
    ) -> Result<EchoControl<'a>, String> {
        context.include_neural = false;
        self.create(context)
    }
}

/// The actual factory invocation. Consuming construction methods prevent a
/// second mutable estimator loan after a product has captured that dependency.
/// Contexts and estimator views cannot escape the callback or cross threads.
pub struct EchoControlCreationContext<'a> {
    native: &'a ffi::NativeEchoCreationContext,
    dependencies: Pin<&'a mut ffi::NativeEchoDependencies>,
    environment: Environment,
    failure: crate::AudioProcessingCustomFailureObserver,
    include_neural: bool,
    _sequence: PhantomData<Rc<()>>,
}
impl<'a> EchoControlCreationContext<'a> {
    pub fn environment(&self) -> &Environment {
        &self.environment
    }
    pub fn sample_rate_hz(&self) -> i32 {
        ffi::audio_echo_context_rate(self.native)
    }
    pub fn render_channels(&self) -> i32 {
        ffi::audio_echo_context_render_channels(self.native)
    }
    pub fn capture_channels(&self) -> i32 {
        ffi::audio_echo_context_capture_channels(self.native)
    }
    pub fn neural_estimator(&mut self) -> Option<crate::NeuralResidualEchoEstimatorLoan<'_>> {
        if !self.include_neural {
            return None;
        }
        crate::audio_echo_dependencies::loan(self.dependencies.as_mut())
    }
    /// Construct through the selected owning factory with these exact native
    /// creation inputs, including the supplied estimator when applicable.
    pub fn create(
        self,
        factory: &mut EchoControlFactory,
    ) -> Result<EchoControl<'a>, AudioProcessorError> {
        factory.bind_environment(&self.environment);
        let mut error = 0;
        let native = ffi::audio_echo_context_create(
            factory.native.pin_mut(),
            self.native,
            self.include_neural,
            &mut error,
        );
        AudioProcessingModule::admission(error)?;
        EchoControl::from_context_native(native, self.environment, factory.failure_observer())
    }
    /// Own this exact caller controller. Estimator access is supplied afresh in
    /// each controller callback, not retained in an escaping Rust reference.
    pub fn controller(
        self,
        provider: impl crate::EchoControlProvider,
    ) -> Result<EchoControl<'a>, AudioProcessorError> {
        let context = crate::audio_echo_dependencies::RustEchoControlContext {
            environment: self.environment.clone(),
            rate: self.sample_rate_hz(),
            render_channels: self.render_channels(),
            capture_channels: self.capture_channels(),
        };
        let module = Box::new(RustApmCustomModule {
            provider: std::sync::Mutex::new(Some(Provider::EchoControl(
                Box::new(provider),
                context,
            ))),
            failure: self.failure.clone(),
        });
        let mut error = 0;
        let native = ffi::audio_echo_context_controller(
            self.native,
            self.include_neural,
            module,
            &mut error,
        );
        AudioProcessingModule::admission(error)?;
        EchoControl::from_context_native(native, self.environment, Some(self.failure))
    }
}

pub(crate) fn apm_echo_factory_create(
    module: &RustApmCustomModule,
    native_environment: cxx::UniquePtr<ffi::NativeEnvironment>,
    retention: &RustEchoEnvironment,
    native: &ffi::NativeEchoCreationContext,
    dependencies: Pin<&mut ffi::NativeEchoDependencies>,
    neural_overload: bool,
) -> cxx::UniquePtr<ffi::NativeEchoControl> {
    let environment = Environment::from_sdk(native_environment, Some(&retention.0));
    module
        .call(crate::AudioProcessingCustomOperation::Create, |provider| {
            let Provider::EchoFactory(provider) = provider else {
                return Err("echo factory kind mismatch".into());
            };
            let context = EchoControlCreationContext {
                native,
                dependencies,
                environment,
                failure: module.failure.clone(),
                include_neural: neural_overload,
                _sequence: PhantomData,
            };
            let product = if neural_overload {
                provider.create_with_neural(context)?
            } else {
                provider.create(context)?
            };
            Ok(product.into_factory_transfer())
        })
        .unwrap_or_else(cxx::UniquePtr::null)
}
pub(crate) fn apm_echo_factory_invalid(module: &RustApmCustomModule) {
    module.failure.record(
        crate::AudioProcessingCustomOperation::Create,
        crate::AudioProcessingCustomFailureKind::InvalidBridgeValue,
    );
}
