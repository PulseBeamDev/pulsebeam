#!/usr/bin/env bash
# Runs at Bazel's workspace boundary, never inside a simulation test action.
set -euo pipefail
revision=$(git rev-parse --verify HEAD)
dirty=false
status=$(git status --porcelain --untracked-files=all)
if [[ -n "$status" ]]; then dirty=true; fi
fingerprint=$(
  {
    printf '%s\0' "$revision"
    git -c core.quotePath=true diff --no-ext-diff --no-textconv --no-renames --binary --src-prefix=a/ --dst-prefix=b/ HEAD -- || exit 1
    git ls-files --others --exclude-standard -z | while IFS= read -r -d '' file; do
      printf '\0%s\0' "$file"
      if [[ -L "$file" ]]; then
        printf 'symlink\0'
        readlink -- "$file" || exit 1
      else
        stat --format='%a' -- "$file" || exit 1
        sha256sum -- "$file" || exit 1
      fi
    done || exit 1
  } | sha256sum
)
printf 'STABLE_SIM_REVISION %s\nSTABLE_SIM_DIRTY %s\nSTABLE_SIM_FINGERPRINT %s\n' "$revision" "$dirty" "${fingerprint%% *}"
