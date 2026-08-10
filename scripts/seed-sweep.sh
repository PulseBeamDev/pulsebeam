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
#   SEEDS   how many seeds to run          (default 20)
#   FROM    first seed                     (default 1)
#   TEST    nextest filter, e.g. properties:: (default: everything)
#   JOBS    nextest threads per iteration  (default: nextest's own choice)
set -uo pipefail

cd "$(dirname "$0")/.."

SEEDS="${SEEDS:-20}"
FROM="${FROM:-1}"
TEST="${TEST:-}"
JOBS="${JOBS:-}"

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

started=$(date +%s)
for seed in $(seq "$FROM" "$last"); do
    printf 'seed %-12s ' "$seed"
    if PULSEBEAM_SIM_SEED="$seed" cargo nextest run --cargo-profile sim \
            -p pulsebeam-simulator --no-fail-fast "${jobs_arg[@]}" $TEST \
            >"${log_dir}/seed-${seed}.log" 2>&1; then
        echo "ok"
    else
        echo "FAILED"
        failed_seeds+=("$seed")
    fi
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
