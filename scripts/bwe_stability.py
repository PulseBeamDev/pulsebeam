#!/usr/bin/env python3
"""
Analyse upstream BWE (Bandwidth Estimation) stability from pulsebeam logs.

Reads lines containing "upstream bwe=..." from a file or stdin, groups samples
by stream_id, aligns every stream's timeline to t=0, then reports per-stream
and aggregate stability statistics.

Usage:
    python3 scripts/bwe_stability.py [OPTIONS] [FILE]

    FILE defaults to stdin when omitted.

Options:
    --min-samples N   Minimum samples required to include a stream (default 5)
    --chart           Print an ASCII timeline chart per stream
    --csv FILE        Also write per-stream rows to a CSV file

To capture upstream BWE logs, run pulsebeam with:
    RUST_LOG=pulsebeam::rtp::monitor=debug ./pulsebeam 2>&1 | tee run.log
    python3 scripts/bwe_stability.py run.log

Log line format (tracing DEBUG):
    <ISO-TS> DEBUG ...: pulsebeam::rtp::monitor: upstream bwe=<VALUE><UNIT> stream_id=<SID>:_
"""

import re
import sys
import math
import csv
import argparse
from collections import defaultdict
from datetime import datetime, timezone

# ---------------------------------------------------------------------------
# Parsing
# ---------------------------------------------------------------------------

# Matches the timestamp, the bitrate value+unit, and the stream_id field.
# The stream_id tracing value is formatted as "vid_XXXX:_" (layer suffix ":_").
_LINE_RE = re.compile(
    r"^(?P<ts>\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z)"
    r".+upstream bwe=(?P<val>[0-9]+\.?[0-9]*)(?P<unit>kbit/s|Mbit/s|bit/s)"
    r".+stream_id=(?P<sid>\S+)"
)


def _to_kbps(val: str, unit: str) -> float:
    v = float(val)
    if unit == "kbit/s":
        return v
    if unit == "Mbit/s":
        return v * 1_000.0
    return v / 1_000.0  # bit/s


def _parse_ts(s: str) -> float:
    """ISO-8601 UTC string → POSIX float seconds."""
    dt = datetime.strptime(s, "%Y-%m-%dT%H:%M:%S.%fZ").replace(tzinfo=timezone.utc)
    return dt.timestamp()


def parse_log(lines) -> dict[str, list[tuple[float, float]]]:
    """Return {stream_id: [(abs_ts_s, kbps), ...]} sorted by timestamp."""
    raw: dict[str, list] = defaultdict(list)
    for line in lines:
        m = _LINE_RE.search(line)
        if not m:
            continue
        ts = _parse_ts(m.group("ts"))
        kbps = _to_kbps(m.group("val"), m.group("unit"))
        raw[m.group("sid")].append((ts, kbps))
    for v in raw.values():
        v.sort(key=lambda x: x[0])
    return dict(raw)


# ---------------------------------------------------------------------------
# Statistics
# ---------------------------------------------------------------------------

def _percentile(lst: list[float], p: float) -> float:
    if not lst:
        return float("nan")
    idx = max(0, min(len(lst) - 1, int(len(lst) * p / 100.0)))
    return lst[idx]


def stream_stats(samples: list[tuple[float, float]]) -> dict:
    """Compute stability metrics for one stream's relative-time series."""
    vals = [kbps for _, kbps in samples]
    n = len(vals)
    mean = sum(vals) / n
    variance = sum((v - mean) ** 2 for v in vals) / n
    std = math.sqrt(variance)
    mn, mx = min(vals), max(vals)
    return {
        "n": n,
        "duration_s": samples[-1][0] - samples[0][0],
        "mean_kbps": mean,
        "std_kbps": std,
        "cv": std / mean if mean > 0 else float("inf"),        # coefficient of variation
        "min_kbps": mn,
        "max_kbps": mx,
        "ratio": mx / mn if mn > 0 else float("inf"),          # max/min
    }


