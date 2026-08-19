#!/usr/bin/env bash
# One-process entry point for merge-hook and doctor-extension workers.
# Callers select the absolute Bash executable and sanitize its startup control
# environment in extension-worker-launch.sh; this file owns the stable runtime
# baseline seen by client code.

set -euo pipefail
CDPATH=
umask 077
shopt -u extglob nocasematch nullglob
trap - EXIT HUP INT QUIT TERM PIPE ALRM USR1 USR2 ERR DEBUG RETURN

_dot_extension_worker_discover_overlays() {
  local context=$TMPDIR/.dot-extension-overlays entry rc=0

  : >"$context" || return 1
  chmod 0600 "$context" || return 1
  (
    # Discovery needs platform predicates and descriptor parsing, but client
    # code does not. Keep those private functions in this short-lived process
    # and publish only the already-validated records as NUL-delimited data.
    # shellcheck source=public/xdg.sh
    . "$DOT_SOURCE_ROOT/lib/dot/public/xdg.sh"
    # shellcheck source=log.sh
    . "$DOT_SOURCE_ROOT/lib/dot/log.sh"
    # shellcheck source=platform.sh
    . "$DOT_SOURCE_ROOT/lib/dot/platform.sh"
    # shellcheck source=overlays.sh
    . "$DOT_SOURCE_ROOT/lib/dot/overlays.sh"
    _discover_overlays || exit 1
    for entry in "${OVERLAYS[@]+"${OVERLAYS[@]}"}"; do
      printf '%s\0' "$entry"
    done
  ) >"$context" || rc=$?
  [[ $rc -eq 0 ]] || {
    rm -f -- "$context"
    return "$rc"
  }

  OVERLAYS=()
  while IFS= read -r -d '' entry; do
    OVERLAYS+=("$entry")
  done <"$context"
  rm -f -- "$context"
}

_dot_extension_worker_load_overlay_protocol() {
  local function_name
  local -A existing_functions=()

  while IFS= read -r function_name; do
    existing_functions["$function_name"]=1
  done < <(compgen -A function)
  # shellcheck source=repos/config.sh
  . "$DOT_SOURCE_ROOT/lib/dot/repos/config.sh"
  # shellcheck source=repos/overlays.sh
  . "$DOT_SOURCE_ROOT/lib/dot/repos/overlays.sh"

  # Keep the canonical read-only authority/identity protocol and erase every
  # unrelated repository mutation helper before any client code is sourced.
  while IFS= read -r function_name; do
    [[ -n ${existing_functions[$function_name]+x} ]] && continue
    case $function_name in
      _overlay_link_target | _overlay_private_regular_file | \
        _overlay_parse_manifest_record | _overlay_manifest_safe | \
        _overlay_is_worktree | _overlay_effective_url | \
        _overlay_origin_matches | _overlay_checkout_matches)
        ;;
      *) unset -f "$function_name" ;;
    esac
  done < <(compgen -A function)
}

_dot_extension_worker_load_merge_api() {
  # shellcheck source=log.sh
  . "$DOT_SOURCE_ROOT/lib/dot/log.sh"
  # shellcheck source=temp.sh
  . "$DOT_SOURCE_ROOT/lib/dot/temp.sh"
  # shellcheck source=merge-block.sh
  . "$DOT_SOURCE_ROOT/lib/dot/merge-block.sh"
  # shellcheck source=families.sh
  . "$DOT_SOURCE_ROOT/lib/dot/families.sh"
  # shellcheck source=merge-hooks.sh
  . "$DOT_SOURCE_ROOT/lib/dot/merge-hooks.sh"
  # shellcheck source=hook-api.sh
  . "$DOT_SOURCE_ROOT/lib/dot/hook-api.sh"
}

_dot_extension_worker_load_doctor_api() {
  # shellcheck source=doctor-api.sh
  . "$DOT_SOURCE_ROOT/lib/dot/doctor-api.sh"
}

_dot_extension_worker_main() {
  local mode=${1:-} script=${2:-}
  local _dot_extension_worker_result=${3:-}

  [[ $# -eq 3 ]] || return 2
  readonly _dot_extension_worker_result
  case $mode in
    merge | pre-sync | doctor) ;;
    *) return 2 ;;
  esac
  case ${DOT_SOURCE_ROOT:-} in
    /*) ;;
    *) return 1 ;;
  esac
  [[ -d $DOT_SOURCE_ROOT/lib/dot && ! -L $DOT_SOURCE_ROOT/lib/dot ]] ||
    return 1
  [[ -n $_dot_extension_worker_result ]] || return 1
  cd "$HOME" || return 1

  # Clear the launcher's arguments before client code is sourced. Extension
  # entry points are intentionally zero-argument functions, and top-level
  # support code should observe the same empty positional-parameter baseline.
  set --
  # Load only the public resolver plus the small shared trust module. Overlay
  # discovery runs in its own process above, and mode-specific APIs are loaded
  # only after the entry script has passed revalidation.
  # shellcheck source=public/xdg.sh
  . "$DOT_SOURCE_ROOT/lib/dot/public/xdg.sh"
  dot_xdg_path state dot/overlay-links || return 1
  DOT_OVERLAY_MANIFEST=$REPLY
  export DOT_OVERLAY_MANIFEST
  _dot_extension_worker_load_overlay_protocol || return 1
  # shellcheck source=extension-trust.sh
  . "$DOT_SOURCE_ROOT/lib/dot/extension-trust.sh"
  _dot_extension_worker_discover_overlays || return 1
  _dot_extension_file_validate "$script" || return 1
  unset -f merge doctor 2>/dev/null || true

  # shellcheck disable=SC2031 # The readonly engine result path survives sourced client code.
  case $mode in
    merge | pre-sync)
      _dot_extension_worker_load_merge_api || return 1
      unset -f _dot_extension_worker_discover_overlays \
        _dot_extension_worker_load_overlay_protocol \
        _dot_extension_worker_load_merge_api \
        _dot_extension_worker_load_doctor_api \
        _dot_extension_worker_main 2>/dev/null || true
      # A source failure still identifies this as a discovered client hook. A
      # script that sources successfully but omits its lifecycle entry point is
      # malformed and retains the historical not-run classification.
      # shellcheck source=/dev/null
      if ! . "$script"; then
        printf '1' >"$_dot_extension_worker_result"
        return 1
      fi
      local entry_point=merge
      [[ $mode == pre-sync ]] && entry_point=prepare
      if ! declare -F "$entry_point" >/dev/null; then
        printf '0' >"$_dot_extension_worker_result"
        return 1
      fi
      printf '1' >"$_dot_extension_worker_result"
      "$entry_point"
      ;;
    doctor)
      _dot_extension_worker_load_doctor_api || return 1
      unset -f _dot_extension_worker_discover_overlays \
        _dot_extension_worker_load_overlay_protocol \
        _dot_extension_worker_load_merge_api \
        _dot_extension_worker_load_doctor_api \
        _dot_extension_worker_main 2>/dev/null || true
      DOT_DOCTOR_RESULT_FILE=$_dot_extension_worker_result
      readonly DOT_DOCTOR_RESULT_FILE
      export DOT_DOCTOR_RESULT_FILE
      # shellcheck source=/dev/null
      . "$script" || return 1
      declare -F doctor >/dev/null || return 1
      doctor
      ;;
  esac
}

_dot_extension_worker_main "$@"
