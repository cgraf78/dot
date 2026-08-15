#!/usr/bin/env bash
# Private runtime entry point. Public callers execute bin/dot; only documented
# modules below lib/dot/public are source-compatible APIs.

set -euo pipefail
CDPATH=

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

# shellcheck disable=SC1091 # Runtime root is resolved above.
. "$DOT_SOURCE_ROOT/lib/dot/public/api-version.sh"
# shellcheck disable=SC1091 # Runtime root is resolved above.
. "$DOT_SOURCE_ROOT/lib/dot/public/xdg.sh"
# shellcheck disable=SC1091 # Runtime root is resolved above.
. "$DOT_SOURCE_ROOT/lib/dot/public/ui.sh"
# shellcheck disable=SC1091 # Runtime root is resolved above.
. "$DOT_SOURCE_ROOT/lib/dot/config.sh"

dot_version() {
  local version revision=unknown

  IFS= read -r version <"$DOT_SOURCE_ROOT/VERSION" || version=unknown
  if [[ -d "$DOT_SOURCE_ROOT/.git" ]] && command -v git >/dev/null 2>&1; then
    revision=$(git -C "$DOT_SOURCE_ROOT" rev-parse --short=12 HEAD 2>/dev/null) ||
      revision=unknown
  fi
  printf 'dot %s (source %s; config 1; extensions 1; library 1)\n' \
    "$version" "$revision"
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
    if [[ -f "$DOT_SOURCE_ROOT/lib/dot/commands.sh" ]]; then
      # shellcheck source=/dev/null
      . "$DOT_SOURCE_ROOT/lib/dot/commands.sh"
      dot_command_dispatch "$@"
    else
      printf 'dot: command is not available in this development build: %s\n' "$1" >&2
      exit 1
    fi
    ;;
esac
