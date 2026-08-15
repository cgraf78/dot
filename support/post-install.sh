#!/usr/bin/env bash
# The shared checkout lock is released before this phase. Initialization may
# bootstrap Shdeps, so it must never inherit the non-reentrant checkout lock.

set -euo pipefail
CDPATH=

ROOT=$(cd -P -- "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
case ${1:-} in
  '') exit 0 ;;
  --init)
    shift
    exec "$ROOT/bin/dot" init "$@"
    ;;
  *)
    printf 'usage: install.sh [--managed] [--init [dot-init-args...]]\n' >&2
    exit 2
    ;;
esac
