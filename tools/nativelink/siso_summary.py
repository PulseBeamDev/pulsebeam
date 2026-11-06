#!/usr/bin/env python3
"""Summarize Siso INFO remote execution records from a log file or stdin.

Records are correlated by UUID within one client log. Digest and metadata lines
must precede completion; later lines are reported as incomplete, not joined to a
past completion. UUIDs are assumed unique within the input. State is discarded
at completion, so memory depends on outstanding or incomplete UUIDs, not total
completed actions. Equal digests do not establish in-flight deduplication.
"""

import argparse
from decimal import Decimal, DecimalException
import json
from pathlib import Path
import re
import sys


_UUID = r"[0-9a-fA-F]{8}-(?:[0-9a-fA-F]{4}-){3}[0-9a-fA-F]{12}"
_SOURCE = re.compile(r"remoteexec\.go:(96|164|177)\]")
_RECORD = re.compile(rf"\s+({_UUID})\s+(.*)")
_COMPLETION = re.compile(r"exit=(-?\d+)\s+cache=(true|false)(?=\s|,|$)")
_DIGEST = re.compile(r"digest:\s+([0-9a-fA-F]{64})/(\d+)(?=\s|,|$)")
_QUEUE = re.compile(r"(?:^|\s)queue=(\S+)")
_DURATION_PART = re.compile(r"(\d+(?:\.\d+)?|\.\d+)(ns|us|µs|μs|ms|s|m|h)")
_UNIT_NS = {"ns": 1, "us": 1000, "µs": 1000, "μs": 1000,
            "ms": 1000000, "s": 1000000000, "m": 60000000000,
            "h": 3600000000000}


def _duration_ns(value):
    """Parse nonnegative Go duration text into integer nanoseconds."""
    if value == "0":
        return 0
    pos = 0
    total = Decimal(0)
    try:
        for part in _DURATION_PART.finditer(value):
            if part.start() != pos:
                return None
            total += Decimal(part.group(1)) * _UNIT_NS[part.group(2)]
            pos = part.end()
        if pos != len(value) or not pos or total != total.to_integral_value():
            return None
        # Go time.Duration is a signed 64-bit nanosecond count.
        if total > 2**63 - 1:
            return None
        return int(total)
    except (DecimalException, ValueError, OverflowError):
        return None


def summarize(lines):
    """Consume an iterable of log lines and return JSON-serializable counts."""
    pending = {}
    completed = {"total": 0, "backend_cache_hits": 0, "backend_cache_misses": 0,
                 "nonzero_exits": 0, "with_metadata": 0, "without_metadata": 0,
                 "with_digest": 0, "without_digest": 0}
    diagnostics = {"malformed_digest_records": 0, "malformed_metadata_records": 0,
                   "malformed_completion_records": 0, "invalid_queue_durations": 0,
                   "duplicate_digest_records": 0, "duplicate_metadata_records": 0}
    queues = {"count": 0, "total_ns": 0, "min_ns": None, "max_ns": None}

    for line in lines:
        source = _SOURCE.search(line)
        if not source:
            continue
        kind = source.group(1)
        record = _RECORD.fullmatch(line[source.end():].rstrip("\r\n"))
        label = {"96": "digest", "164": "metadata", "177": "completion"}[kind]
        if not record:
            diagnostics[f"malformed_{label}_records"] += 1
            continue
        uuid, body = record.groups()
        uuid = uuid.lower()
        if kind == "177":
            match = _COMPLETION.match(body)
            if not match:
                diagnostics["malformed_completion_records"] += 1
                continue
            try:
                exit_code = int(match.group(1))
            except ValueError:
                diagnostics["malformed_completion_records"] += 1
                continue
            state = pending.pop(uuid, {})
            completed["total"] += 1
            completed["backend_cache_hits" if match.group(2) == "true" else "backend_cache_misses"] += 1
            completed["nonzero_exits"] += exit_code != 0
            for field in ("metadata", "digest"):
                completed[("with_" if field in state else "without_") + field] += 1
            queue = state.get("metadata")
            if queue is not None:
                queues["count"] += 1
                queues["total_ns"] += queue
                queues["min_ns"] = queue if queues["min_ns"] is None else min(queues["min_ns"], queue)
                queues["max_ns"] = queue if queues["max_ns"] is None else max(queues["max_ns"], queue)
        elif kind == "96":
            if not _DIGEST.match(body):
                diagnostics["malformed_digest_records"] += 1
                continue
            state = pending.setdefault(uuid, {})
            if "digest" in state:
                diagnostics["duplicate_digest_records"] += 1
            else:
                state["digest"] = True
        else:
            if not body.startswith("execution metadata:"):
                diagnostics["malformed_metadata_records"] += 1
                continue
            state = pending.setdefault(uuid, {})
            if "metadata" in state:
                diagnostics["duplicate_metadata_records"] += 1
                continue
            values = _QUEUE.findall(body)
            queue = _duration_ns(values[0]) if len(values) == 1 else None
            if queue is None:
                diagnostics["invalid_queue_durations"] += 1
            state["metadata"] = queue

    incomplete = {"uuids": len(pending),
                  "with_metadata": sum("metadata" in state for state in pending.values()),
                  "with_digest": sum("digest" in state for state in pending.values())}
    queues["mean_ns"] = queues["total_ns"] / queues["count"] if queues["count"] else None
    return {"completed": completed, "queue_duration": queues,
            "incomplete_without_completion": incomplete, "diagnostics": diagnostics}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log_path", help="Siso INFO log path, or - for stdin")
    args = parser.parse_args()
    if args.log_path == "-":
        result = summarize(sys.stdin)
    else:
        with Path(args.log_path).open(encoding="utf-8", errors="replace") as lines:
            result = summarize(lines)
    print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
