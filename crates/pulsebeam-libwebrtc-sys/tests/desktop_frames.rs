#![cfg(all(feature = "native", target_os = "linux"))]
//! SDK CPU/shared-frame consumers, not evidence of physical desktop capture.
use pulsebeam_webrtc_sys::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
#[path = "common/panic_payload.rs"]
mod panic_payload;

fn rect(left: i32, top: i32, right: i32, bottom: i32) -> DesktopRect {
    DesktopRect {
        left,
        top,
        right,
        bottom,
    }
}
fn pixels() -> Vec<u8> {
    (1..=16).collect()
}

#[test]
fn sdk_pixels_formats_and_independent_cpu_copies() {
    for format in [
        DesktopFrame::ARGB,
        DesktopFrame::ABGR,
        DesktopFrame::BGRA,
        DesktopFrame::RGBA,
    ] {
        let mut frame = DesktopFrame::new(2, 2, format).unwrap();
        // BasicDesktopFrame allocation need not initialize pixels. Initialize
        // through the SDK before observing any bytes.
        frame.set_data_to_black().unwrap();
        assert!(frame.data_is_black().unwrap());
        assert_eq!(frame.packed_pixels().unwrap(), vec![0; 16]);
        frame
            .copy_pixels_from_buffer(&pixels(), 0, 8, rect(0, 0, 2, 2))
            .unwrap();
        assert_eq!(frame.packed_pixels().unwrap(), pixels());
        assert_eq!(frame.pixel_at(1, 1).unwrap(), [13, 14, 15, 16]);
        assert!(!frame.data_is_black().unwrap());
        let mut copy = frame.try_clone().unwrap();
        assert_eq!(copy.info().pixel_format, format);
        assert_eq!(copy.packed_pixels().unwrap(), pixels());
        copy.set_data_to_black().unwrap();
        assert_eq!(frame.packed_pixels().unwrap(), pixels());
        assert_eq!(copy.packed_pixels().unwrap(), vec![0; 16]);
        copy.copy_pixels_from(&frame, (1, 1), rect(0, 0, 1, 1))
            .unwrap();
        assert_eq!(copy.pixel_at(0, 0).unwrap(), [13, 14, 15, 16]);
        assert_eq!(copy.pixel_at(1, 1).unwrap(), [0; 4]);
    }
}

#[test]
fn metadata_copy_and_native_region_swap_preserve_values() {
    let mut source = DesktopFrame::new(2, 2, DesktopFrame::ARGB).unwrap();
    source.set_data_to_black().unwrap();
    let metadata = DesktopFrameMetadata {
        top_left: (-17, 23),
        dpi: (96, 96),
        device_scale_factor: Some(1.25),
        may_contain_cursor: true,
        capture_time_ms: -83,
        capturer_id: u32::MAX,
        icc_profile: vec![0, 255, 0, 17],
        updated_region: vec![rect(0, 0, 1, 2)],
    };
    source.set_metadata(&metadata).unwrap();
    assert_eq!(source.info().metadata, metadata);
    let mut target = DesktopFrame::new(2, 2, DesktopFrame::ARGB).unwrap();
    target.set_data_to_black().unwrap();
    target.copy_metadata_from(&source).unwrap();
    assert_eq!(target.info().metadata, metadata);
    let old_region = vec![rect(1, 0, 2, 1)];
    target
        .set_metadata(&DesktopFrameMetadata {
            updated_region: old_region.clone(),
            ..Default::default()
        })
        .unwrap();
    target.move_metadata_from(&mut source).unwrap();
    assert_eq!(target.info().metadata, metadata);
    let mut source_after_move = metadata;
    source_after_move.updated_region = old_region;
    assert_eq!(source.info().metadata, source_after_move);
    let snapshot = target.snapshot().unwrap();
    drop(target);
    std::thread::spawn(move || {
        assert_eq!(snapshot.info.metadata.capture_time_ms, -83);
        assert_eq!(snapshot.packed_pixels.unwrap(), vec![0; 16]);
    })
    .join()
    .unwrap();
}

