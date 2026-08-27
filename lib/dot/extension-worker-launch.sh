# shellcheck shell=bash
# Launch one extension in a fresh copy of the already-selected Bash runtime.
#
# The client environment remains available because merge policy legitimately
# consumes platform and tool variables (for example WSL and credential hints).
# Shell-control inputs are different: BASH_ENV, exported functions, and
# inherited option variables can execute code or alter parsing before the
# extension entry point is validated. Remove only that control plane, then let
# the worker establish the documented option/trap baseline itself.

if ! declare -F _dot_overlay_context_create >/dev/null 2>&1; then
  # shellcheck source=overlay-context.sh
  . "${BASH_SOURCE[0]%/*}/overlay-context.sh"
fi

_dot_extension_worker_exec() {
  local mode=${1:-} script=${2:-} temporary=${3:-} channel=${4:-}
  local context=${5:-} token=${6:-}
  local bash_path=${BASH:-} function_name kind temporary_mode

  [[ $# -eq 6 ]] || return 2
  case $mode in
    merge | pre-sync | doctor) ;;
    *)
      rm -f -- "$context" 2>/dev/null || true
      return 2
      ;;
  esac
  case $bash_path in
    /*) [[ -x $bash_path ]] || {
      rm -f -- "$context" 2>/dev/null || true
      return 1
    } ;;
    *)
      rm -f -- "$context" 2>/dev/null || true
      return 1
      ;;
  esac
  [[ -d $temporary && ! -L $temporary && -O $temporary ]] || {
    rm -f -- "$context" 2>/dev/null || true
    return 1
  }
  temporary_mode=$(command stat -c '%a' "$temporary" 2>/dev/null ||
    command stat -f '%Lp' "$temporary" 2>/dev/null) || {
    rm -f -- "$context" 2>/dev/null || true
    return 1
  }
  [[ $temporary_mode != *[!0-7]* ]] || {
    rm -f -- "$context" 2>/dev/null || true
    return 1
  }
  (((8#$temporary_mode & 077) == 0)) || {
    rm -f -- "$context" 2>/dev/null || true
    return 1
  }
  [[ -n $channel && -n $context && -n $token ]] || {
    rm -f -- "$context" 2>/dev/null || true
    return 1
  }

  # Exported Bash functions are encoded as environment records during exec.
  # Removing their export attribute in this launcher subshell preserves the
  # coordinator while ensuring a hook cannot inherit or shadow engine helpers.
  while IFS= read -r function_name; do
    export -n -f "${function_name?}" 2>/dev/null || true
  done < <(compgen -A function)

  # These variables affect noninteractive Bash before the worker script gets
  # control. Ordinary client variables intentionally remain available.
  unset BASH_ENV ENV CDPATH GLOBIGNORE BASH_COMPAT POSIXLY_CORRECT BASH_XTRACEFD
  export -n BASHOPTS SHELLOPTS

  TMPDIR=$temporary
  export TMPDIR
  for kind in config state cache data; do
    dot_xdg_home "$kind" || return 1
    case $kind in
      config)
        XDG_CONFIG_HOME=$REPLY
        export XDG_CONFIG_HOME
        ;;
      state)
        XDG_STATE_HOME=$REPLY
        export XDG_STATE_HOME
        ;;
      cache)
        XDG_CACHE_HOME=$REPLY
        export XDG_CACHE_HOME
        ;;
      data)
        XDG_DATA_HOME=$REPLY
        export XDG_DATA_HOME
        ;;
    esac
  done

  exec </dev/null
  exec "$bash_path" --noprofile --norc \
    "$DOT_SOURCE_ROOT/lib/dot/extension-worker.sh" \
    "$mode" "$script" "$channel" "$context" "$token"
}

# Synchronous callers must survive the worker's exec so they can record status
# and elapsed time. Asynchronous cleanup owners should instead background
# _dot_extension_worker_exec directly; then `$!` is the exact Bash worker PID.
_dot_extension_worker_run() (
  _dot_extension_worker_exec "$@"
)
