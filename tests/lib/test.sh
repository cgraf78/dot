#!/usr/bin/env bash

set -euo pipefail

DOT_TEST_ROOT=$(cd -P -- "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
DOT_TEST_TMP=$(mktemp -d)
export DOT_TEST_ROOT DOT_TEST_TMP
trap 'rm -rf "$DOT_TEST_TMP"' EXIT HUP INT TERM

# Public-engine fixtures must not inherit a client repository's PATH-visible
# Git launcher. That launcher intentionally interprets synthetic HOME values as
# dotfiles work trees and would make isolated generic tests exercise private
# client policy. Keep the first ordinary Git directory ahead of the caller's
# PATH; individual tests can still prepend their own fakes.
while IFS= read -r dot_test_git; do
  [[ -x $dot_test_git ]] || continue
  [[ $dot_test_git != "${HOME:-}/.local/bin/git" ]] || continue
  PATH=${dot_test_git%/*}:$PATH
  export PATH
  break
done < <(type -a -p git 2>/dev/null || true)
unset dot_test_git

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
