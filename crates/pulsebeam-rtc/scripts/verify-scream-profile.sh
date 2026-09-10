#!/bin/sh
set -eu

url='https://www.ietf.org/archive/id/draft-ietf-ccwg-rfc8298bis-screamv2-01.txt'
expected_bytes='100016'
expected_sha256='7beb2ff371106fa81a1678607ab3a1eb439ac9b0843d0d7fb64dab671132667f'
artifact="$(mktemp)"
trap 'rm -f "$artifact"' EXIT HUP INT TERM

curl --fail --location --silent --show-error "$url" --output "$artifact"

actual_bytes="$(wc -c < "$artifact" | tr -d ' ')"
actual_sha256="$(sha256sum "$artifact" | cut -d ' ' -f 1)"

if [ "$actual_bytes" != "$expected_bytes" ]; then
    echo "SCReAM profile byte length mismatch: expected $expected_bytes, got $actual_bytes" >&2
    exit 1
fi
if [ "$actual_sha256" != "$expected_sha256" ]; then
    echo "SCReAM profile SHA-256 mismatch: expected $expected_sha256, got $actual_sha256" >&2
    exit 1
fi

echo "verified draft-ietf-ccwg-rfc8298bis-screamv2-01.txt ($actual_bytes bytes, $actual_sha256)"
