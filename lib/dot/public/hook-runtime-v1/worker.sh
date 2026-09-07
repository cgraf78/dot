#!/usr/bin/env bash

# shellcheck disable=SC1090,SC1091 # Runtime-root and trusted hook paths are dynamic.
# shellcheck disable=SC2034 # REPLY_* values are consumed by sourced public APIs.

# Rust validates and decodes the capability context before entering this
# interpreter boundary. This adapter exposes only the versioned shell API that
# user-authored hooks need, then invokes the selected public hook entry point.
set -euo pipefail

CDPATH=
umask 077
shopt -u extglob nocasematch nullglob
trap - EXIT HUP INT QUIT TERM PIPE ALRM USR1 USR2 ERR DEBUG RETURN

mode=$1
script=$2
result=$3
context=$4

exec 3<"$context"
rm -f -- "$context"
IFS= read -r -d '' stage <&3
IFS= read -r -d '' set_kind <&3
IFS= read -r -d '' retiring_name <&3
IFS= read -r -d '' retiring_root <&3
IFS= read -r -d '' record_count <&3
OVERLAYS=()
for ((record_index = 0; record_index < record_count; record_index++)); do
  IFS= read -r -d '' record <&3
  OVERLAYS+=("$record")
done
exec 3<&-

set --
REPLY_STAGE=$stage
REPLY_SET_KIND=$set_kind
cd "$HOME"

# shellcheck source=../xdg.sh
. "$DOT_SOURCE_ROOT/lib/dot/public/xdg.sh"
declare -A existing_functions=()
while IFS= read -r function_name; do
  existing_functions["$function_name"]=1
done < <(compgen -A function)
# shellcheck source=repos/config.sh
. "$DOT_SOURCE_ROOT/lib/dot/public/hook-runtime-v1/repos/config.sh"
# shellcheck source=repos/overlays.sh
. "$DOT_SOURCE_ROOT/lib/dot/public/hook-runtime-v1/repos/overlays.sh"
while IFS= read -r function_name; do
  [[ -n ${existing_functions[$function_name]+x} ]] && continue
  case $function_name in
    _overlay_link_target | _overlay_private_regular_file | \
      _overlay_parse_manifest_record | _overlay_manifest_safe | \
      _overlay_is_worktree | _overlay_effective_url | \
      _overlay_origin_matches | _overlay_checkout_matches) ;;
    *) unset -f "$function_name" ;;
  esac
done < <(compgen -A function)
unset existing_functions function_name
# shellcheck source=extension-trust.sh
. "$DOT_SOURCE_ROOT/lib/dot/public/hook-runtime-v1/extension-trust.sh"

if [[ $mode == deactivate ]]; then
  DOT_RETIRING_OVERLAY=$retiring_name
  DOT_RETIRING_OVERLAY_ROOT=$retiring_root
  readonly DOT_RETIRING_OVERLAY DOT_RETIRING_OVERLAY_ROOT
  export DOT_RETIRING_OVERLAY DOT_RETIRING_OVERLAY_ROOT
  OVERLAYS=()
fi
readonly -a OVERLAYS
if [[ $mode == pre-sync ]]; then
  DOT_PRE_SYNC_STAGE=$stage
  readonly DOT_PRE_SYNC_STAGE
  export DOT_PRE_SYNC_STAGE
fi

unset -f merge prepare deactivate doctor 2>/dev/null || true
case $mode in
  merge | pre-sync | deactivate)
    # shellcheck source=log.sh
    . "$DOT_SOURCE_ROOT/lib/dot/public/hook-runtime-v1/log.sh"
    # shellcheck source=temp.sh
    . "$DOT_SOURCE_ROOT/lib/dot/public/hook-runtime-v1/temp.sh"
    # shellcheck source=merge-block.sh
    . "$DOT_SOURCE_ROOT/lib/dot/public/hook-runtime-v1/merge-block.sh"
    # shellcheck source=families.sh
    . "$DOT_SOURCE_ROOT/lib/dot/public/hook-runtime-v1/families.sh"
    # shellcheck source=merge-hooks.sh
    . "$DOT_SOURCE_ROOT/lib/dot/public/hook-runtime-v1/merge-hooks.sh"
    # shellcheck source=hook-api.sh
    . "$DOT_SOURCE_ROOT/lib/dot/public/hook-runtime-v1/hook-api.sh"
    if ! . "$script"; then
      printf 1 >"$result"
      exit 1
    fi
    entry=merge
    [[ $mode == pre-sync ]] && entry=prepare
    [[ $mode == deactivate ]] && entry=deactivate
    if ! declare -F "$entry" >/dev/null; then
      printf 0 >"$result"
      exit 1
    fi
    printf 1 >"$result"
    "$entry"
    ;;
  doctor)
    # shellcheck source=doctor-api.sh
    . "$DOT_SOURCE_ROOT/lib/dot/public/hook-runtime-v1/doctor-api.sh"
    DOT_DOCTOR_RESULT_FILE=$result
    readonly DOT_DOCTOR_RESULT_FILE
    export DOT_DOCTOR_RESULT_FILE
    . "$script"
    declare -F doctor >/dev/null
    doctor
    ;;
esac
