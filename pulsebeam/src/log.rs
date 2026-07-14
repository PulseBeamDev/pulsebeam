//! This module defines cross-cutting fastrace logging helpers and targets.

use std::borrow::Cow;
use std::fmt::{Debug, Display};

use fastrace::prelude::{Event, LocalSpan};

pub(crate) const TARGET_VIDEO: &str = "pulsebeam::x::video";
pub(crate) const TARGET_AUDIO: &str = "pulsebeam::x::audio";

pub use crate::{debug, error, info, trace, warn};

pub fn emit(
	level: &'static str,
	target: Option<String>,
	message: String,
	properties: Vec<(Cow<'static, str>, Cow<'static, str>)>,
) {
	let mut event = Event::new(message).with_property(|| ("level", level));
	if let Some(target) = target {
		event = event.with_property(|| ("target", target));
	}
	if !properties.is_empty() {
		event = event.with_properties(|| properties);
	}
	LocalSpan::add_event(event);
}

pub fn prop_display(
	key: &'static str,
	value: &impl Display,
) -> (Cow<'static, str>, Cow<'static, str>) {
	(Cow::Borrowed(key), Cow::Owned(value.to_string()))
}

pub fn prop_debug(
	key: &'static str,
	value: &impl Debug,
) -> (Cow<'static, str>, Cow<'static, str>) {
	(Cow::Borrowed(key), Cow::Owned(format!("{value:?}")))
}

#[macro_export]
macro_rules! __pulsebeam_log_parse {
	($level:expr, $target:expr, [$($props:expr),* $(,)?], $fmt:literal $(, $args:expr)* $(,)?) => {{
		$crate::log::emit($level, $target, format!($fmt $(, $args)*), vec![$($props),*]);
	}};
	($level:expr, $target:expr, [$($props:expr),* $(,)?], target: $new_target:expr, $($rest:tt)+) => {{
		$crate::__pulsebeam_log_parse!(
			$level,
			Some(($new_target).to_string()),
			[$($props),*],
			$($rest)+
		);
	}};
	($level:expr, $target:expr, [$($props:expr),* $(,)?], $name:ident = %$value:expr, $($rest:tt)+) => {{
		$crate::__pulsebeam_log_parse!(
			$level,
			$target,
			[$($props,)* $crate::log::prop_display(stringify!($name), &$value)],
			$($rest)+
		);
	}};
	($level:expr, $target:expr, [$($props:expr),* $(,)?], $name:ident = ?$value:expr, $($rest:tt)+) => {{
		$crate::__pulsebeam_log_parse!(
			$level,
			$target,
			[$($props,)* $crate::log::prop_debug(stringify!($name), &$value)],
			$($rest)+
		);
	}};
	($level:expr, $target:expr, [$($props:expr),* $(,)?], $name:ident = $value:expr, $($rest:tt)+) => {{
		$crate::__pulsebeam_log_parse!(
			$level,
			$target,
			[$($props,)* $crate::log::prop_debug(stringify!($name), &$value)],
			$($rest)+
		);
	}};
	($level:expr, $target:expr, [$($props:expr),* $(,)?], %$name:ident, $($rest:tt)+) => {{
		$crate::__pulsebeam_log_parse!(
			$level,
			$target,
			[$($props,)* $crate::log::prop_display(stringify!($name), &$name)],
			$($rest)+
		);
	}};
	($level:expr, $target:expr, [$($props:expr),* $(,)?], ?$name:ident, $($rest:tt)+) => {{
		$crate::__pulsebeam_log_parse!(
			$level,
			$target,
			[$($props,)* $crate::log::prop_debug(stringify!($name), &$name)],
			$($rest)+
		);
	}};
	($level:expr, $target:expr, [$($props:expr),* $(,)?], $name:ident, $($rest:tt)+) => {{
		$crate::__pulsebeam_log_parse!(
			$level,
			$target,
			[$($props,)* $crate::log::prop_debug(stringify!($name), &$name)],
			$($rest)+
		);
	}};
}

#[macro_export]
macro_rules! __pulsebeam_log {
	($level:expr, $($rest:tt)+) => {{
		$crate::__pulsebeam_log_parse!($level, None, [], $($rest)+);
	}};
}

#[macro_export]
macro_rules! trace {
	($($rest:tt)+) => {{
		$crate::__pulsebeam_log!("trace", $($rest)+);
	}};
}

#[macro_export]
macro_rules! debug {
	($($rest:tt)+) => {{
		$crate::__pulsebeam_log!("debug", $($rest)+);
	}};
}

#[macro_export]
macro_rules! info {
	($($rest:tt)+) => {{
		$crate::__pulsebeam_log!("info", $($rest)+);
	}};
}

#[macro_export]
macro_rules! warn {
	($($rest:tt)+) => {{
		$crate::__pulsebeam_log!("warn", $($rest)+);
	}};
}

#[macro_export]
macro_rules! error {
	($($rest:tt)+) => {{
		$crate::__pulsebeam_log!("error", $($rest)+);
	}};
}
