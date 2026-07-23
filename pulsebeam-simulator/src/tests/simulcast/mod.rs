//! Simulcast/BWE simulation tests, organized by real-world use case rather
//! than by isolated network variable.
//!
//! Each use case has its own realistic hard-condition profile and its own
//! definition of "very high QoE" (see `support::qoe_floor` doc comments in
//! each submodule) -- a teleoperation feed and a broadcast viewer do not
//! share a bar. Every test asserts through `pulsebeam_simulator::tests::common::qoe`:
//! decodability is unconditional in all of them, QoE score floors and
//! freeze bounds are calibrated per use case.

mod support;

mod broadcast;
mod multi_party;
mod one_to_one;
mod screen_share;
mod teleoperation;
