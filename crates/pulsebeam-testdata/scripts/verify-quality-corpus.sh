#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root/src"

while IFS=, read -r hash_field length_field file_field; do
    expected_hash="${hash_field#file=}"
    expected_length="${length_field#length=}"
    file="${file_field#name=}"
    test -n "$file"
    test "$(sha256sum "$file" | awk '{print $1}')" = "$expected_hash"
    test "$(wc -c < "$file")" -eq "$expected_length"
done < <(grep '^file=' quality-corpus.manifest)
