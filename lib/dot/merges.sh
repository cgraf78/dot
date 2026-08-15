# shellcheck shell=bash
# Run versioned client merge-hook extensions. Each worker sources one hook,
# requires a zero-argument merge function, and cannot leak shell state back to
# the coordinator or the next hook.

if ! declare -F _dot_extension_worker_run >/dev/null 2>&1; then
  # shellcheck source=extension-worker-launch.sh
  . "${BASH_SOURCE[0]%/*}/extension-worker-launch.sh"
fi

_merge_trim() {
  local _line="$1"
  _line="${_line#"${_line%%[![:space:]]*}"}"
  _line="${_line%"${_line##*[![:space:]]}"}"
  printf '%s' "$_line"
}

_merge_label_from_script() {
  local _base="${1##*/}"
  _base="${_base%.sh}"
  _base="${_base%.serial}"
  if [[ "$_base" =~ ^[0-9]+[-_](.+)$ ]]; then
    _base="${BASH_REMATCH[1]}"
  fi
  printf '%s' "$_base"
}

_merge_progress_detail() {
  local _done="$1" _total="$2" _label="$3"
  # Merge hook filenames already define the durable hook identity. Keep progress
  # generic here so adding a hook never requires teaching dot about product
  # display names or abbreviations in a second place.
  _ui_progress_detail_with_label "$_label" "$_done" "$_total"
}

# Print ordered merge hook specs as `<sort-key><tab><script-path>`.
_merge_hook_specs() {
  local _key _identity _script _root
  local -A _seen=()

  [[ "${DOT_EXTENSION_API:-}" == 1 && -n "${DOT_EXTENSIONS_DIR:-}" ]] ||
    return 0
  if ! _dot_extension_root_validate; then
    printf 'dot: unsafe extension root: %s\n' "$DOT_EXTENSIONS_DIR" >&2
    return 1
  fi
  _root=$DOT_EXTENSIONS_DIR/merge-hooks.d
  if [[ ! -e $_root && ! -L $_root ]]; then
    return 0
  fi
  if ! _dot_extension_directory_validate "$_root"; then
    printf 'dot: unsafe merge-hook directory: %s\n' "$_root" >&2
    return 1
  fi

  {
    shopt -s nullglob
    for _script in "$_root"/*.sh; do
      _dot_extension_file_validate "$_script" || {
        printf 'dot: unsafe merge hook: %s\n' "$_script" >&2
        return 1
      }
      _key="${_script##*/}"
      _key="${_key%.sh}"
      _key="${_key%.serial}"
      _identity=$_key
      [[ "$_identity" =~ ^([0-9]+[-_])?([a-z][a-z0-9-]*)$ ]] || {
        printf 'dot: invalid merge-hook identity: %s\n' "${_script##*/}" >&2
        return 1
      }
      _identity=${BASH_REMATCH[2]}
      [[ -z ${_seen[$_identity]+x} ]] || {
        printf 'dot: duplicate merge-hook identity: %s\n' "$_identity" >&2
        return 1
      }
      _seen[$_identity]=1
      printf '%s\t%s\n' "$_key" "$_script"
    done
    shopt -u nullglob
  } | LC_ALL=C sort
}

_merge_hook_is_serial() {
  [[ "$2" == *.serial.sh ]]
}

_merge_parallel_jobs() {
  local _jobs="${DOT_MERGE_JOBS:-}"
  case "$_jobs" in
    '' | *[!0-9]*) _jobs="$(_dot_update_jobs)" ;;
  esac
  [[ "$_jobs" -lt 1 ]] && _jobs=1
  printf '%s\n' "$_jobs"
}

_merge_summary() {
  local _count="$1"
  if [[ "$_count" -eq 1 ]]; then
    printf '1 config merged'
  else
    printf '%s configs merged' "$_count"
  fi
}

_merge_failure_summary() {
  local _count="$1"
  if [[ "$_count" -eq 1 ]]; then
    printf '1 config hook failed'
  else
    printf '%s config hooks failed' "$_count"
  fi
}

