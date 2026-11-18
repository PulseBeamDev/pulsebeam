use std::collections::VecDeque;
use std::fmt;

const RECORDS: usize = 64;
const MESSAGE_BYTES: usize = 2048;

#[derive(Default)]
pub(crate) struct LogBuffer {
    deferred: bool,
    records: VecDeque<DeferredLog>,
}

pub struct DeferredLog {
    level: log::Level,
    target: &'static str,
    message: String,
}

impl DeferredLog {
    pub fn emit(self) {
        log::log!(target: self.target, self.level, "{}", self.message);
    }
}

impl LogBuffer {
    pub(crate) fn defer(&mut self) {
        self.deferred = true;
    }

    pub(crate) fn drain(&mut self) -> Vec<DeferredLog> {
        self.records.drain(..).collect()
    }

    pub(crate) fn write(
        &mut self,
        level: log::Level,
        target: &'static str,
        args: fmt::Arguments<'_>,
    ) {
        if !self.deferred {
            log::log!(target: target, level, "{args}");
            return;
        }
        if self.records.len() >= RECORDS {
            return;
        }
        let mut message = Message(String::new());
        if fmt::write(&mut message, args).is_ok() {
            self.records.push_back(DeferredLog {
                level,
                target,
                message: message.0,
            });
        }
    }
}

struct Message(String);
impl fmt::Write for Message {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let remaining = MESSAGE_BYTES.saturating_sub(self.0.len());
        let end = if text.len() <= remaining {
            text.len()
        } else {
            text.char_indices()
                .map(|(index, _)| index)
                .take_while(|index| *index <= remaining)
                .last()
                .unwrap_or_default()
        };
        self.0.push_str(text.get(..end).unwrap_or_default());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_diagnostics_are_bounded_owned_and_utf8_safe() {
        let mut logs = LogBuffer::default();
        logs.defer();
        for _ in 0..1000 {
            logs.write(
                log::Level::Warn,
                "core",
                format_args!("{}", "界".repeat(1000)),
            );
        }
        let records = logs.drain();
        assert_eq!(records.len(), RECORDS);
        assert!(
            records
                .iter()
                .all(|record| record.message.len() <= MESSAGE_BYTES)
        );
        assert!(logs.drain().is_empty());
    }
}
