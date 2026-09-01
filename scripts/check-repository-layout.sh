#!/usr/bin/env bash
set -euo pipefail

failed=0

while IFS= read -r directory; do
    if [[ ! -s "$directory/README.md" ]]; then
        echo "missing module README: $directory/README.md" >&2
        failed=1
    fi
done < <(find agents crates -mindepth 1 -maxdepth 1 -type d | sort)

if [[ -d docs ]]; then
    echo "root docs/ must not exist; documentation belongs to a module" >&2
    failed=1
fi

while IFS= read -r task_file; do
    echo "legacy task file: $task_file" >&2
    failed=1
done < <(find . -path ./target -prune -o -type f \( -iname makefile -o -name justfile \) -print)

if rg -n --hidden \
    'make (lint-check|test-unit|test-sim-seed|test-sim|test-browser|test-routing|build-ebpf|sim-sweep|bwe-baseline|test|lint|release)' \
    --glob '!target/**' --glob '!*.lock' --glob '!LICENSE' .; then
    echo "legacy Make command reference found" >&2
    failed=1
fi

exit "$failed"