_merge_warning_summary() {
  local _total="$1" _failed="$2"
  local _succeeded=$((_total - _failed))
  printf '%s, %s' \
    "$(_merge_summary "$_succeeded")" \
    "$(_merge_failure_summary "$_failed")"
}

_print_merge_result() {
  local _script="$1" _elapsed_ms="$2" _log="$3" _status="${4:-ok}"
  local _line _label=""
  local -a _details=()

  # Verbose hooks own their first output line as the display label. That keeps
  # the runner generic while still allowing hooks to show a friendlier target
  # name when the filename stem is too mechanical.
  while IFS= read -r _line || [[ -n "$_line" ]]; do
    _line="$(_merge_trim "$_line")"
    [[ -n "$_line" ]] || continue
    if [[ -z "$_label" ]]; then
      _label="$_line"
    else
      _details+=("$_line")
    fi
  done <"$_log"

  [[ -n "$_label" ]] || _label="$(_merge_label_from_script "$_script")"
  _ui_item "$_status" "$_label" "$(_ui_duration_ms "$_elapsed_ms")"
  for _line in "${_details[@]}"; do
    _ui_detail "$_line"
  done
}

_merge_result_prefix() {
  local _dir="$1" _idx="$2"
  printf '%s/%03d' "$_dir" "$_idx"
}

_run_merge_hook_capture() {
  local _idx="$1" _script="$2" _result_dir="$3"
  local _prefix _started_ms _elapsed_ms _merge_rc=0 _hook_pid _worker_tmp
  _dot_cleanup_prepare_subshell
  _prefix="$(_merge_result_prefix "$_result_dir" "$_idx")"
  _worker_tmp="$_result_dir/worker.$_idx"
  _started_ms="$(_ui_now_ms)"

  # Result records stay in the coordinator-owned batch root; extension scratch
  # gets a distinct private directory. This prevents accidental filename
  # collisions between parallel hooks without pretending same-user code is a
  # security sandbox.
  if ! (umask 077 && mkdir "$_worker_tmp"); then
    printf 'dot: cannot create private merge-hook TMPDIR\n' >"$_prefix.log"
    _merge_rc=1
  elif [[ "${DOT_CLEANUP_INHERIT_GROUP:-0}" == 1 ]]; then
    # The outer coordinator already owns this capture process and its entire
    # group. Running the hook here makes the process that records readiness the
    # exact child the coordinator can wait and reap. A nested supervisor cannot
    # reap its hook after the outer leader is stopped as the non-reusable PGID
    # anchor, which left orphan zombies on container runners with a passive PID
    # 1. Interactive capture workers still use the nested path below so a
    # parent-only signal can cancel work outside a private group.
    _dot_extension_worker_run merge "$_script" "$_worker_tmp" \
      "$_prefix.has_merge" >"$_prefix.log" 2>&1 || _merge_rc=$?
  else
    _dot_cleanup_begin_job_launch
    _dot_extension_worker_exec merge "$_script" "$_worker_tmp" \
      "$_prefix.has_merge" <&"$DOT_CLEANUP_LAUNCH_STDIN_FD" \
      >"$_prefix.log" 2>&1 &
    _hook_pid=$!
    _dot_cleanup_finish_job_launch "$_hook_pid"
    if wait "$_hook_pid"; then
      _merge_rc=0
    else
      _merge_rc=$?
    fi
    _dot_cleanup_unregister_pid "$_hook_pid"
  fi

  _elapsed_ms=$(($(_ui_now_ms) - _started_ms))
  printf '%s' "$_merge_rc" >"$_prefix.rc"
  printf '%s' "$_elapsed_ms" >"$_prefix.elapsed_ms"
}

