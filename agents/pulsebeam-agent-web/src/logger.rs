use serde::Deserialize;
use wasm_bindgen::JsValue;

struct CoreConsoleLogger;

static CORE_CONSOLE_LOGGER: CoreConsoleLogger = CoreConsoleLogger;

#[derive(Clone, Copy, Default, Deserialize, PartialEq, PartialOrd)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LogLevel {
    Off,
    Error,
    #[default]
    Warn,
    Info,
    Debug,
    Trace,
}

pub(crate) struct BrowserLogger {
    level: LogLevel,
}

impl BrowserLogger {
    pub(crate) fn new(level: LogLevel) -> Self {
        let _ = log::set_logger(&CORE_CONSOLE_LOGGER);
        log::set_max_level(log::LevelFilter::Trace);
        Self { level }
    }

    pub(crate) fn debug(&self, target: &str, message: String) {
        self.log(LogLevel::Debug, target, message);
    }

    pub(crate) fn info(&self, target: &str, message: &str) {
        self.log(LogLevel::Info, target, message);
    }

    pub(crate) fn warn(&self, target: &str, message: &str) {
        self.log(LogLevel::Warn, target, message);
    }

    fn log(&self, level: LogLevel, target: &str, message: impl AsRef<str>) {
        if self.level == LogLevel::Off || level > self.level {
            return;
        }
        let message = message.as_ref();
        let rendered = JsValue::from_str(&format!("[{target}] {message}"));
        match level {
            LogLevel::Off => {}
            LogLevel::Error => web_sys::console::error_1(&rendered),
            LogLevel::Warn => web_sys::console::warn_1(&rendered),
            LogLevel::Info => web_sys::console::info_1(&rendered),
            LogLevel::Debug | LogLevel::Trace => web_sys::console::debug_1(&rendered),
        }
    }
}

impl log::Log for CoreConsoleLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.target().starts_with("pulsebeam_agent_core")
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let rendered = JsValue::from_str(&format!("[{}] {}", record.target(), record.args()));
        match record.level() {
            log::Level::Error => web_sys::console::error_1(&rendered),
            log::Level::Warn => web_sys::console::warn_1(&rendered),
            log::Level::Info => web_sys::console::info_1(&rendered),
            log::Level::Debug | log::Level::Trace => web_sys::console::debug_1(&rendered),
        }
    }

    fn flush(&self) {}
}

impl From<LogLevel> for agent_core::LogLevel {
    fn from(value: LogLevel) -> Self {
        match value {
            LogLevel::Off => Self::Off,
            LogLevel::Error => Self::Error,
            LogLevel::Warn => Self::Warn,
            LogLevel::Info => Self::Info,
            LogLevel::Debug => Self::Debug,
            LogLevel::Trace => Self::Trace,
        }
    }
}
