//! Matching-artifact coverage of the protobuf-enabled stock dump implementation.
#![cfg(unix)]
use pulsebeam_webrtc_sys::*;
use std::{
    fs::{self, File},
    io::Write,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "pulsebeam-apm-dump-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn worker() -> TaskQueue {
    TaskQueueFactory::default_threaded()
        .unwrap()
        .create_queue("explicit-aec-worker", QueuePriority::Normal)
        .unwrap()
}
fn processor(preamp: bool) -> AudioProcessingModule {
    let mut config = AudioProcessorConfig::default();
    config.pre_amplifier.enabled = preamp;
    AudioProcessingModule::builtin_with_config(&Environment::builder().build().unwrap(), &config)
        .unwrap()
}
fn capture(p: &mut AudioProcessingModule) {
    let mut pcm = vec![1000; 160];
    pcm.extend([777; 9]);
    assert_eq!(
        p.process_interleaved(
            AudioProcessingStream {
                sample_rate_hz: 16000,
                channels: 1
            },
            &mut pcm
        )
        .unwrap(),
        AudioProcessingStatus::OK
    );
    assert_eq!(&pcm[160..], &[777; 9]);
}
fn varint(data: &[u8], offset: &mut usize) -> u64 {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let byte = data[*offset];
        *offset += 1;
        value |= u64::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return value;
        }
    }
    panic!("invalid native protobuf varint");
}
fn fields(data: &[u8]) -> Vec<(u64, u8, &[u8])> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let key = varint(data, &mut offset);
        let wire = (key & 7) as u8;
        let start;
        match wire {
            0 => {
                start = offset;
                varint(data, &mut offset);
            }
            1 => {
                start = offset;
                offset += 8;
            }
            2 => {
                let size = varint(data, &mut offset) as usize;
                start = offset;
                offset += size;
            }
            5 => {
                start = offset;
                offset += 4;
            }
            _ => panic!("unexpected native protobuf wire type {wire}"),
        }
        result.push((key >> 3, wire, &data[start..offset]));
    }
    result
}
fn records(bytes: &[u8]) -> Vec<&[u8]> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let size = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        result.push(&bytes[offset..offset + size]);
        offset += size;
    }
    result
}
fn streams(bytes: &[u8]) -> Vec<Vec<u8>> {
    records(bytes)
        .into_iter()
        .flat_map(fields)
        .filter(|(tag, wire, _)| *tag == 4 && *wire == 2)
        .map(|(_, _, value)| value.to_vec())
        .collect()
}

#[test]
fn stock_path_dump_contains_real_pcm_and_runtime_events_with_non_utf8_path() {
    let dir = Directory::new();
    let path = dir.0.join(std::ffi::OsString::from_vec(vec![b'd', 255]));
    let mut p = processor(true);
    assert!(
        p.create_and_attach_aec_dump(path.as_os_str().as_bytes(), -1, worker())
            .unwrap()
    );
    assert!(
        p.post_runtime_setting(AudioProcessingRuntimeSetting::CapturePreGain(2.0))
            .unwrap()
    );
    capture(&mut p);
    capture(&mut p);
    p.detach_aec_dump().unwrap();
    p.detach_aec_dump().unwrap();
    let bytes = fs::read(path).unwrap();
    let streams = streams(&bytes);
    assert_eq!(streams.len(), 2);
    let input: Vec<u8> = [1000i16; 160]
        .into_iter()
        .flat_map(i16::to_le_bytes)
        .collect();
    let output: Vec<u8> = [2000i16; 160]
        .into_iter()
        .flat_map(i16::to_le_bytes)
        .collect();
    assert!(
        fields(&streams[0])
            .iter()
            .any(|(tag, wire, value)| *tag == 1 && *wire == 2 && *value == input)
    );
    assert!(
        fields(&streams[1])
            .iter()
            .any(|(tag, wire, value)| *tag == 2 && *wire == 2 && *value == output)
    );
    assert!(
        records(&bytes)
            .into_iter()
            .flat_map(fields)
            .filter(|(tag, wire, _)| *tag == 6 && *wire == 2)
            .any(
                |(_, _, runtime)| fields(runtime).iter().any(|(tag, wire, value)| *tag == 1
                    && *wire == 5
                    && *value == 2.0f32.to_le_bytes())
            )
    );
}

