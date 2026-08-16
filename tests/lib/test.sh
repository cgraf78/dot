#!/usr/bin/env bash

set -euo pipefail

DOT_TEST_ROOT=$(cd -P -- "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
DOT_TEST_TMP=$(mktemp -d)
DOT_TEST_TMP=$(cd -P -- "$DOT_TEST_TMP" && pwd -P)
export DOT_TEST_ROOT DOT_TEST_TMP
trap 'rm -rf "$DOT_TEST_TMP"' EXIT HUP INT TERM

# Public-engine tests own their complete synthetic HOME/XDG topology. CI
# runners may export host-specific XDG roots, which would otherwise make a
# fixture read configuration or state outside its isolated HOME and produce
# platform-dependent path identities.
unset XDG_CONFIG_HOME XDG_STATE_HOME XDG_CACHE_HOME XDG_DATA_HOME XDG_RUNTIME_DIR

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

assert_files_equal() {
  local expected=$1 actual=$2 label=$3 expected_hash actual_hash

  expected_hash=$(git hash-object --no-filters -- "$expected") ||
    fail "$label: could not hash expected file"
  actual_hash=$(git hash-object --no-filters -- "$actual") ||
    fail "$label: could not hash actual file"
  if [[ $actual_hash == "$expected_hash" ]]; then
    return 0
  fi
  git diff --no-index --no-ext-diff --no-textconv -- \
    "$expected" "$actual" >&2 || true
  fail "$label"
}

process_is_live() {
  local pid=$1 line rest

  kill -0 "$pid" 2>/dev/null || return 1
  # A container PID 1 may leave an already-terminated orphan as a zombie.
  # `kill -0` still succeeds for that inert record, while the production
  # cleanup contract consistently treats procfs state Z as no longer live.
  if [[ -r /proc/$pid/stat ]] &&
    IFS= read -r line 2>/dev/null <"/proc/$pid/stat"; then
    rest=${line##*) }
    [[ $rest == "$line" || ${rest%% *} != Z ]] || return 1
  fi
  return 0
}
