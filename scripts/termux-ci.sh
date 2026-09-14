#!/data/data/com.termux/files/usr/bin/bash
set -euo pipefail

# The standard matrix owns the full test suite. This job executes the
# NDK-built Android binary inside the real Termux app sandbox and
# verifies the slice-1 CLI contract (help text, version shape, unknown
# command) on-device. Deeper suites arrive with their owning slices.
binary=.termux-ci/dot

[[ -x $binary ]] || {
  printf 'termux-ci: transported binary missing: %s\n' "$binary" >&2
  exit 1
}

help_expected='usage: dot <command> [<args>]'
help_actual=$("$binary" help)
[[ $help_actual == "$help_expected"* ]] || {
  printf 'termux-ci: unexpected help output: %s\n' "$help_actual" >&2
  exit 1
}

version_actual=$("$binary" version)
case $version_actual in
  'dot commit '*'(config 1; extensions 1; library 1)') ;;
  *)
    printf 'termux-ci: unexpected version output: %s\n' "$version_actual" >&2
    exit 1
    ;;
esac

if "$binary" frobnicate 2>/dev/null; then
  printf 'termux-ci: unknown command unexpectedly succeeded\n' >&2
  exit 1
fi

printf 'termux-ci: ok\n'
