use alloc::format;

static LOGGER: WebLogger = WebLogger;

struct WebLogger;

impl log::Log for WebLogger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        log_record(record);
    }

    fn flush(&self) {}
}

fn log_record(record: &log::Record<'_>) {
    let module = record.module_path().unwrap_or(record.target());
    let file = record.file().unwrap_or("?");
    let line = record.line().unwrap_or(0);

    let msg = format!(
        "{:<5} {} {}:{}: {}",
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
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Debug);
}
