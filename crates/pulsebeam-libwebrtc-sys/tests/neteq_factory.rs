//! Public factory/context ownership, nullable inputs and callback failures.
use pulsebeam_webrtc_sys::*;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Product {
    id: i32,
    drops: Arc<AtomicUsize>,
    context: NetEqCreationContext,
}
impl Drop for Product {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl NetEqProvider for Product {
    fn insert_packet_with_info(
        &mut self,
        _: &RtpHeaderValues,
        _: &[u8],
        _: &RtpReceivedPacketInfo,
    ) -> NetEqReturnCode {
        NetEqReturnCode(self.id)
    }
    fn get_audio(
        &mut self,
        _: &mut AudioFrameMut<'_>,
        _: Option<NetEqOutput<'_, bool>>,
        _: Option<NetEqOutput<'_, i32>>,
        _: Option<NetEqOperation>,
    ) -> NetEqReturnCode {
        panic!("product panic")
    }
    fn set_codecs(&mut self, _: &BTreeMap<i32, SdpAudioFormat>) {}
    fn register_payload_type(&mut self, _: i32, _: &SdpAudioFormat) -> bool {
        true
    }
    fn remove_payload_type(&mut self, _: u8) -> NetEqReturnCode {
        NetEqReturnCode::OK
    }
    fn remove_all_payload_types(&mut self) {}
    fn set_maximum_buffer_packets(&mut self, _: usize) {}
    fn set_fast_accelerate(&mut self, _: bool) {}
    fn set_minimum_delay(&mut self, _: i32) -> bool {
        true
    }
    fn set_maximum_delay(&mut self, _: i32) -> bool {
        true
    }
    fn set_base_minimum_delay_ms(&mut self, _: i32) -> bool {
        true
    }
    fn base_minimum_delay_ms(&self) -> i32 {
        self.id
    }
    fn target_delay_ms(&self) -> i32 {
        self.context.environment.timestamp().microseconds().unwrap() as i32
    }
    fn filtered_current_delay_ms(&self) -> i32 {
        i32::MIN
    }
    fn network_statistics(
        &mut self,
        _: Option<NetEqOutput<'_, NetEqNetworkStatistics>>,
    ) -> NetEqReturnCode {
        NetEqReturnCode::OK
    }
    fn current_network_statistics(&self) -> NetEqNetworkStatistics {
        Default::default()
    }
    fn lifetime_statistics(&self) -> NetEqLifetimeStatistics {
        Default::default()
    }
    fn operations_and_state(&self) -> NetEqOperationsAndState {
        Default::default()
    }
    fn playout_timestamp(&self) -> Option<u32> {
        None
    }
    fn last_output_sample_rate_hz(&self) -> i32 {
        i32::MIN
    }
    fn flush_buffers(&mut self) {}
    fn sync_buffer_size_ms(&self) -> i32 {
        i32::MAX
    }
}
#[derive(Clone, Copy)]
enum Behavior {
    Products,
    RejectFirst,
    Panic,
}
struct Factory {
    calls: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    product_drops: Arc<AtomicUsize>,
    contexts: Arc<Mutex<Vec<NetEqCreationContext>>>,
    behavior: Behavior,
    panic_drop: bool,
}
impl NetEqFactoryProvider for Factory {
    fn create(
        &mut self,
        context: NetEqCreationContext,
    ) -> Result<Box<dyn NetEqProvider>, NetEqFactoryError> {
        let id = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        match self.behavior {
            Behavior::Panic => panic!("factory panic"),
            Behavior::RejectFirst if id == 1 => {
                return Err(NetEqFactoryError::Rejected("requested rejection".into()));
            }
            _ => {}
        }
        self.contexts.lock().unwrap().push(context.clone());
        Ok(Box::new(Product {
            id: id as i32,
            drops: self.product_drops.clone(),
            context,
        }))
    }
}
impl Drop for Factory {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        if self.panic_drop {
            panic!("factory destructor panic");
        }
    }
}
fn factory(
    behavior: Behavior,
    panic_drop: bool,
) -> (
    NetEqFactory,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<Mutex<Vec<NetEqCreationContext>>>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let product_drops = Arc::new(AtomicUsize::new(0));
    let contexts = Arc::new(Mutex::new(Vec::new()));
    (
        NetEqFactory::from_provider(Factory {
            calls: calls.clone(),
            drops: drops.clone(),
            product_drops: product_drops.clone(),
            contexts: contexts.clone(),
            behavior,
            panic_drop,
        }),
        calls,
        drops,
        product_drops,
        contexts,
    )
}

