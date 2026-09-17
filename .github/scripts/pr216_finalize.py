from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    p = Path(path)
    s = p.read_text()
    if old not in s:
        raise SystemExit(f"{label} not found")
    p.write_text(s.replace(old, new, 1))


# Fix current CI compile errors.
replace_once(
    "crates/pulsebeam-rtc/src/egress.rs",
    "owner.update_controller(at, Some((1, true)), &[], Duration::ZERO, 0);",
    "owner.update_controller(at, Some((1, true)), &[], Duration::ZERO, false, 0);",
    "egress update_controller test call",
)

for method in ("recover", "set_missing", "confirm_losses", "emit"):
    replace_once(
        "crates/pulsebeam-rtc/src/sent_history/feedback.rs",
        f"    fn {method}(",
        f"    pub(super) fn {method}(",
        f"test visibility for {method}",
    )

# Do not advance the logical expiry cursor merely because one terminal ring slot is reused;
# older retained entries can still exist. next_deadline already scans retained generations.
replace_once(
    "crates/pulsebeam-rtc/src/sent_history.rs",
    """        if let Some(previous) = self.entries[slot] {\n            self.remove_indexes(previous);\n            // Once a ring generation is overwritten, no earlier sent id can still be\n            // retained. Keep the expiration cursor on a live generation.\n            self.oldest_sent_id = self.oldest_sent_id.max(previous.id.0.saturating_add(1));\n        }\n""",
    """        if let Some(previous) = self.entries[slot] {\n            self.remove_indexes(previous);\n        }\n""",
    "unsafe oldest_sent_id bump",
)

# Make the application-limited contract describe the pinned draft's actual control boundary:
# PulseBeam observes application-limited state but does not inject a private ref_wnd freeze.
replace_once(
    "crates/pulsebeam-rtc/docs/congestion-control.md",
    "| Application-limited behavior | Freeze unsupported growth, decay confidence, demand-aware bounded probes | Collapse estimate to media rate; grow without observations | Separates offered media rate from path capacity and avoids both needless collapse and evidence-free optimism. |",
    "| Application-limited behavior | Keep the draft-defined bytes-in-flight growth bound authoritative; decay PulseBeam confidence and use demand-aware bounded probes | Add a PulseBeam-specific ref_wnd freeze; collapse estimate to media rate | Keeps product policy outside the SCReAM core while low offered load naturally limits growth through the draft's bytes-in-flight state. |",
    "application-limited decision row",
)
replace_once(
    "crates/pulsebeam-rtc/docs/congestion-control.md",
    "- unsupported reference-window growth stops;",
    "- SCReAM's draft-defined bytes-in-flight growth bound remains authoritative; PulseBeam injects no separate application-limited freeze into the core;",
    "application-limited behavior bullet",
)
replace_once(
    "crates/pulsebeam-rtc/docs/congestion-control.md",
    "- Feedback-stale and application-limited states cannot cause unsupported growth.",
    "- Feedback-stale timer polls cannot replay growth; under low offered load, SCReAM growth remains bounded by the draft-defined bytes-in-flight gate.",
    "application-limited validation bullet",
)
replace_once(
    "crates/pulsebeam-rtc/src/congestion.rs",
    "fn application_limited_feedback_does_not_inflate_reference_window() {",
    "fn draft_bytes_in_flight_gate_limits_growth_under_low_offer() {",
    "application-limited test name",
)
