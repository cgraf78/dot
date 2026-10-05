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
# Snapshot the pre-existing functions so only the overlay allowlist below
# survives the library sources. Membership against the newline-joined
# snapshot needs one process listing and no per-item forks, and stays
# compatible with Bash 3.2, which ships no associative arrays. The name
# stays quoted inside the pattern so glob characters in it match
# literally; function names cannot contain newlines.
existing_functions=$(compgen -A function)
existing_functions=$'\n'"$existing_functions"$'\n'
# shellcheck source=repos/config.sh
. "$DOT_SOURCE_ROOT/lib/dot/public/hook-runtime-v1/repos/config.sh"
# shellcheck source=repos/overlays.sh
. "$DOT_SOURCE_ROOT/lib/dot/public/hook-runtime-v1/repos/overlays.sh"
while IFS= read -r function_name; do
  case $existing_functions in
    *$'\n'"$function_name"$'\n'*) continue ;;
  esac
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
    # Under `set -e` a failing command ends the extension with no output,
    # which used to surface as an empty failure row. The ERR trap notes the
    # last failure in this shell (errtrace carries it into functions); the
    # EXIT trap hands the note to the coordinator only when the worker
    # fails. An explicit `exit` other than the noted command means the note
    # is about a failure the extension tolerated under `set +e`, so it is
    # dropped; any other failing command, `return` included, overwrites the
    # note itself, and inside a sourced file or `eval` the exit-time command
    # is the enclosing one, so only `exit` can be compared. The note lives in
    # shell variables because subshells and command substitutions, where a
    # failing command does not stop the extension, must not leave one
    # behind. It names the innermost frame outside Dot's own runtime: a
    # helper that rejects its arguments fails inside the public API, but the
    # fix belongs on the extension line that called it. Reaching this file's
    # own top level means the extension file failed to load, or `doctor`
    # itself returned the status; only the phase and last command are known
    # then. The note path is the coordinator's
    # `doctor_orchestrator::failure_path`; deriving it from the readonly
    # result path keeps an extension's own `result` variable out of it.
    _dot_doctor_failure_file=$DOT_DOCTOR_RESULT_FILE.failure
    readonly _dot_doctor_failure_file
    _dot_doctor_phase=load
    _dot_doctor_failure=()
    _dot_doctor_note_failure() {
      local status=$1 command=$2 frame where='' root=${DOT_EXTENSIONS_DIR:-}
      local -a pipe=("${@:3}")
      # `${pipe[*]}` joins with IFS; the extension's own IFS must not leak.
      local IFS=' '
      root=${root%/}
      # Under `pipefail` a pipeline whose last element succeeded failed in an
      # earlier one, yet Bash names only the last element (and its line):
      # the earlier element ran in a subshell whose own ERR trap is gone.
      # Mark the pipeline and list each element's status rather than blame
      # a command that succeeded; the statuses lead so the coordinator's
      # length cap cannot cut them. Statuses that are all zero belong to an
      # earlier pipeline (a redirection on a group can fail after it), and
      # `[[` and `((` leave the previous pipeline's statuses in place, so
      # neither counts. A public helper's name below still wins.
      if ((${#pipe[@]} > 1)) && [[ ${pipe[${#pipe[@]} - 1]} == 0 &&
        " ${pipe[*]} " == *[1-9]* && $command != '[['* &&
        $command != '(('* ]]; then
        command="pipeline with statuses ${pipe[*]}, ending in: $command"
      fi
      for ((frame = 1; frame < ${#BASH_SOURCE[@]}; frame++)); do
        case ${BASH_SOURCE[frame]} in
          "$0") break ;;
          "$DOT_SOURCE_ROOT"/lib/dot/*) command=${FUNCNAME[frame]} ;;
          *)
            where=${BASH_SOURCE[frame]}
            [[ -z $root ]] || where=${where#"$root"/}
            where+=:${BASH_LINENO[frame - 1]}
            break
            ;;
        esac
      done
      # The worker's own `. "$script"` line says nothing about the cause.
      [[ -n $where || $_dot_doctor_phase != load ]] || command=
      _dot_doctor_failure=("$status" "$_dot_doctor_phase" "$where" "$command" "$2")
    }
    _dot_doctor_report_failure() {
      [[ $1 -ne 0 && ${#_dot_doctor_failure[@]} -eq 5 ]] || return 0
      case $2 in
        exit | 'exit '*) [[ $2 == "${_dot_doctor_failure[4]}" ]] || return 0 ;;
      esac
      printf '%s\0%s\0%s\0%s' "${_dot_doctor_failure[@]:0:4}" \
        >"$_dot_doctor_failure_file" 2>/dev/null || :
    }
    trap '_dot_doctor_note_failure "$?" "$BASH_COMMAND" "${PIPESTATUS[@]}"' ERR
    trap '_dot_doctor_report_failure "$?" "$BASH_COMMAND"' EXIT
    set -E
    . "$script"
    if ! declare -F doctor >/dev/null; then
      printf 'dot: %s defines no doctor function\n' "${script##*/}" >&2
      exit 1
    fi
    _dot_doctor_phase=run
    doctor
    ;;
esac
