from pathlib import Path

path = Path("crates/pulsebeam-rtc/docs/congestion-control.md")
text = path.read_text()
old = "| Application-limited | Offered RTP below 85% threshold for 200 ms with no pacing/window block | Freeze unsupported growth, decay confidence, demand-aware probes | Offered load recovers or another state dominates |"
new = "| Application-limited | Offered RTP below 85% threshold for 200 ms with no pacing/window block | Keep the draft bytes-in-flight growth bound authoritative, decay confidence, demand-aware probes | Offered load recovers or another state dominates |"
if text.count(old) != 1:
    raise SystemExit(f"expected one application-limited table row, found {text.count(old)}")
path.write_text(text.replace(old, new, 1))
