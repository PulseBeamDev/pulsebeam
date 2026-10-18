#!/bin/sh
set -eu

root=$(git rev-parse --show-toplevel)
cd "$root"

current=$(git config --local --get core.hooksPath || :)
if [ -n "$current" ] && [ "$current" != ".githooks" ]; then
    printf 'Existing core.hooksPath=%s; refusing to overwrite it.\n' "$current" >&2
    printf 'Unconfigure that setting first if you want PulseBeam hooks.\n' >&2
    exit 1
fi

git config --local core.hooksPath .githooks
printf 'PulseBeam Git hooks enabled: .githooks\n'
