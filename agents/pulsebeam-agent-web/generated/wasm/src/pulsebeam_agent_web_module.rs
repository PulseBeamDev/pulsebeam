#[allow(unused_imports)]
use uniffi_runtime_javascript::{self as js, uniffi as u, IntoJs, IntoRust};
use wasm_bindgen::prelude::wasm_bindgen;
extern "C" {
    fn uniffi_pulsebeam_agent_web_fn_clone_mediaregistryproof(
        handle: u64,
        status_: &mut u::RustCallStatus,
    ) -> u64;
    fn uniffi_pulsebeam_agent_web_fn_free_mediaregistryproof(
        handle: u64,
        status_: &mut u::RustCallStatus,
    );
    fn uniffi_pulsebeam_agent_web_fn_constructor_mediaregistryproof_new(
        status_: &mut u::RustCallStatus,
    ) -> u64;
    fn uniffi_pulsebeam_agent_web_fn_method_mediaregistryproof_create_stream(
        ptr: u64,
        status_: &mut u::RustCallStatus,
    ) -> u64;
    fn uniffi_pulsebeam_agent_web_fn_method_mediaregistryproof_release_stream(
        ptr: u64,
        stream: u64,
        status_: &mut u::RustCallStatus,
    );
    fn uniffi_pulsebeam_agent_web_fn_method_mediaregistryproof_release_track(
        ptr: u64,
        track: u64,
        status_: &mut u::RustCallStatus,
    );
    fn uniffi_pulsebeam_agent_web_fn_method_mediaregistryproof_retained_media(
        ptr: u64,
        status_: &mut u::RustCallStatus,
    ) -> u64;
    fn uniffi_pulsebeam_agent_web_fn_method_mediaregistryproof_round_trip_stream(
        ptr: u64,
        stream: u64,
        status_: &mut u::RustCallStatus,
    ) -> u64;
    fn uniffi_pulsebeam_agent_web_fn_method_mediaregistryproof_round_trip_track(
        ptr: u64,
        track: u64,
        status_: &mut u::RustCallStatus,
    ) -> u64;
    fn uniffi_pulsebeam_agent_web_fn_func_normalize_agent_config(
        config: u::RustBuffer,
        status_: &mut u::RustCallStatus,
    ) -> u::RustBuffer;
    fn uniffi_pulsebeam_agent_web_checksum_func_normalize_agent_config() -> u16;
    fn uniffi_pulsebeam_agent_web_checksum_method_mediaregistryproof_create_stream() -> u16;
    fn uniffi_pulsebeam_agent_web_checksum_method_mediaregistryproof_release_stream() -> u16;
    fn uniffi_pulsebeam_agent_web_checksum_method_mediaregistryproof_release_track() -> u16;
    fn uniffi_pulsebeam_agent_web_checksum_method_mediaregistryproof_retained_media() -> u16;
    fn uniffi_pulsebeam_agent_web_checksum_method_mediaregistryproof_round_trip_stream() -> u16;
    fn uniffi_pulsebeam_agent_web_checksum_method_mediaregistryproof_round_trip_track() -> u16;
    fn uniffi_pulsebeam_agent_web_checksum_constructor_mediaregistryproof_new() -> u16;
    fn ffi_pulsebeam_agent_web_uniffi_contract_version() -> u32;
}
#[wasm_bindgen]
pub fn ubrn_uniffi_pulsebeam_agent_web_fn_clone_mediaregistryproof(
    handle: js::Handle,
    f_status_: &mut js::RustCallStatus,
) -> js::Handle {
    let mut u_status_ = u::RustCallStatus::default();
    let value_ = unsafe {
        uniffi_pulsebeam_agent_web_fn_clone_mediaregistryproof(
            u64::into_rust(handle),
            &mut u_status_,
        )
    };
    f_status_.copy_from(u_status_);
    value_.into_js()
}
#[wasm_bindgen]
pub fn ubrn_uniffi_pulsebeam_agent_web_fn_free_mediaregistryproof(
    handle: js::Handle,
    f_status_: &mut js::RustCallStatus,
) {
    let mut u_status_ = u::RustCallStatus::default();
    unsafe {
        uniffi_pulsebeam_agent_web_fn_free_mediaregistryproof(
            u64::into_rust(handle),
            &mut u_status_,
        )
    };
    f_status_.copy_from(u_status_);
}
#[wasm_bindgen]
pub fn ubrn_uniffi_pulsebeam_agent_web_fn_constructor_mediaregistryproof_new(
    f_status_: &mut js::RustCallStatus,
) -> js::Handle {
    let mut u_status_ = u::RustCallStatus::default();
    let value_ = unsafe {
        uniffi_pulsebeam_agent_web_fn_constructor_mediaregistryproof_new(&mut u_status_)
    };
    f_status_.copy_from(u_status_);
    value_.into_js()
}
#[wasm_bindgen]
pub fn ubrn_uniffi_pulsebeam_agent_web_fn_method_mediaregistryproof_create_stream(
    ptr: js::Handle,
    f_status_: &mut js::RustCallStatus,
) -> js::UInt64 {
    let mut u_status_ = u::RustCallStatus::default();
    let value_ = unsafe {
        uniffi_pulsebeam_agent_web_fn_method_mediaregistryproof_create_stream(
            u64::into_rust(ptr),
            &mut u_status_,
        )
    };
    f_status_.copy_from(u_status_);
    value_.into_js()
}
#[wasm_bindgen]
pub fn ubrn_uniffi_pulsebeam_agent_web_fn_method_mediaregistryproof_release_stream(
    ptr: js::Handle,
    stream: js::UInt64,
    f_status_: &mut js::RustCallStatus,
) {
    let mut u_status_ = u::RustCallStatus::default();
    unsafe {
        uniffi_pulsebeam_agent_web_fn_method_mediaregistryproof_release_stream(
            u64::into_rust(ptr),
            u64::into_rust(stream),
            &mut u_status_,
        )
    };
    f_status_.copy_from(u_status_);
}
#[wasm_bindgen]
pub fn ubrn_uniffi_pulsebeam_agent_web_fn_method_mediaregistryproof_release_track(
    ptr: js::Handle,
    track: js::UInt64,
    f_status_: &mut js::RustCallStatus,
) {
    let mut u_status_ = u::RustCallStatus::default();
    unsafe {
        uniffi_pulsebeam_agent_web_fn_method_mediaregistryproof_release_track(
            u64::into_rust(ptr),
            u64::into_rust(track),
            &mut u_status_,
        )
    };
    f_status_.copy_from(u_status_);
}
#[wasm_bindgen]
pub fn ubrn_uniffi_pulsebeam_agent_web_fn_method_mediaregistryproof_retained_media(
    ptr: js::Handle,
    f_status_: &mut js::RustCallStatus,
) -> js::UInt64 {
    let mut u_status_ = u::RustCallStatus::default();
    let value_ = unsafe {
        uniffi_pulsebeam_agent_web_fn_method_mediaregistryproof_retained_media(
            u64::into_rust(ptr),
            &mut u_status_,
        )
    };
    f_status_.copy_from(u_status_);
    value_.into_js()
}
#[wasm_bindgen]
pub fn ubrn_uniffi_pulsebeam_agent_web_fn_method_mediaregistryproof_round_trip_stream(
    ptr: js::Handle,
    stream: js::UInt64,
    f_status_: &mut js::RustCallStatus,
) -> js::UInt64 {
    let mut u_status_ = u::RustCallStatus::default();
    let value_ = unsafe {
        uniffi_pulsebeam_agent_web_fn_method_mediaregistryproof_round_trip_stream(
            u64::into_rust(ptr),
            u64::into_rust(stream),
            &mut u_status_,
        )
    };
    f_status_.copy_from(u_status_);
    value_.into_js()
}
#[wasm_bindgen]
pub fn ubrn_uniffi_pulsebeam_agent_web_fn_method_mediaregistryproof_round_trip_track(
    ptr: js::Handle,
    track: js::UInt64,
    f_status_: &mut js::RustCallStatus,
) -> js::UInt64 {
    let mut u_status_ = u::RustCallStatus::default();
    let value_ = unsafe {
        uniffi_pulsebeam_agent_web_fn_method_mediaregistryproof_round_trip_track(
            u64::into_rust(ptr),
            u64::into_rust(track),
            &mut u_status_,
        )
    };
    f_status_.copy_from(u_status_);
    value_.into_js()
}
#[wasm_bindgen]
pub fn ubrn_uniffi_pulsebeam_agent_web_fn_func_normalize_agent_config(
    config: js::ForeignBytes,
    f_status_: &mut js::RustCallStatus,
) -> js::ForeignBytes {
    let mut u_status_ = u::RustCallStatus::default();
    let value_ = unsafe {
        uniffi_pulsebeam_agent_web_fn_func_normalize_agent_config(
            u::RustBuffer::into_rust(config),
            &mut u_status_,
        )
    };
    f_status_.copy_from(u_status_);
    value_.into_js()
}
#[wasm_bindgen]
pub unsafe fn ubrn_uniffi_pulsebeam_agent_web_checksum_func_normalize_agent_config() -> js::UInt16 {
    uniffi_pulsebeam_agent_web_checksum_func_normalize_agent_config().into_js()
}
#[wasm_bindgen]
pub unsafe fn ubrn_uniffi_pulsebeam_agent_web_checksum_method_mediaregistryproof_create_stream() -> js::UInt16 {
    uniffi_pulsebeam_agent_web_checksum_method_mediaregistryproof_create_stream()
        .into_js()
}
#[wasm_bindgen]
pub unsafe fn ubrn_uniffi_pulsebeam_agent_web_checksum_method_mediaregistryproof_release_stream() -> js::UInt16 {
    uniffi_pulsebeam_agent_web_checksum_method_mediaregistryproof_release_stream()
        .into_js()
}
#[wasm_bindgen]
pub unsafe fn ubrn_uniffi_pulsebeam_agent_web_checksum_method_mediaregistryproof_release_track() -> js::UInt16 {
    uniffi_pulsebeam_agent_web_checksum_method_mediaregistryproof_release_track()
        .into_js()
}
#[wasm_bindgen]
pub unsafe fn ubrn_uniffi_pulsebeam_agent_web_checksum_method_mediaregistryproof_retained_media() -> js::UInt16 {
    uniffi_pulsebeam_agent_web_checksum_method_mediaregistryproof_retained_media()
        .into_js()
}
#[wasm_bindgen]
pub unsafe fn ubrn_uniffi_pulsebeam_agent_web_checksum_method_mediaregistryproof_round_trip_stream() -> js::UInt16 {
    uniffi_pulsebeam_agent_web_checksum_method_mediaregistryproof_round_trip_stream()
        .into_js()
}
#[wasm_bindgen]
pub unsafe fn ubrn_uniffi_pulsebeam_agent_web_checksum_method_mediaregistryproof_round_trip_track() -> js::UInt16 {
    uniffi_pulsebeam_agent_web_checksum_method_mediaregistryproof_round_trip_track()
        .into_js()
}
#[wasm_bindgen]
pub unsafe fn ubrn_uniffi_pulsebeam_agent_web_checksum_constructor_mediaregistryproof_new() -> js::UInt16 {
    uniffi_pulsebeam_agent_web_checksum_constructor_mediaregistryproof_new().into_js()
}
#[wasm_bindgen]
pub unsafe fn ubrn_ffi_pulsebeam_agent_web_uniffi_contract_version() -> js::UInt32 {
    ffi_pulsebeam_agent_web_uniffi_contract_version().into_js()
}
