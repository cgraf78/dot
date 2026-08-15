# shellcheck shell=bash
# Process-wide `dot update` serialization.
#
# `mkdir` is atomic on the local filesystems supported by macOS, Linux, and
# WSL. Unlike a PID-only lock, the owner record also carries the process start
# identity, so a reused PID cannot keep a crashed update locked forever.

if ! declare -F _dot_xdg_path >/dev/null 2>&1; then
  # shellcheck source=xdg.sh disable=SC1091
  . "${BASH_SOURCE[0]%/*}/xdg.sh"
fi
if ! declare -F _dot_cleanup_all >/dev/null 2>&1; then
  # shellcheck source=resources.sh disable=SC1091
  . "${BASH_SOURCE[0]%/*}/resources.sh"
fi

_dot_update_lock_path() {
  _dot_xdg_path state "dot/update.lock.d"
}

_dot_update_lock_owner_file() {
  printf '%s/owner\n' "$1"
}

_dot_update_lock_process_start() {
  # `ps` provides the only portable process-birth identifier available to the
  # supported shell environments. Treat it as an opaque OS identity token and
  # force a stable locale/time zone so cron and interactive callers agree.
  LC_ALL=C TZ=UTC0 ps -o lstart= -p "$1" 2>/dev/null
}

_dot_update_lock_mtime() {
  stat -c '%Y' "$1" 2>/dev/null || stat -f '%m' "$1" 2>/dev/null
}

_dot_update_lock_now() {
  date +%s
}

_dot_update_lock_read_owner() {
  local lock_dir="$1" owner_file key value
  owner_file="$(_dot_update_lock_owner_file "$lock_dir")"
  [[ -f "$owner_file" && ! -L "$owner_file" ]] || return 1

  DOT_UPDATE_LOCK_OWNER_PID=""
  DOT_UPDATE_LOCK_OWNER_START=""
  DOT_UPDATE_LOCK_OWNER_TOKEN=""
  while IFS=$'\t' read -r key value; do
    case "$key" in
      pid) DOT_UPDATE_LOCK_OWNER_PID="$value" ;;
      start) DOT_UPDATE_LOCK_OWNER_START="$value" ;;
      token) DOT_UPDATE_LOCK_OWNER_TOKEN="$value" ;;
    esac
  done <"$owner_file"

  [[ "$DOT_UPDATE_LOCK_OWNER_PID" =~ ^[0-9]+$ ]] || return 1
  [[ -n "$DOT_UPDATE_LOCK_OWNER_START" && -n "$DOT_UPDATE_LOCK_OWNER_TOKEN" ]]
}

_dot_update_lock_owner_is_active() {
  local start
  kill -0 "$DOT_UPDATE_LOCK_OWNER_PID" 2>/dev/null || return 1
  start="$(_dot_update_lock_process_start "$DOT_UPDATE_LOCK_OWNER_PID")" || return 1
  [[ "$start" == "$DOT_UPDATE_LOCK_OWNER_START" ]]
}

_dot_update_lock_is_initializing() {
  local lock_dir="$1" mtime now
  mtime="$(_dot_update_lock_mtime "$lock_dir")" || return 1
  now="$(_dot_update_lock_now)" || return 1
  ((now - mtime < 5))
}

_dot_update_lock_remove_dir() {
  local lock_dir="$1" owner_file
  owner_file="$(_dot_update_lock_owner_file "$lock_dir")"
  rm -f "$owner_file"
  rmdir "$lock_dir" 2>/dev/null
}

_dot_update_lock_claim() {
  local source="$1" claim
  [[ -f "$source" && ! -L "$source" ]] || return 1
  claim="${source}.reclaim.$$.${RANDOM}"
  mv "$source" "$claim" 2>/dev/null || return 1
  REPLY="$claim"
}

_dot_update_lock_find_claim() {
  local lock_dir="$1" claim
  for claim in "$lock_dir"/owner.reclaim.*; do
    [[ -f "$claim" && ! -L "$claim" ]] || continue
    REPLY="$claim"
    return 0
  done
  return 1
}

_dot_update_lock_reclaim_empty() {
  local lock_dir="$1" guard_dir
  guard_dir="$lock_dir/reclaim.d"

  # A crash can happen after mkdir but before the owner write. An inner
  # directory gives exactly one cleaner the right to remove that otherwise
  # empty lock. If a cleaner dies, its empty guard is itself safely reclaimable.
  if [[ -d "$guard_dir" && ! -L "$guard_dir" ]]; then
    _dot_update_lock_is_initializing "$guard_dir" && return 1
    rmdir "$guard_dir" 2>/dev/null || return 1
  elif [[ -e "$guard_dir" || -L "$guard_dir" ]]; then
    return 1
  elif ! mkdir "$guard_dir" 2>/dev/null; then
    return 1
  else
    rmdir "$guard_dir" 2>/dev/null || return 1
  fi

  rmdir "$lock_dir" 2>/dev/null
}