#[test]
fn contexts_and_independent_products_outlive_all_creation_owners() {
    let (factory, calls, drops, product_drops, contexts) = factory(Behavior::Products, false);
    let clock = ManualClock::new(Duration::from_micros(100)).unwrap();
    let environment = Environment::builder().clock(&clock).build().unwrap();
    let bounds = environment.clock_time_bounds();
    let decoder_factory = AudioDecoderFactory::builtin_opus().unwrap();
    // Construction does not apply stock config constraints to caller products.
    let config = NetEqConfig {
        sample_rate_hz: i32::MIN,
        max_packets_in_buffer: usize::MAX,
        min_delay_ms: i32::MAX,
        max_delay_ms: i32::MIN,
        ..Default::default()
    };
    let first = factory
        .create(&environment, &config, &decoder_factory)
        .unwrap();
    let second = factory
        .clone()
        .create(&environment, &config, &decoder_factory)
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    // Actual provider values identify both products without a stock-tag API.
    assert_eq!(
        (
            first.base_minimum_delay_ms(),
            second.base_minimum_delay_ms()
        ),
        (1, 2)
    );
    drop(factory);
    drop(environment);
    drop(decoder_factory);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    clock.advance(Duration::from_micros(50)).unwrap();
    assert_eq!(
        (
            first.base_minimum_delay_ms(),
            second.base_minimum_delay_ms()
        ),
        (1, 2)
    );
    assert_eq!(first.target_delay_ms(), 150);
    assert_eq!(first.filtered_current_delay_ms(), i32::MIN);
    assert_eq!(first.last_output_sample_rate_hz(), i32::MIN);
    drop(first);
    drop(second);
    assert_eq!(product_drops.load(Ordering::SeqCst), 2);
    let contexts = contexts.lock().unwrap();
    for context in contexts.iter() {
        assert_eq!(context.config, config);
        assert_eq!(context.environment.clock_time_bounds(), bounds);
        assert_eq!(context.environment.timestamp().microseconds().unwrap(), 150);
        let decoder_factory = context.decoder_factory.as_ref().unwrap();
        let format = SdpAudioFormat::new(b"opus".to_vec(), 48000, 2);
        assert!(decoder_factory.is_supported_decoder(&format).unwrap());
        assert!(
            decoder_factory
                .create(&context.environment, &format)
                .unwrap()
                .is_some()
        );
    }
}

#[test]
fn absent_decoder_factory_reaches_the_owned_creation_context() {
    let (factory, calls, _, _, contexts) = factory(Behavior::Products, false);
    let environment = Environment::builder().build().unwrap();
    let config = NetEqConfig::default();
    let product = factory
        .create_with_decoder_factory(&environment, &config, None)
        .unwrap();
    drop(factory);
    drop(environment);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(product.base_minimum_delay_ms(), 1);
    let contexts = contexts.lock().unwrap();
    assert!(contexts[0].decoder_factory.is_none());
    assert_eq!(contexts[0].config, config);
}

#[test]
fn same_registration_reentry_is_rejected_before_locking_and_remains_observable() {
    struct Reenter(Arc<Mutex<Option<NetEqFactory>>>);
    impl NetEqFactoryProvider for Reenter {
        fn create(
            &mut self,
            context: NetEqCreationContext,
        ) -> Result<Box<dyn NetEqProvider>, NetEqFactoryError> {
            let factory = self.0.lock().unwrap().as_ref().unwrap().clone();
            match factory.create_with_decoder_factory(&context.environment, &context.config, None) {
                Err(error) => Err(error),
                Ok(_) => panic!("recursive creation must not dispatch"),
            }
        }
    }
    let slot = Arc::new(Mutex::new(None));
    let factory = NetEqFactory::from_provider(Reenter(slot.clone()));
    *slot.lock().unwrap() = Some(factory.clone());
    let observer = factory.failure_observer();
    let environment = Environment::builder().build().unwrap();
    for _ in 0..2 {
        assert!(matches!(
            factory.create_with_decoder_factory(&environment, &NetEqConfig::default(), None),
            Err(NetEqFactoryError::ReentrantCall)
        ));
    }
    slot.lock().unwrap().take();
    drop(factory);
    assert_eq!(observer.failure(), Some(NetEqFactoryError::ReentrantCall));
}

#[test]
fn rejection_is_typed_and_does_not_disable_unrelated_product_creation() {
    let (factory, calls, _, _, _) = factory(Behavior::RejectFirst, false);
    let observer = factory.failure_observer();
    let environment = Environment::builder().build().unwrap();
    let decoder = AudioDecoderFactory::builtin_opus().unwrap();
    let config = NetEqConfig::default();
    assert!(
        matches!(factory.create(&environment, &config, &decoder), Err(NetEqFactoryError::Rejected(message)) if message == "requested rejection")
    );
    let product = factory.create(&environment, &config, &decoder).unwrap();
    assert_eq!(product.base_minimum_delay_ms(), 2);
    assert!(!product.is_failed());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    drop(factory);
    assert_eq!(
        observer.failure(),
        Some(NetEqFactoryError::Rejected("requested rejection".into()))
    );
}

#[test]
fn factory_panic_disables_callbacks_and_destructor_panic_is_contained() {
    let (factory, calls, drops, _, _) = factory(Behavior::Panic, true);
    let observer = factory.failure_observer();
    let environment = Environment::builder().build().unwrap();
    let decoder = AudioDecoderFactory::builtin_opus().unwrap();
    for _ in 0..2 {
        assert!(matches!(
            factory.create(&environment, &NetEqConfig::default(), &decoder),
            Err(NetEqFactoryError::Panicked)
        ));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(factory);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(observer.failure(), Some(NetEqFactoryError::Panicked));
}

#[test]
fn product_failure_observer_survives_factory_and_product_destruction() {
    let (factory, _, _, _, _) = factory(Behavior::Products, false);
    let environment = Environment::builder().build().unwrap();
    let decoder = AudioDecoderFactory::builtin_opus().unwrap();
    let mut product = factory
        .create(&environment, &NetEqConfig::default(), &decoder)
        .unwrap();
    let observer = product.failure_observer().unwrap();
    drop(factory);
    let mut frame = AudioFrame::new();
    assert_eq!(
        product.get_audio(&mut frame, None, None, None),
        Ok(NetEqReturnCode::FAIL)
    );
    drop(product);
    assert_eq!(observer.failure(), Some(NetEqFailure::Panicked));
}
