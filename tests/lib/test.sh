#!/usr/bin/env bash

set -euo pipefail

DOT_TEST_ROOT=$(cd -P -- "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
DOT_TEST_TMP=$(mktemp -d)
export DOT_TEST_ROOT DOT_TEST_TMP
trap 'rm -rf "$DOT_TEST_TMP"' EXIT HUP INT TERM

fail() {
  printf 'test: %s\n' "$*" >&2
  exit 1
}

assert_eq() {
  local expected=$1 actual=$2 label=$3
  [[ "$actual" == "$expected" ]] ||
    fail "$label: expected [$expected], got [$actual]"
}

assert_contains() {
  local needle=$1 haystack=$2 label=$3
  [[ "$haystack" == *"$needle"* ]] ||
    fail "$label: missing [$needle]"
}
