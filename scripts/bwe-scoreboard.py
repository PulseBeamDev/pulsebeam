#!/usr/bin/env python3
"""Turn a `just test` run into a stable, diffable scoreboard.

Reads nextest output on stdin and writes one block per plan: the plan's name, its outcome, and
the measured behaviour of every participant's link.

The point is the diff. A congestion-control change rarely improves everything - it trades one
scenario against another - and a pass/fail summary hides that entirely. Comparing this file
before and after shows the whole matrix at once, so "helps screenshare, wrecks cellular" is one
line of diff rather than a discovery made days later.

This is only meaningful because the simulation is deterministic (see sim_clock.rs / sim_rand.rs).
Without that the file would churn on every run and the diff would carry no signal.

A failing plan records only its outcome. The scoreboard is emitted at the end of a plan and a
failed assertion panics before reaching it, so there are no metrics to report. The failure itself
prints its own context, so nothing is lost at the point of diagnosis - but it does mean a plan
which starts failing loses its numbers from the baseline, and the diff shows that as a block
shrinking rather than as values moving.
"""

import re
import sys

ANSI = re.compile(r"\x1b\[[0-9;]*m")
SCOREBOARD = re.compile(r"\[scoreboard\] (.*)")
RESULT = re.compile(r"^\s+(PASS|FAIL|TRY \d+ FAIL) \[.*?\] \(\s*\d+/\d+\) \S+ (\S+)")

# Randomised plans are excluded. They draw a fresh seed each run, so their numbers differ every
# time by design - that is the point of them. Including them would make the baseline churn on
# every run and drown the signal from the fixed matrix. Anything they find is captured in
# proptest's own regressions file instead, and re-runs from there.
EXPLORATORY = re.compile(r"tests::properties::")

# Volatile fields: real durations that vary with host speed even when the simulation itself is
# reproducible. Kept out of the baseline so the diff shows behaviour, not machine noise.
ELAPSED = re.compile(r"\d+\.\d+(µs|ms|ns|s)")


def normalise(metrics: str) -> str:
    return ELAPSED.sub(lambda m: f"<{m.group(1)}>", metrics)


def main() -> int:
    pending: list[str] = []
    blocks: dict[str, list[str]] = {}

    for raw in sys.stdin:
        line = ANSI.sub("", raw)
        if m := SCOREBOARD.search(line):
            pending.append(m.group(1).strip())
        elif m := RESULT.match(line):
            outcome, name = m.group(1), m.group(2)
            if EXPLORATORY.search(name):
                pending = []
                continue
            # nextest reports each result twice: once as it happens, once in the summary.
            blocks.setdefault(name, [f"  [{outcome}]"] + [f"  {p}" for p in pending])
            pending = []

    if not blocks:
        print("no plans found - was this the output of `just test`?", file=sys.stderr)
        return 1

    for name in sorted(blocks):
        print(name)
        for line in blocks[name]:
            print(normalise(line))
        print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
