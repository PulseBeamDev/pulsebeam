//! Endpoint dependency descriptor mapping for one output SSRC.

mod mapping;
mod templates;

pub use mapping::{Evidence, Mode, OutputMapper, Packet, Rejected};

pub use templates::{Retention, TemplateInterval, TemplateIntervals};
