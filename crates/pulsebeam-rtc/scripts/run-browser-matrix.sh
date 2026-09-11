#!/usr/bin/env bash
set -euo pipefail

platform=""
include_root_tests=false
while (($#)); do
  case "$1" in
    --platform)
      (($# >= 2)) || exit 2
      platform="$2"
      shift 2
      ;;
    --include-root-tests)
      include_root_tests=true
      shift
      ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[[ "$platform" == "linux-x86_64" ]] || {
  echo "--platform linux-x86_64 is required" >&2
  exit 2
}
[[ "$include_root_tests" == true ]] || {
  echo "--include-root-tests is required for the acceptance aggregate" >&2
  exit 2
}

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace="$(cd "$script_dir/../../.." && pwd)"
cache="$workspace/target/pulsebeam-rtc-browsers/$platform"

"$script_dir/provision-browsers.sh" --platform "$platform"
"$script_dir/provision-browsers.sh" --platform "$platform" --verify-only

cd "$workspace"
PULSEBEAM_RTC_BROWSER=chrome \
PULSEBEAM_BROWSER_BINARY="$cache/bin/chrome" \
PULSEBEAM_WEBDRIVER_BINARY="$cache/bin/chromedriver" \
  cargo test -p pulsebeam-rtc --features browser-tests --test browser_interop -- --ignored --exact chrome_matrix
PULSEBEAM_RTC_BROWSER=firefox \
PULSEBEAM_BROWSER_BINARY="$cache/bin/firefox" \
PULSEBEAM_WEBDRIVER_BINARY="$cache/bin/geckodriver" \
  cargo test -p pulsebeam-rtc --features browser-tests --test browser_interop -- --ignored --exact firefox_matrix
PULSEBEAM_BROWSER_BINARY="$cache/bin/chrome" just test