def aggregate_stats(per_stream: dict[str, dict]) -> dict:
    cvs    = sorted(s["cv"]         for s in per_stream.values())
    ratios = sorted(s["ratio"]      for s in per_stream.values())
    means  = sorted(s["mean_kbps"]  for s in per_stream.values())
    stds   = sorted(s["std_kbps"]   for s in per_stream.values())

    def P(lst, p):
        return _percentile(lst, p)

    return {
        "n_streams": len(per_stream),
        "n_samples": sum(s["n"] for s in per_stream.values()),
        "cv":     {"p10": P(cvs, 10),   "p25": P(cvs, 25),   "p50": P(cvs, 50),
                   "p75": P(cvs, 75),   "p90": P(cvs, 90),   "p99": P(cvs, 99)},
        "ratio":  {"p10": P(ratios,10), "p25": P(ratios,25), "p50": P(ratios,50),
                   "p75": P(ratios,75), "p90": P(ratios,90), "p99": P(ratios,99)},
        "mean_kbps": {"p10": P(means,10), "p25": P(means,25), "p50": P(means,50),
                      "p75": P(means,75), "p90": P(means,90), "p99": P(means,99)},
        "std_kbps":  {"p10": P(stds,10),  "p25": P(stds,25),  "p50": P(stds,50),
                      "p75": P(stds,75),  "p90": P(stds,90),  "p99": P(stds,99)},
    }


# ---------------------------------------------------------------------------
# ASCII chart
# ---------------------------------------------------------------------------

def ascii_chart(sid: str, samples: list[tuple[float, float]], width: int = 72) -> str:
    """Render a horizontal time-series sparkline for one stream."""
    if len(samples) < 2:
        return ""
    times = [t for t, _ in samples]
    vals  = [v for _, v in samples]
    t_min, t_max = times[0], times[-1]
    v_min, v_max = min(vals), max(vals)
    v_range = v_max - v_min or 1.0
    t_range = t_max - t_min or 1.0

    bars = "▁▂▃▄▅▆▇█"
    buckets = [[] for _ in range(width)]
    for t, v in zip(times, vals):
        col = int((t - t_min) / t_range * (width - 1))
        buckets[col].append(v)

    row = ""
    for b in buckets:
        if b:
            avg = sum(b) / len(b)
            idx = int((avg - v_min) / v_range * (len(bars) - 1))
            row += bars[idx]
        else:
            row += " "

    return (
        f"  {sid[:28]:<28}  [{v_min:7.0f}–{v_max:7.0f} kbps]  "
        f"|{row}|  dur={t_range:.0f}s"
    )


# ---------------------------------------------------------------------------
# Output
# ---------------------------------------------------------------------------

def print_pct_row(label: str, d: dict) -> None:
    print(
        f"  {label:<10}  "
        f"p10={d['p10']:8.3f}  p25={d['p25']:8.3f}  p50={d['p50']:8.3f}  "
        f"p75={d['p75']:8.3f}  p90={d['p90']:8.3f}  p99={d['p99']:8.3f}"
    )


