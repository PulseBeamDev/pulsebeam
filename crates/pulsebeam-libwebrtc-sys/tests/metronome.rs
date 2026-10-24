use pulsebeam_webrtc_sys::*;
use std::sync::{Arc, Mutex};

struct Source(Arc<Mutex<Vec<std::thread::ThreadId>>>);
impl MetronomeProvider for Source {
    fn tick_period(&self) -> TimeDelta {
        TimeDelta::from_millis(10).unwrap()
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.0.lock().unwrap().push(std::thread::current().id());
    }
}
#[test]
fn actor_factory_retains_both_providers_through_migration() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Metronome>();
    let encode = Arc::new(Mutex::new(Vec::new()));
    let decode = Arc::new(Mutex::new(Vec::new()));
    let encode_handle = Metronome::from_provider(Source(encode.clone()));
    let decode_handle = Metronome::from_provider(Source(decode.clone()));
    let session = ProductionSession::builder(ProductionSessionConfig::default())
        .encode_metronome(&encode_handle)
        .decode_metronome(&decode_handle)
        .build()
        .unwrap();
    drop((encode_handle, decode_handle));
    assert!(encode.lock().unwrap().is_empty());
    assert!(decode.lock().unwrap().is_empty());
    let actor = std::thread::spawn(move || {
        let owner = std::thread::current().id();
        let mut session = session;
        session.shutdown().unwrap();
        drop(session);
        owner
    })
    .join()
    .unwrap();
    let encode = encode.lock().unwrap();
    let decode = decode.lock().unwrap();
    assert_eq!(encode.len(), 1);
    assert_eq!(decode.len(), 1);
    assert_eq!(
        encode[0], decode[0],
        "both native dependencies retire on the same sequence"
    );
    assert_ne!(
        encode[0], actor,
        "application actor must not dispose worker-owned providers"
    );
}

#[test]
fn native_factory_retires_providers_on_the_supplied_worker() {
    let worker = WorkerThread::start().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    assert!(worker.post(move || tx.send(std::thread::current().id()).unwrap()));
    let worker_id = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
    let encode = Arc::new(Mutex::new(Vec::new()));
    let decode = Arc::new(Mutex::new(Vec::new()));
    let encode_handle = Metronome::from_provider(Source(encode.clone()));
    let decode_handle = Metronome::from_provider(Source(decode.clone()));
    let factory = PeerConnectionFactory::builder()
        .worker_thread(worker)
        .encode_metronome(&encode_handle)
        .decode_metronome(&decode_handle)
        .build()
        .unwrap();
    drop((encode_handle, decode_handle));
    assert!(encode.lock().unwrap().is_empty());
    assert!(decode.lock().unwrap().is_empty());
    drop(factory);
    assert_eq!(*encode.lock().unwrap(), [worker_id]);
    assert_eq!(*decode.lock().unwrap(), [worker_id]);
}
