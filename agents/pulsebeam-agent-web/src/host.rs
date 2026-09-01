use agent_core::host::{Host, Instant, timestamp};
use alloc::format;

fn now() -> Instant {
    let ms = web_sys::window().unwrap().performance().unwrap().now();

    Instant::from_micros((ms * 1_000.0) as u64)
}

fn log(record: &log::Record<'_>) {
    let module = record.module_path().unwrap_or(record.target());
    let file = record.file().unwrap_or("?");
    let line = record.line().unwrap_or(0);

    let msg = format!(
        "{} {:<5} {} {}:{}: {}",
        timestamp().as_millis(),
        record.level(),
        module,
        file,
        line,
        record.args(),
    );

    let msg = wasm_bindgen::JsValue::from_str(&msg);

    match record.level() {
        log::Level::Error => web_sys::console::error_1(&msg),
        log::Level::Warn => web_sys::console::warn_1(&msg),
        log::Level::Info => web_sys::console::info_1(&msg),
        log::Level::Debug => web_sys::console::debug_1(&msg),
        log::Level::Trace => web_sys::console::debug_1(&msg),
    }
}

pub fn install() {
    let host = Host {
        now,
        log,
        installed_at: now(),
    };
    agent_core::host::install(host, log::LevelFilter::Debug);
}