def print_report(
    stats: dict[str, dict],
    series: dict[str, list[tuple[float, float]]],
    agg: dict,
    chart: bool,
) -> None:
    print("=" * 72)
    print("  Upstream BWE Stability Analysis")
    print("=" * 72)
    print(f"  Streams  : {agg['n_streams']}")
    print(f"  Samples  : {agg['n_samples']}")
    print()

    print("  Coefficient of Variation = std/mean  [lower = more stable]")
    print_pct_row("CV", agg["cv"])
    print()

    print("  Max/Min ratio  [lower = flatter; 1.00 = perfectly constant]")
    print_pct_row("ratio", agg["ratio"])
    print()

    print("  Mean estimate (kbit/s)")
    print_pct_row("mean", agg["mean_kbps"])
    print()

    print("  Std-dev (kbit/s)")
    print_pct_row("std", agg["std_kbps"])
    print()

    # Worst 10 streams by CV
    print("  Top 10 most unstable streams (highest CV):")
    print(f"  {'stream_id':<32}  {'n':>5}  {'mean':>7}  {'std':>7}  "
          f"{'cv':>6}  {'ratio':>6}  {'dur_s':>6}")
    worst = sorted(stats.items(), key=lambda kv: kv[1]["cv"], reverse=True)[:10]
    for sid, s in worst:
        print(f"  {sid:<32}  {s['n']:5d}  {s['mean_kbps']:7.1f}  "
              f"{s['std_kbps']:7.1f}  {s['cv']:6.3f}  {s['ratio']:6.2f}x  "
              f"{s['duration_s']:6.0f}")
    print()

    # Best 10 streams by CV (among those with enough samples)
    stable_cands = [(sid, s) for sid, s in stats.items() if s["n"] >= 10]
    if stable_cands:
        print("  Top 10 most stable streams (lowest CV, min 10 samples):")
        print(f"  {'stream_id':<32}  {'n':>5}  {'mean':>7}  {'std':>7}  "
              f"{'cv':>6}  {'ratio':>6}  {'dur_s':>6}")
        best = sorted(stable_cands, key=lambda kv: kv[1]["cv"])[:10]
        for sid, s in best:
            print(f"  {sid:<32}  {s['n']:5d}  {s['mean_kbps']:7.1f}  "
                  f"{s['std_kbps']:7.1f}  {s['cv']:6.3f}  {s['ratio']:6.2f}x  "
                  f"{s['duration_s']:6.0f}")
        print()

    if chart:
        print("  ASCII timeline (▁–█ = low–high estimate per bucket):")
        for sid in sorted(series):
            print(ascii_chart(sid, series[sid]))
        print()

    print("=" * 72)


# ---------------------------------------------------------------------------
# CSV export
# ---------------------------------------------------------------------------

def write_csv(path: str, per_stream: dict[str, dict]) -> None:
    fields = ["stream_id", "n", "duration_s", "mean_kbps", "std_kbps",
              "cv", "min_kbps", "max_kbps", "ratio"]
    with open(path, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=fields)
        w.writeheader()
        for sid, s in sorted(per_stream.items()):
            w.writerow({"stream_id": sid, **{k: round(v, 4) for k, v in s.items()}})
    print(f"  Per-stream CSV written to: {path}")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> None:
    ap = argparse.ArgumentParser(
        description="Analyse upstream BWE log stability across all streams."
    )
    ap.add_argument("file", nargs="?", help="Log file (default: stdin)")
    ap.add_argument("--min-samples", type=int, default=5, metavar="N",
                    help="Minimum samples to include a stream (default 5)")
    ap.add_argument("--chart", action="store_true",
                    help="Print ASCII timeline charts per stream")
    ap.add_argument("--csv", metavar="FILE",
                    help="Write per-stream stats to CSV")
    args = ap.parse_args()

    src = open(args.file) if args.file else sys.stdin
    raw = parse_log(src)

    if not raw:
        print(
            "No 'upstream bwe' entries found.\n"
            "Run pulsebeam with: RUST_LOG=pulsebeam::rtp::monitor=debug"
        )
        sys.exit(1)

    # Build relative-time series and filter by min_samples
    per_stream: dict[str, list[tuple[float, float]]] = {}
    for sid, samples in raw.items():
        if len(samples) < args.min_samples:
            continue
        t0 = samples[0][0]
        per_stream[sid] = [(t - t0, kbps) for t, kbps in samples]

    if not per_stream:
        print(f"No streams with ≥{args.min_samples} samples.")
        sys.exit(1)

    stats = {sid: stream_stats(samples) for sid, samples in per_stream.items()}
    agg = aggregate_stats(stats)

    print_report(stats, per_stream, agg, chart=args.chart)

    if args.csv:
        write_csv(args.csv, stats)


if __name__ == "__main__":
    main()
