use js_sys::Function;
use serde::Deserialize;
use wasm_bindgen::JsValue;

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
    sink: Option<Function>,
}

impl BrowserLogger {
    pub(crate) fn new(level: LogLevel, sink: Option<Function>) -> Self {
        Self { level, sink }
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
        let delivered = self.sink.as_ref().is_some_and(|sink| {
            sink.call3(
                &JsValue::UNDEFINED,
                &JsValue::from_str(level.as_str()),
                &JsValue::from_str(target),
                &JsValue::from_str(message),
            )
            .is_ok()
        });
        if delivered {
            return;
        }
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

impl LogLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}
