#!/usr/bin/env bash
# Private runtime entry point. Public callers execute bin/dot; only documented
# modules below lib/dot/public are source-compatible APIs.

set -euo pipefail
CDPATH=

# Git and init publication honor the process umask when materializing tracked
# worktree paths. Extension entry points must never become group/other writable,
# but retain a stricter caller policy such as 0077 instead of replacing it.
umask g-w,o-w

# BASHOPTS may be exported by the caller. Strict command and config tokens are
# byte-sensitive interfaces, so an inherited interactive convenience option
# must not make their case matching permissive.
shopt -u nocasematch

if [[ ${BASH_VERSINFO[0]} -lt 4 ]]; then
  printf 'dot: Bash 4 or newer is required\n' >&2
  exit 1
fi

DOT_SOURCE_ROOT=$(cd -P -- "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
export DOT_SOURCE_ROOT
# shellcheck source=temp.sh
. "$DOT_SOURCE_ROOT/lib/dot/temp.sh"
# shellcheck disable=SC2034 # Sourced provider and repository modules consume the original argv.
DOT_ORIGINAL_ARGV=("$@")
if [[ -n ${DOT_REEXEC_EXPECTED_REVISION:-} ]]; then
  _dot_reexec_observed=$(_dot_source_git rev-parse HEAD 2>/dev/null || true)
  if [[ $_dot_reexec_observed != "$DOT_REEXEC_EXPECTED_REVISION" ]]; then
    printf 'dot: re-exec revision mismatch: expected %s, found %s\n' \
      "$DOT_REEXEC_EXPECTED_REVISION" "${_dot_reexec_observed:-<missing>}" >&2
    exit 1
  fi
  unset _dot_reexec_observed
fi

# shellcheck disable=SC1091 # Runtime root is resolved above.
. "$DOT_SOURCE_ROOT/lib/dot/public/api-version.sh"
# shellcheck disable=SC1091 # Runtime root is resolved above.
. "$DOT_SOURCE_ROOT/lib/dot/public/xdg.sh"
# shellcheck disable=SC1091 # Runtime root is resolved above.
. "$DOT_SOURCE_ROOT/lib/dot/public/ui.sh"
# shellcheck disable=SC1091 # Runtime root is resolved above.
. "$DOT_SOURCE_ROOT/lib/dot/config.sh"

dot_version() {
  local revision=unknown

  if command -v git >/dev/null 2>&1; then
    revision=$(_dot_source_git rev-parse --short=12 HEAD 2>/dev/null) ||
      revision=unknown
  fi
  printf 'dot commit %s (config 1; extensions 1; library 1)\n' "$revision"
}

dot_help() {
  cat <<'EOF'
usage: dot <command> [<args>]

Commands:
  update           Converge the base repository, overlays, hooks, and provider
  pull             Alias for update
  fetch            Fetch the base repository and active Git overlays
  push             Push the base repository and active Git overlays
  status           Show base and overlay status
  diff             Show base and overlay differences
  cron             Show the installed user crontab
  doctor           Run core and configured extension health checks
  test             Run built-in and configured extension tests
  init             Initialize or resume a client dotfiles repository
  help             Show this command summary

Run `dot init --help` for initialization and recovery syntax.
EOF
}

case ${1:-help} in
  help | -h | --help)
    dot_help
    ;;
  --version | version)
    dot_version
    ;;
  *)
    dot_config_load || exit 2
    # shellcheck source=runtime.sh
    . "$DOT_SOURCE_ROOT/lib/dot/runtime.sh"
    # shellcheck source=commands.sh
    . "$DOT_SOURCE_ROOT/lib/dot/commands.sh"
    dot_command_dispatch "$@"
    ;;
esac