#[test]
fn direct_layout_validation_refuses_out_of_bounds_without_mutation() {
    for (width, height, format) in [
        (-1, 2, DesktopFrame::ARGB),
        (2, -1, DesktopFrame::ARGB),
        (i32::MAX, 1, DesktopFrame::ARGB),
        (1, i32::MAX, DesktopFrame::ARGB),
        (2, 2, 0),
    ] {
        assert_eq!(
            DesktopFrame::new(width, height, format).err().unwrap().kind,
            PeerErrorKind::InvalidParameter
        );
    }
    let empty = DesktopFrame::new(0, 0, DesktopFrame::ARGB).unwrap();
    assert_eq!((empty.info().width, empty.info().height), (0, 0));
    let mut frame = DesktopFrame::new(2, 2, DesktopFrame::ARGB).unwrap();
    frame.set_data_to_black().unwrap();
    for (x, y) in [(-1, 0), (0, -1), (2, 0), (0, 2)] {
        assert_eq!(
            frame.pixel_at(x, y).err().unwrap().kind,
            PeerErrorKind::InvalidParameter
        );
    }
    for (offset, stride, destination) in [
        (usize::MAX, 8, rect(0, 0, 2, 2)),
        (0, -8, rect(0, 0, 2, 2)),
        (0, 8, rect(0, 0, 3, 2)),
        (0, 8, rect(1, 0, 0, 2)),
    ] {
        assert_eq!(
            frame
                .copy_pixels_from_buffer(&pixels(), offset, stride, destination)
                .err()
                .unwrap()
                .kind,
            PeerErrorKind::InvalidParameter
        );
        assert_eq!(frame.packed_pixels().unwrap(), vec![0; 16]);
    }
    let before = frame.info();
    assert_eq!(
        frame
            .set_metadata(&DesktopFrameMetadata {
                top_left: (i32::MAX, 0),
                ..Default::default()
            })
            .err()
            .unwrap()
            .kind,
        PeerErrorKind::InvalidParameter
    );
    assert_eq!(frame.info(), before);
}

