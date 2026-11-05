#!/usr/bin/env python3
"""Summarize NativeLink 1.8.3 worker execution tracing records.

This is a measurement helper, not a scheduler or backend. Fingerprints cover
only the JSON argument vector, not the action key, environment, inputs, or
working directory. NativeLink's command_directory includes an operation-
specific sandbox path, so it is deliberately excluded from the fingerprint.

The observed peak requires complete lifecycle records with accurate timestamps
to prove actual concurrency. Missing records can distort it in either direction.
Operation IDs must be unique across the entire input, including any replay or
process restart. Equal timestamps are ordered starts before completions, giving
an upper bound for that timestamp's unresolved ordering. Ambiguous or malformed
records are reported, rather than silently presented as complete evidence.
"""

import argparse
from collections import Counter, defaultdict
from datetime import datetime
import hashlib
import json
from pathlib import Path
import re


_ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
_HEADER = re.compile(r"(?m)^\s*([^\s]+)\s+(?:TRACE|DEBUG|INFO|WARN|ERROR)\s+[^\n]*")
_TIMESTAMP = re.compile(r"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)(?:\.(\d{1,9}))?Z\Z")
_OPERATION = re.compile(r'\boperation_id\s*:\s*"([^"\n]+)"')
_ARGS = re.compile(r"\bExecuting command,\s*args\s*:\s*")
_DIRECTORY = re.compile(r"\s*,\s*command_directory\s*:")
_EXIT = re.compile(r"\bexit_code\s*:\s*(-?\d+)\b")


def _timestamp(value):
    match = _TIMESTAMP.fullmatch(value)
    if not match:
        return None
    try:
        second = datetime.strptime(match.group(1), "%Y-%m-%dT%H:%M:%S")
    except ValueError:
        return None
    return second, (match.group(2) or "").ljust(9, "0")


def _fingerprint(header):
    match = _ARGS.search(header)
    if not match:
        return None
    try:
        args, end = json.JSONDecoder().raw_decode(header[match.end():])
    except ValueError:
        return None
    if (not isinstance(args, list) or not all(isinstance(arg, str) for arg in args)
            or not _DIRECTORY.match(header[match.end() + end:])):
        return None
    canonical = json.dumps(args, ensure_ascii=True, separators=(",", ":"))
    return hashlib.sha256(canonical.encode("ascii")).hexdigest()


def analyze(text):
    """Return deterministic JSON-compatible counts, diagnostics and fingerprints."""
    text = _ANSI.sub("", text)
    headers = list(_HEADER.finditer(text))
    events = []
    counts = Counter()
    fingerprints = defaultdict(list)
    for index, header in enumerate(headers):
        line = header.group(0)
        start = "Executing command" in line
        complete = "Command complete" in line
        if not start and not complete:
            continue
        kind = "start" if start else "completion"
        counts[kind + "s"] += 1
        end = headers[index + 1].start() if index + 1 < len(headers) else len(text)
        record = text[header.end():end]
        ids = set(_OPERATION.findall(record))
        operation = next(iter(ids)) if len(ids) == 1 else None
        timestamp = _timestamp(header.group(1))
        if operation is None:
            counts["missing_or_ambiguous_operation_ids"] += 1
        if timestamp is None:
            counts["invalid_timestamps"] += 1
        if start:
            fingerprint = _fingerprint(line)
            if fingerprint is None:
                counts["invalid_command_arguments"] += 1
            else:
                fingerprints[fingerprint].append(operation)
        else:
            match = _EXIT.search(line)
            if match is None:
                counts["unknown_exit_codes"] += 1
            elif int(match.group(1)) != 0:
                counts["nonzero_exits"] += 1
        if timestamp is not None and operation is not None:
            events.append((timestamp, 0 if start else 1, index, operation))

    active = set()
    seen_starts = set()
    seen_completions = set()
    unmatched_completions = []
    repeated_starts = []
    repeated_completions = []
    peak = 0
    for _, kind, _, operation in sorted(events):
        if kind == 0:
            if operation in seen_starts:
                repeated_starts.append(operation)
                continue
            seen_starts.add(operation)
            active.add(operation)
            peak = max(peak, len(active))
        else:
            if operation in seen_completions:
                repeated_completions.append(operation)
                continue
            seen_completions.add(operation)
            if operation in active:
                active.remove(operation)
            else:
                unmatched_completions.append(operation)

    duplicates = [
        {"sha256": fingerprint, "count": len(operations),
         "operation_ids": sorted(op for op in operations if op is not None)}
        for fingerprint, operations in sorted(fingerprints.items()) if len(operations) > 1
    ]
    return {
        "starts": counts["starts"],
        "completions": counts["completions"],
        "peak_simultaneous_executing": peak,
        "nonzero_exits": counts["nonzero_exits"],
        "incomplete_operation_ids": sorted(active),
        "unmatched_completion_operation_ids": sorted(unmatched_completions),
        "duplicate_command_fingerprints": duplicates,
        "diagnostics": {
            "missing_or_ambiguous_operation_ids": counts["missing_or_ambiguous_operation_ids"],
            "invalid_timestamps": counts["invalid_timestamps"],
            "invalid_command_arguments": counts["invalid_command_arguments"],
            "unknown_exit_codes": counts["unknown_exit_codes"],
            "repeated_start_operation_ids": sorted(repeated_starts),
            "repeated_completion_operation_ids": sorted(repeated_completions),
        },
        "limits": [
            "Peak requires complete lifecycle records and accurate timestamps; equal timestamps are ordered starts first.",
            "Operation IDs must be unique across the input, including replays and process restarts.",
            "Command fingerprints cover argument vectors only; matching fingerprints do not establish action key equality.",
            "This is a measurement helper, not a scheduler or backend.",
        ],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log_path", type=Path)
    args = parser.parse_args()
    print(json.dumps(analyze(args.log_path.read_text(encoding="utf-8")), indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