_dot_update_lock_reclaim_stale() {
  local lock_dir="$1" owner_file claim
  owner_file="$(_dot_update_lock_owner_file "$lock_dir")"

  # Renaming the owner file, rather than the directory, is an atomic stale
  # claim. A second contender cannot remove a directory after the first one
  # has recreated it because only this claimant still owns the moved file.
  if [[ -f "$owner_file" && ! -L "$owner_file" ]]; then
    _dot_update_lock_claim "$owner_file" || return 1
    claim="$REPLY"
  elif _dot_update_lock_find_claim "$lock_dir"; then
    _dot_update_lock_claim "$REPLY" || return 1
    claim="$REPLY"
  else
    _dot_update_lock_reclaim_empty "$lock_dir"
    return
  fi

  [[ -f "$claim" && ! -L "$claim" ]] || return 1
  rm "$claim" || return 1
  rmdir "$lock_dir" 2>/dev/null
}

_dot_update_lock_write_owner() {
  local lock_dir="$1" owner_file start token
  owner_file="$(_dot_update_lock_owner_file "$lock_dir")"
  start="$(_dot_update_lock_process_start "$$")" || return 1
  token="$$.${SECONDS}.${RANDOM}"

  (
    umask 077
    {
      printf 'pid\t%s\n' "$$"
      printf 'start\t%s\n' "$start"
      printf 'token\t%s\n' "$token"
    } >"$owner_file"
  ) || return 1
  chmod 600 "$owner_file" 2>/dev/null || true
  DOT_UPDATE_LOCK_TOKEN="$token"
  export DOT_UPDATE_LOCK_TOKEN
}

_dot_update_lock_release() {
  local lock_dir
  _dot_update_lock_path || return 0
  lock_dir="$REPLY"
  [[ -n "${DOT_UPDATE_LOCK_TOKEN:-}" ]] || return 0
  _dot_update_lock_read_owner "$lock_dir" || return 0
  [[ "$DOT_UPDATE_LOCK_OWNER_PID" == "$$" ]] || return 0
  [[ "$DOT_UPDATE_LOCK_OWNER_TOKEN" == "$DOT_UPDATE_LOCK_TOKEN" ]] || return 0

  _dot_update_lock_remove_dir "$lock_dir" ||
    _warn "  warning: unable to remove dot update lock state: $lock_dir"
  unset DOT_UPDATE_LOCK_TOKEN
}

_dot_update_lock_install_traps() {
  # Lock release must run even if an unexpected cleanup command fails under
  # errexit; leaving the lock behind would turn a cleanup defect into a second
  # persistent failure on every later update.
  # Ignore handled signals before cleanup starts. `_dot_cleanup_all` preserves
  # that disposition, so lock-release subprocesses remain protected too.
  trap '_dot_cleanup_on_exit; _dot_update_lock_release' EXIT
  _dot_cleanup_install_signal_traps
}

_dot_update_lock_reenter() {
  local lock_dir="$1" start
  [[ -n "${DOT_UPDATE_LOCK_TOKEN:-}" ]] || return 1
  _dot_update_lock_read_owner "$lock_dir" || return 1
  [[ "$DOT_UPDATE_LOCK_OWNER_PID" == "$$" ]] || return 1
  [[ "$DOT_UPDATE_LOCK_OWNER_TOKEN" == "$DOT_UPDATE_LOCK_TOKEN" ]] || return 1
  start="$(_dot_update_lock_process_start "$$")" || return 1
  [[ "$start" == "$DOT_UPDATE_LOCK_OWNER_START" ]] || return 1
  _dot_update_lock_install_traps
}

# Acquire the process-wide update lock. Returns 75 when another valid owner is
# active; callers use DOT_UPDATE_LOCK_CRON_MODE to choose quiet cron behavior.
_dot_update_lock_acquire() {
  local arg lock_dir state_dir attempt=0
  DOT_UPDATE_LOCK_CRON_MODE=0
  for arg in "$@"; do
    [[ "$arg" == "--cron" ]] && DOT_UPDATE_LOCK_CRON_MODE=1
  done

  _dot_update_lock_path || return 1
  lock_dir="$REPLY"
  state_dir="${lock_dir%/*}"
  mkdir -p "$state_dir" || return 1

  if _dot_update_lock_reenter "$lock_dir"; then
    return 0
  fi

  while ((attempt < 3)); do
    if mkdir "$lock_dir" 2>/dev/null; then
      chmod 700 "$lock_dir" 2>/dev/null || true
      if _dot_update_lock_write_owner "$lock_dir"; then
        _dot_update_lock_install_traps
        return 0
      fi
      _dot_update_lock_remove_dir "$lock_dir" || true
      return 1
    fi

    [[ -d "$lock_dir" && ! -L "$lock_dir" ]] || {
      _warn "  warning: dot update lock path is not a directory: $lock_dir"
      return 1
    }

    if _dot_update_lock_read_owner "$lock_dir" && _dot_update_lock_owner_is_active; then
      if [[ "$DOT_UPDATE_LOCK_CRON_MODE" -eq 0 ]]; then
        _warn "  warning: dot update already running (pid $DOT_UPDATE_LOCK_OWNER_PID)"
      fi
      return 75
    fi

    if ! _dot_update_lock_read_owner "$lock_dir" && _dot_update_lock_is_initializing "$lock_dir"; then
      if [[ "$DOT_UPDATE_LOCK_CRON_MODE" -eq 0 ]]; then
        _warn "  warning: dot update lock is initializing"
      fi
      return 75
    fi

    _dot_update_lock_reclaim_stale "$lock_dir" || return 75
    attempt=$((attempt + 1))
  done

  return 75
}