struct Token(Arc<AtomicUsize>);
impl Drop for Token {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn owning_shared_frame_retains_storage_and_supports_signed_rows() {
    for (stride, offset) in [(12, 0), (-12, 12)] {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut memory = DesktopMemory::heap(24, -37).unwrap();
        memory.retain(Token(drops.clone()));
        memory.write(0, &pixels()[..8]).unwrap();
        memory.write(12, &pixels()[8..]).unwrap();
        let mut frame =
            DesktopFrame::from_shared_memory_at(2, 2, stride, DesktopFrame::ARGB, offset, memory)
                .unwrap();
        let expected = if stride > 0 {
            pixels()
        } else {
            [&pixels()[8..], &pixels()[..8]].concat()
        };
        assert_eq!(frame.packed_pixels().unwrap(), expected);
        assert_eq!(frame.info().stride, stride);
        let shared = frame.info().shared_memory.unwrap();
        assert_eq!(shared.id, -37);
        assert_eq!(shared.size, 24 - offset);
        assert!(!shared.has_handle);
        assert!(frame.try_clone_shared_memory_fd().unwrap().is_none());
        assert_eq!(
            frame.data_is_black().err().unwrap().kind,
            PeerErrorKind::UnsupportedOperation
        );
        assert_eq!(
            frame.set_data_to_black().err().unwrap().kind,
            PeerErrorKind::UnsupportedOperation
        );
        let copy = frame.try_clone().unwrap();
        assert_eq!(copy.packed_pixels().unwrap(), expected);
        assert!(copy.info().shared_memory.is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(frame);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(copy.packed_pixels().unwrap(), expected);
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let mut short = DesktopMemory::heap(15, 0).unwrap();
    short.retain(Token(drops.clone()));
    assert_eq!(
        DesktopFrame::from_shared_memory(2, 2, 8, DesktopFrame::ARGB, short)
            .err()
            .unwrap()
            .kind,
        PeerErrorKind::InvalidParameter
    );
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "constructor failure consumes the allocation and dependencies"
    );
}

#[derive(Clone, Copy)]
enum Mode {
    Accept,
    Reject,
    Short,
    Panic,
}
struct MemoryFactory {
    mode: Mode,
    requested: Arc<Mutex<Vec<usize>>>,
    factory_drops: Arc<AtomicUsize>,
    product_drops: Arc<AtomicUsize>,
}
impl Drop for MemoryFactory {
    fn drop(&mut self) {
        self.factory_drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl DesktopMemoryFactory for MemoryFactory {
    fn create_shared_memory(&mut self, size: usize) -> Option<DesktopMemory> {
        self.requested.lock().unwrap().push(size);
        let capacity = match self.mode {
            Mode::Accept => size,
            Mode::Reject => return None,
            Mode::Short => size - 1,
            Mode::Panic => panic_payload::raise(),
        };
        let mut memory = DesktopMemory::heap(capacity, -19).unwrap();
        memory.write(0, &vec![83; capacity]).unwrap();
        memory.retain(Token(self.product_drops.clone()));
        Some(memory)
    }
}

#[test]
fn actual_sdk_shared_frame_factory_distinguishes_rejection_and_quarantine() {
    for mode in [Mode::Accept, Mode::Reject, Mode::Short, Mode::Panic] {
        let requested = Arc::new(Mutex::new(Vec::new()));
        let factory_drops = Arc::new(AtomicUsize::new(0));
        let product_drops = Arc::new(AtomicUsize::new(0));
        let handle = DesktopMemoryFactoryHandle::new(MemoryFactory {
            mode,
            requested: requested.clone(),
            factory_drops: factory_drops.clone(),
            product_drops: product_drops.clone(),
        });
        let frame =
            DesktopFrame::from_shared_memory_factory(2, 2, DesktopFrame::ARGB, &handle).unwrap();
        assert_eq!(*requested.lock().unwrap(), vec![16]);
        assert_eq!(
            handle.has_failed(),
            matches!(mode, Mode::Short | Mode::Panic)
        );
        if let Some(frame) = frame {
            assert!(matches!(mode, Mode::Accept));
            assert_eq!(frame.packed_pixels().unwrap(), vec![83; 16]);
            assert_eq!(frame.info().shared_memory.unwrap().id, -19);
            drop(handle);
            assert_eq!(factory_drops.load(Ordering::SeqCst), 1);
            assert_eq!(product_drops.load(Ordering::SeqCst), 0);
            assert_eq!(frame.packed_pixels().unwrap(), vec![83; 16]);
            drop(frame);
            assert_eq!(product_drops.load(Ordering::SeqCst), 1);
        } else {
            assert!(!matches!(mode, Mode::Accept));
            if handle.has_failed() {
                assert!(
                    DesktopFrame::from_shared_memory_factory(2, 2, DesktopFrame::ARGB, &handle)
                        .unwrap()
                        .is_none()
                );
                assert_eq!(
                    *requested.lock().unwrap(),
                    vec![16],
                    "quarantined provider must not be called again"
                );
            }
            drop(handle);
            assert_eq!(factory_drops.load(Ordering::SeqCst), 1);
            assert_eq!(
                product_drops.load(Ordering::SeqCst),
                usize::from(matches!(mode, Mode::Short))
            );
        }
    }
}

#[test]
fn mapped_owned_fd_aliases_outlive_the_original_buffer() {
    let mut memory = DesktopMemory::allocate(16, 71).unwrap();
    memory.write(0, &pixels()).unwrap();
    let fd = memory.try_clone_fd().unwrap().unwrap();
    let mapped = DesktopMemory::from_fd(fd, 16, -71).unwrap();
    assert_eq!(mapped.read(0, 16).unwrap(), pixels());
    drop(memory);
    let frame = DesktopFrame::from_shared_memory(2, 2, 8, DesktopFrame::ARGB, mapped).unwrap();
    assert_eq!(frame.packed_pixels().unwrap(), pixels());
    let fd = frame.try_clone_shared_memory_fd().unwrap().unwrap();
    drop(frame);
    std::thread::spawn(move || {
        let mapped = DesktopMemory::from_fd(fd, 16, 17).unwrap();
        assert_eq!(mapped.read(0, 16).unwrap(), pixels());
    })
    .join()
    .unwrap();
    let fd = std::fs::File::open("/dev/null").unwrap().into();
    assert!(
        DesktopMemory::from_fd(fd, 16, 0).is_err(),
        "unsealed/read-only FD must not be treated as owned writable pixels"
    );
}