#[test]
fn owning_file_preserves_position_and_worker_survives_processor_destruction() {
    let dir = Directory::new();
    let path = dir.0.join("owned-file");
    let mut file = File::create(&path).unwrap();
    file.write_all(b"kept").unwrap();
    let completed = Arc::new(AtomicBool::new(false));
    let flag = completed.clone();
    let queue = worker();
    assert!(queue.post(move || {
        std::thread::sleep(Duration::from_millis(20));
        flag.store(true, Ordering::Release);
    }));
    {
        let mut p = processor(false);
        assert!(
            p.create_and_attach_aec_dump_file(file, i64::MIN, queue)
                .unwrap()
        );
        capture(&mut p);
    } // Module/dumper barrier completes before the moved queue is destroyed.
    assert!(completed.load(Ordering::Acquire));
    let bytes = fs::read(path).unwrap();
    assert_eq!(&bytes[..4], b"kept");
    assert_eq!(streams(&bytes[4..]).len(), 1);
}

#[test]
fn failed_creation_keeps_old_attachment_and_replacement_flushes_its_exact_worker() {
    let dir = Directory::new();
    let first = dir.0.join("first");
    let second = dir.0.join("second");
    let missing = dir.0.join("missing-parent/file");
    let mut p = processor(false);
    assert!(
        p.create_and_attach_aec_dump(first.as_os_str().as_bytes(), -1, worker())
            .unwrap()
    );
    capture(&mut p);
    assert!(
        !p.create_and_attach_aec_dump(missing.as_os_str().as_bytes(), -1, worker())
            .unwrap()
    );
    capture(&mut p);
    assert!(
        p.create_and_attach_aec_dump(second.as_os_str().as_bytes(), -1, worker())
            .unwrap()
    );
    assert_eq!(streams(&fs::read(first).unwrap()).len(), 2);
    capture(&mut p);
    p.detach_aec_dump().unwrap();
    assert_eq!(streams(&fs::read(second).unwrap()).len(), 1);
}

#[test]
fn native_zero_budget_is_successful_creation_not_invented_recording() {
    let dir = Directory::new();
    let path = dir.0.join("empty");
    let mut p = processor(false);
    assert!(
        p.create_and_attach_aec_dump(path.as_os_str().as_bytes(), 0, worker())
            .unwrap()
    );
    capture(&mut p);
    p.detach_aec_dump().unwrap();
    assert!(fs::read(path).unwrap().is_empty());
    // A readonly FILE is also safe when the native budget proves no writes.
    let readonly = File::open(&dir.0.join("empty")).unwrap();
    assert!(
        p.create_and_attach_aec_dump_file(readonly, 0, worker())
            .unwrap()
    );
    capture(&mut p);
    p.detach_aec_dump().unwrap();
}

#[test]
fn worker_and_filename_safety_errors_do_not_enter_native_file_creation() {
    let dir = Directory::new();
    let path = dir.0.join("untouched");
    let mut p = processor(false);
    let clock = ManualClock::new(Duration::from_micros(0)).unwrap();
    let queues = TaskQueueFactory::cooperative(&clock).unwrap();
    let cooperative = queues
        .create_queue("manual", QueuePriority::Normal)
        .unwrap();
    assert_eq!(
        p.create_and_attach_aec_dump(path.as_os_str().as_bytes(), -1, cooperative),
        Err(AudioProcessorError::UnqualifiedDumpWorker)
    );
    assert!(!path.exists());
    let mut closed = worker();
    closed.close();
    assert_eq!(
        p.create_and_attach_aec_dump(path.as_os_str().as_bytes(), -1, closed),
        Err(AudioProcessorError::InvalidDumpWorker)
    );
    let mut nul = path.as_os_str().as_bytes().to_vec();
    nul.extend(b"\0suffix");
    assert_eq!(
        p.create_and_attach_aec_dump(&nul, -1, worker()),
        Err(AudioProcessorError::InvalidDumpPath)
    );
    assert!(!path.exists());
    capture(&mut p);
}