_print_merge_capture() {
  local _hook_key="$1" _idx="$2" _result_dir="$3"
  local _prefix _has_merge _merge_rc _elapsed_ms _merge_status=ok
  _prefix="$(_merge_result_prefix "$_result_dir" "$_idx")"
  # The coordinator owns these single-scalar files. Bash's direct file form
  # preserves command-substitution newline handling and the existing fallback
  # values without starting three `cat` processes for every completed hook.
  _has_merge=0
  { _has_merge="$(<"$_prefix.has_merge")"; } 2>/dev/null || _has_merge=0
  [[ "$_has_merge" -eq 1 ]] || return 1

  _merge_rc=1
  { _merge_rc="$(<"$_prefix.rc")"; } 2>/dev/null || _merge_rc=1
  _elapsed_ms=0
  { _elapsed_ms="$(<"$_prefix.elapsed_ms")"; } 2>/dev/null || _elapsed_ms=0
  [[ "$_merge_rc" -eq 0 ]] || _merge_status=warning

  if [[ "${DOT_VERBOSE:-0}" -eq 1 && "${DOT_QUIET:-0}" -ne 1 ]]; then
    _print_merge_result "$_hook_key" "$_elapsed_ms" "$_prefix.log" "$_merge_status"
  elif [[ "$_merge_rc" -ne 0 ]]; then
    _logfile_print "$_hook_key" "$_prefix.log"
    _warn "  warning: merge failed"
  fi

  return 0
}

