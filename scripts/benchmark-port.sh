#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf 'usage: %s SHELL_DOT RUST_DOT\n' "${0##*/}" >&2
  exit 2
}

[[ $# -eq 2 ]] || usage
shell_dot=$1
rust_dot=$2

[[ -x $shell_dot && -x $rust_dot ]] || {
  printf 'error: both dot paths must be executable\n' >&2
  exit 2
}
shell_dot=$(cd "$(dirname "$shell_dot")" && pwd -P)/$(basename "$shell_dot")
rust_dot=$(cd "$(dirname "$rust_dot")" && pwd -P)/$(basename "$rust_dot")
shell_root=$(cd "$(dirname "$shell_dot")/.." && pwd -P)
[[ -f $shell_root/lib/dot/main.sh ]] || {
  printf 'error: SHELL_DOT must be the bin/dot entry in a pre-cutover checkout\n' >&2
  exit 2
}
command -v hyperfine >/dev/null 2>&1 || {
  printf 'error: hyperfine is required\n' >&2
  exit 2
}

# The paths are arguments, never interpolated into a command string.  This
# keeps the measurement independent of PATH and avoids benchmarking a launcher
# or checkout different from the one the caller selected.
hyperfine --warmup 10 --runs 50 --shell=none \
  --command-name 'shell help' "$shell_dot help" \
  --command-name 'rust help' "$rust_dot help" \
  --command-name 'shell version' "$shell_dot version" \
  --command-name 'rust version' "$rust_dot version"

printf '\nHistorical Bash full-update benchmark:\n'
DOT_PERF_EXECUTABLE=$shell_dot \
  DOT_PERF_SOURCE_ROOT=$shell_root \
  DOT_PERF_BUDGET_MULTIPLIER=10 \
  cargo test --release --locked --test perf_update -- --ignored --nocapture

printf '\nNative Rust full-update benchmark:\n'
DOT_PERF_EXECUTABLE=$rust_dot \
  DOT_PERF_SOURCE_ROOT=$PWD \
  DOT_PERF_BUDGET_MULTIPLIER=1 \
  cargo test --release --locked --test perf_update -- --ignored --nocapture
