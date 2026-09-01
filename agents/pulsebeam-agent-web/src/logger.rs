use std::cell::RefCell;

use js_sys::Function;
use wasm_bindgen::{JsValue, prelude::wasm_bindgen};

struct BrowserLogger;

static LOGGER: BrowserLogger = BrowserLogger;

thread_local! {
    static SINK: RefCell<Option<Function>> = const { RefCell::new(None) };
}

impl log::Log for BrowserLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let level = record.level().to_string().to_lowercase();
        let target = record.target();
        let message = record.args().to_string();
        let delivered = SINK.with_borrow(|sink| {
            sink.as_ref().is_some_and(|sink| {
                sink.call3(
                    &JsValue::UNDEFINED,
                    &JsValue::from_str(&level),
                    &JsValue::from_str(target),
                    &JsValue::from_str(&message),
                )
                .is_ok()
            })
        });
        if delivered {
            return;
        }
        let rendered = JsValue::from_str(&format!("[{target}] {message}"));
        match record.level() {
            log::Level::Error => web_sys::console::error_1(&rendered),
            log::Level::Warn => web_sys::console::warn_1(&rendered),
            log::Level::Info => web_sys::console::info_1(&rendered),
            log::Level::Debug | log::Level::Trace => web_sys::console::debug_1(&rendered),
        }
    }

    fn flush(&self) {}
}

#[wasm_bindgen]
pub fn configure_logging(level: &str, sink: Option<Function>) -> Result<(), JsValue> {
    let filter = match level {
        "off" => log::LevelFilter::Off,
        "error" => log::LevelFilter::Error,
        "warn" => log::LevelFilter::Warn,
        "info" => log::LevelFilter::Info,
        "debug" => log::LevelFilter::Debug,
        "trace" => log::LevelFilter::Trace,
        _ => return Err(JsValue::from_str("invalid log level")),
    };
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(filter);
    SINK.set(sink);
    Ok(())
}