_run_merge_hook_batch() {
  local _result_dir _jobs _running=0 _pid
  local _hook_spec _hook_key _script _hook_label
  local _capture_prefix _capture_rc
  local _idx=0 _n_merged=0 _n_failed=0
  # Prompt-capable workers stay in the terminal group and retain the nested
  # supervisor path above. On a platform without strong descendant identity,
  # the outer batch can signal only that capture worker. Give it longer than
  # the hook's normal one-second grace so it can reap a TERM-resistant hook
  # before the batch escalates. Noninteractive workers use one owned group and
  # run their hook directly. The capture worker resets its copied cleanup state,
  # including this value, in _dot_cleanup_prepare_subshell.
  # shellcheck disable=SC2034 # Read dynamically by _dot_cleanup_owned.
  local DOT_CLEANUP_GRACE_ATTEMPTS=40
  local -a _specs=("$@") _pids=()

  ((${#_specs[@]} > 0)) || {
    REPLY=0
    return 0
  }

  if ! _dot_cleanup_mktemp -d; then
    _warn '  warning: could not allocate merge-hook scratch storage'
    DOT_MERGE_FAILED_COUNT=$((${DOT_MERGE_FAILED_COUNT:-0} + ${#_specs[@]}))
    REPLY=0
    return 0
  fi
  _result_dir=$REPLY
  _jobs="$(_merge_parallel_jobs)"

  for _hook_spec in "${_specs[@]}"; do
    IFS=$'\t' read -r _hook_key _script <<<"$_hook_spec"
    _idx=$((_idx + 1))
    _merge_index=$((_merge_index + 1))
    _hook_label="$(_merge_label_from_script "$_hook_key")"

    # Progress reflects the current hook stem, not hook output. Output may be
    # captured or suppressed depending on verbosity, so filenames are the stable
    # identity available before the hook runs.
    if [[ "${DOT_UI_TOTAL:-0}" -gt 0 ]]; then
      if [[ "${DOT_VERBOSE:-0}" -eq 1 ]]; then
        _ui_stage_update "$_hook_label $_merge_index/${#_hooks[@]}"
      else
        _ui_stage_update "$(_merge_progress_detail "$_merge_index" "${#_hooks[@]}" "$_hook_label")"
      fi
    fi

    _dot_cleanup_begin_job_launch
    _run_merge_hook_capture "$_idx" "$_script" "$_result_dir" \
      <&"$DOT_CLEANUP_LAUNCH_STDIN_FD" &
    _pid=$!
    _dot_cleanup_finish_job_launch "$_pid"
    _pids+=("$_pid")
    _running=$((_running + 1))
    if [[ "$_running" -ge "$_jobs" ]]; then
      wait "${_pids[0]}" 2>/dev/null || true
      _dot_cleanup_unregister_pid "${_pids[0]}"
      _pids=("${_pids[@]:1}")
      _running=$((_running - 1))
    fi
  done

  for _pid in "${_pids[@]}"; do
    wait "$_pid" 2>/dev/null || true
    _dot_cleanup_unregister_pid "$_pid"
  done

  _idx=0
  for _hook_spec in "${_specs[@]}"; do
    IFS=$'\t' read -r _hook_key _script <<<"$_hook_spec"
    _idx=$((_idx + 1))
    _capture_prefix="$(_merge_result_prefix "$_result_dir" "$_idx")"
    _capture_rc=1
    { _capture_rc="$(<"$_capture_prefix.rc")"; } 2>/dev/null || _capture_rc=1
    if _print_merge_capture "$_hook_key" "$_idx" "$_result_dir"; then
      _n_merged=$((_n_merged + 1))
    fi
    if [[ "$_capture_rc" -ne 0 ]]; then
      _n_failed=$((_n_failed + 1))
    fi
  done

  _dot_cleanup_remove_path "$_result_dir" || true
  DOT_MERGE_FAILED_COUNT=$((${DOT_MERGE_FAILED_COUNT:-0} + _n_failed))
  REPLY="$_n_merged"
}

_run_merges() {
  local _hooks=()
  local _hook_spec _hook_key _script _specs
  DOT_MERGE_FAILED_COUNT=0

  if ! _specs=$(_merge_hook_specs); then
    _warn '  warning: merge-hook discovery failed'
    return 1
  fi
  while IFS= read -r _hook_spec; do
    [[ -n "$_hook_spec" ]] || continue
    _hooks+=("$_hook_spec")
  done <<<"$_specs"

  if [[ ${#_hooks[@]} -eq 0 ]]; then
    if [[ "${DOT_UI_TOTAL:-0}" -gt 0 ]]; then
      _ui_stage_start "Configs" "checking config hooks"
      _ui_stage_finish ok "no config hooks"
    fi
    return 0
  fi
  if [[ "${DOT_UI_TOTAL:-0}" -gt 0 ]]; then
    _ui_stage_start "Configs" "running config hooks"
  else
    _ui_stage "Configs"
  fi

  local _n_merged=0
  local _merge_index=0
  local -a _parallel_batch=()
  for _hook_spec in "${_hooks[@]}"; do
    IFS=$'\t' read -r _hook_key _script <<<"$_hook_spec"

    if _merge_hook_is_serial "$_hook_key" "$_script"; then
      if ((${#_parallel_batch[@]} > 0)); then
        _run_merge_hook_batch "${_parallel_batch[@]}"
        _n_merged=$((_n_merged + REPLY))
        _parallel_batch=()
      fi
      _run_merge_hook_batch "$_hook_spec"
      _n_merged=$((_n_merged + REPLY))
    else
      _parallel_batch+=("$_hook_spec")
    fi
  done

  if ((${#_parallel_batch[@]} > 0)); then
    _run_merge_hook_batch "${_parallel_batch[@]}"
    _n_merged=$((_n_merged + REPLY))
  fi

  # Non-verbose mode intentionally reports one aggregate config result. The
  # hook-level rows are useful when debugging, but too noisy for the regular
  # fleet update path where `dot update` runs frequently.
  if [[ "$DOT_MERGE_FAILED_COUNT" -gt 0 ]]; then
    if [[ "${DOT_UI_TOTAL:-0}" -gt 0 ]]; then
      _ui_stage_finish warning \
        "$(_merge_warning_summary "$_n_merged" "$DOT_MERGE_FAILED_COUNT")"
    fi
  elif [[ $_n_merged -gt 0 && "${DOT_QUIET:-0}" -ne 1 && "${DOT_VERBOSE:-0}" -ne 1 ]]; then
    if [[ "${DOT_UI_TOTAL:-0}" -gt 0 ]]; then
      _ui_stage_finish ok "$(_merge_summary "$_n_merged")"
    else
      _ui_status ok "$(_merge_summary "$_n_merged")"
    fi
  elif [[ $_n_merged -gt 0 && "${DOT_UI_TOTAL:-0}" -gt 0 ]]; then
    _ui_stage_finish ok "$(_merge_summary "$_n_merged")"
  elif [[ "${DOT_UI_TOTAL:-0}" -gt 0 ]]; then
    # The stage was started because hook files exist, but none defined merge()
    # (e.g. a helper .sh in the dir). Finish it so the dashboard row does not
    # stay stuck in the "running" state.
    _ui_stage_finish ok "no config changes"
  fi
  [[ "$DOT_MERGE_FAILED_COUNT" -eq 0 ]]
}
