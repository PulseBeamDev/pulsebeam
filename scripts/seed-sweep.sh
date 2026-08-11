#!/usr/bin/env bash
#
# Run the simulation suite over a range of seeds.
#
# The suite's value is that it is a set of property tests over a simulated
# network. A property that has only ever been checked at one seed is not a
# property: it is one sample of one ordering of arrivals, losses and
# reorderings. This sweeps the rest of that space while keeping every failure
# exactly reproducible, because the seed is the whole input.
#
# Each iteration is an independent `nextest` run, so a seed that hangs or
# aborts the process cannot take the sweep with it. Failures are collected and
# reported at the end as replay commands rather than stopping the run — one bad
# seed should not hide the other nineteen.
#
# Two things decide how many seeds an hour buys.
#
# The filter. Most of the suite is not seed-sensitive: the data-channel and signalling plans
# finish in milliseconds and assert the same thing under every seed, and CI already runs them at
# the committed one. Filtering to the plans where a seed changes the answer barely moves wall time
# per seed - the longest plan is itself seed-sensitive and stays - but it cuts the CPU each seed
# needs, which is what lets seeds overlap.
#
# The overlap, which turned out not to help. Seeds are independent, so running several at once
# looks like a free win - but a single seed already runs 57 plans across every core, so lanes only
# add contention. Measured on a 20-core machine: three lanes took 293s per seed against 234s
# sequential. `LANES` is left available for a machine big enough that one seed cannot fill it;
# the default is one because on this one it is a loss.
#
#   SEEDS   how many seeds to run          (default 20)
#   FROM    first seed                     (default 1)
#   TEST    nextest filter                 (default: the seed-sensitive plans)
#   ALL=1   sweep every plan instead
#   JOBS    nextest threads per iteration  (default: nextest's own choice)
#   LANES   how many seeds to run at once   (default 3)
set -uo pipefail

cd "$(dirname "$0")/.."

SEEDS="${SEEDS:-20}"
FROM="${FROM:-1}"
# `-E` expression rather than a substring filter, so it is one argument and quotes cleanly.
if [[ -n "${ALL:-}" ]]; then
    TEST="${TEST:-}"
else
    TEST="${TEST:--E test(/bwe::|properties::|video::/)}"
fi
LANES="${LANES:-1}"
# Split the machine between lanes rather than letting each lane assume it owns it. Three lanes
# each spawning one test per core is heavy oversubscription, and the seeds finish slower together
# than they would one at a time.
if [[ -z "${JOBS:-}" ]]; then
    cores="$(nproc 2>/dev/null || echo 4)"
    JOBS=$(( cores / LANES ))
    (( JOBS < 1 )) && JOBS=1
fi

jobs_arg=()
[[ -n "$JOBS" ]] && jobs_arg=(--test-threads "$JOBS")

log_dir="$(mktemp -d)"
failed_seeds=()
last=$((FROM + SEEDS - 1))

echo "sweeping seeds ${FROM}..${last} (${SEEDS} runs)${TEST:+, filter: ${TEST}}"
echo "logs: ${log_dir}"
echo

# Build once so the first iteration's compile time is not attributed to it, and
# so a compile error fails now rather than twenty times.
if ! cargo nextest run --cargo-profile sim -p pulsebeam-simulator \
        --no-run >"${log_dir}/build.log" 2>&1; then
    echo "build failed:"
    tail -40 "${log_dir}/build.log"
    exit 1
fi

run_seed() {
    local seed="$1"
    if PULSEBEAM_SIM_SEED="$seed" cargo nextest run --cargo-profile sim \
            -p pulsebeam-simulator --no-fail-fast "${jobs_arg[@]}" $TEST \
            >"${log_dir}/seed-${seed}.log" 2>&1; then
        echo "ok" >"${log_dir}/seed-${seed}.status"
    else
        echo "FAILED" >"${log_dir}/seed-${seed}.status"
    fi
}

started=$(date +%s)
running=0
for seed in $(seq "$FROM" "$last"); do
    run_seed "$seed" &
    running=$((running + 1))
    if (( running >= LANES )); then
        wait -n 2>/dev/null || true
        running=$((running - 1))
    fi
done
wait

for seed in $(seq "$FROM" "$last"); do
    status="$(cat "${log_dir}/seed-${seed}.status" 2>/dev/null || echo FAILED)"
    printf 'seed %-12s %s\n' "$seed" "$status"
    [[ "$status" == "ok" ]] || failed_seeds+=("$seed")
done
elapsed=$(( $(date +%s) - started ))

echo
echo "swept ${SEEDS} seeds in ${elapsed}s"

if [[ ${#failed_seeds[@]} -eq 0 ]]; then
    echo "no failures"
    exit 0
fi

echo
echo "${#failed_seeds[@]} of ${SEEDS} seeds failed:"
for seed in "${failed_seeds[@]}"; do
    echo
    echo "  seed ${seed} — replay with:"
    echo "      make test-sim-seed SEED=${seed}${TEST:+ TEST='${TEST}'}"
    grep -E '^\s+(FAIL|TRY [0-9]+ FAIL)' "${log_dir}/seed-${seed}.log" \
        | sed 's/^/        /' | sort -u | head -20
done
echo
echo "full logs in ${log_dir}"
exit 1
