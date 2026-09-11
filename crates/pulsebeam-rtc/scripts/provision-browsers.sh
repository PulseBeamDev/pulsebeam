#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 [--platform linux-x86_64] [--verify-only]" >&2
  exit 2
}

platform="linux-x86_64"
verify_only=false
while (($#)); do
  case "$1" in
    --platform)
      (($# >= 2)) || usage
      platform="$2"
      shift 2
      ;;
    --verify-only)
      verify_only=true
      shift
      ;;
    *) usage ;;
  esac
done

[[ "$platform" == "linux-x86_64" ]] || {
  echo "unsupported browser platform: $platform" >&2
  exit 2
}

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
crate_dir="$(cd "$script_dir/.." && pwd)"
workspace="$(cd "$crate_dir/../.." && pwd)"
matrix="$crate_dir/browser/browser-matrix.json"
cache_root="$workspace/target/pulsebeam-rtc-browsers"
cache="$cache_root/$platform"
downloads="$cache_root/downloads"

mapfile -t artifacts < <(python3 - "$matrix" "$platform" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    matrix = json.load(source)
try:
    artifacts = matrix["platforms"][sys.argv[2]]["artifacts"]
except KeyError as error:
    raise SystemExit(f"matrix has no platform {sys.argv[2]!r}: {error}")
for artifact in artifacts:
    fields = ("name", "version", "url", "bytes", "sha256", "archive", "executable", "probe")
    print("\t".join(str(artifact[field]) for field in fields))
PY
)

verify_archive() {
  local archive_path="$1" expected_bytes="$2" expected_sha="$3" name="$4"
  [[ -f "$archive_path" ]] || {
    echo "$name archive is missing: $archive_path" >&2
    return 1
  }
  local actual_bytes actual_sha
  actual_bytes="$(wc -c < "$archive_path")"
  actual_sha="$(sha256sum "$archive_path" | awk '{print $1}')"
  [[ "$actual_bytes" == "$expected_bytes" ]] || {
    echo "$name archive length mismatch: expected $expected_bytes, got $actual_bytes" >&2
    return 1
  }
  [[ "$actual_sha" == "$expected_sha" ]] || {
    echo "$name archive SHA-256 mismatch: expected $expected_sha, got $actual_sha" >&2
    return 1
  }
}

verify_install() {
  local name="$1" executable="$2" probe="$3"
  local stable="$cache/bin/$name"
  [[ -x "$stable" ]] || {
    echo "$name executable is missing: $stable" >&2
    return 1
  }
  local actual
  actual="$("$stable" --version 2>&1 | head -n 1 | sed 's/[[:space:]]*$//')"
  [[ "$actual" == "$probe"* ]] || {
    echo "$name version mismatch: expected '$probe', got '$actual'" >&2
    return 1
  }
  [[ "$(readlink -f "$stable")" == "$(readlink -f "$cache/artifacts/$name/$executable")" ]] || {
    echo "$name stable link does not point at the matrix executable" >&2
    return 1
  }
}

if [[ "$verify_only" == false ]]; then
  mkdir -p "$downloads" "$cache/artifacts" "$cache/bin"
fi

for row in "${artifacts[@]}"; do
  IFS=$'\t' read -r name version url bytes sha archive_kind executable probe <<< "$row"
  filename="${url##*/}"
  archive_path="$downloads/$filename"
  if [[ "$verify_only" == false ]] && ! verify_archive "$archive_path" "$bytes" "$sha" "$name" 2>/dev/null; then
    partial="$archive_path.partial.$$"
    trap 'rm -f "${partial:-}"; rm -rf "${staging:-}"' EXIT
    echo "downloading $name $version"
    curl --fail --location --retry 3 --output "$partial" "$url"
    verify_archive "$partial" "$bytes" "$sha" "$name"
    mv -f "$partial" "$archive_path"
  fi
  verify_archive "$archive_path" "$bytes" "$sha" "$name"

  if [[ "$verify_only" == false ]]; then
    staging="$cache/artifacts/$name.tmp.$$"
    rm -rf "$staging"
    mkdir -p "$staging"
    case "$archive_kind" in
      zip) unzip -q "$archive_path" -d "$staging" ;;
      tar.xz) tar -xJf "$archive_path" -C "$staging" ;;
      tar.gz) tar -xzf "$archive_path" -C "$staging" ;;
      *) echo "unsupported archive type for $name: $archive_kind" >&2; exit 2 ;;
    esac
    [[ -x "$staging/$executable" ]] || {
      echo "$name archive did not contain executable $executable" >&2
      exit 1
    }
    rm -rf "$cache/artifacts/$name"
    mv "$staging" "$cache/artifacts/$name"
    link="$cache/bin/$name.tmp.$$"
    ln -s "../artifacts/$name/$executable" "$link"
    mv -Tf "$link" "$cache/bin/$name"
  fi
  verify_install "$name" "$executable" "$probe"
done

echo "verified browser matrix $platform in $cache"
