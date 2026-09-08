#!/data/data/com.termux/files/usr/bin/bash
set -euo pipefail

# The standard matrix owns the full test suite. This job executes the
# NDK-built Android binary inside the real Termux app sandbox and
# verifies the transported release's startup contract: help, version, and
# fail-closed operational-command handling.
binary=.termux-ci/dot
metadata=.termux-ci/.dot-install.json

[[ -x $binary ]] || {
  printf 'termux-ci: transported binary missing: %s\n' "$binary" >&2
  exit 1
}
[[ -f $metadata ]] || {
  printf 'termux-ci: transported release metadata missing: %s\n' "$metadata" >&2
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

set +e
unknown_output=$("$binary" frobnicate 2>&1)
unknown_status=$?
set -e
if [[ $unknown_status -ne 1 ]]; then
  printf 'termux-ci: unknown command unexpectedly succeeded\n' >&2
  exit 1
fi
[[ $unknown_output == 'dot: startup: cannot resolve source root from executable' ]] || {
  printf 'termux-ci: unexpected operational failure: %s\n' "$unknown_output" >&2
  exit 1
}

printf 'termux-ci: ok\n'
