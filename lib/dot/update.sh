# shellcheck shell=bash
# `dot update` orchestration.
#
# This module owns the update lifecycle end-to-end after init has loaded the
# core helpers. The launcher still performs the tiny pre-scan for flags that
# affect shdeps bootstrap before this file is sourced; everything else about
# update/pull behavior should live here so cron, reexec, repo sync, and final
# convergence stay in one readable state machine.

_dot_update_no_base_pull() {
  _ensure_repo_config
  _ui_stage_start "Repos" "checking repositories"
  _pull_overlays
  local status=ok
  [[ "${DOT_PULL_OVERLAY_FAILED:-0}" -eq 0 ]] || status=failed
  if [[ -n "${REPLY:-}" ]]; then
    _ui_stage_finish "$status" "no base repo, $REPLY"
  else
    _ui_stage_finish "$status" "no base repo"
  fi
  [[ "${DOT_PULL_OVERLAY_FAILED:-0}" -eq 0 ]]
}

_dot_update_cpu_count() {
  local _n=""
  _n=$(getconf _NPROCESSORS_ONLN 2>/dev/null || true)
  if [[ -z "$_n" && "$(uname -s)" = "Darwin" ]]; then
    _n=$(sysctl -n hw.ncpu 2>/dev/null || true)
  fi
  case "$_n" in
    '' | *[!0-9]*) _n=4 ;;
  esac
  [[ "$_n" -lt 1 ]] && _n=1
  printf '%s\n' "$_n"
}

_dot_update_jobs() {
  local _jobs="${DOT_UPDATE_JOBS:-}"
  case "$_jobs" in
    '' | *[!0-9]*) _jobs="$(_dot_update_cpu_count)" ;;
  esac
  [[ "$_jobs" -lt 1 ]] && _jobs=1
  printf '%s\n' "$_jobs"
}

_dot_update_prepare_shdeps_jobs() {
  [[ -n "${SHDEPS_JOBS+x}" ]] && return 0
  SHDEPS_JOBS="$(_dot_update_jobs)"
  export SHDEPS_JOBS
}

_dot_update_sync_repos() {
  local sync_status=0
  if _base_repo_exists; then
    if _repo_pull_all "$@"; then
      :
    else
      sync_status=$?
    fi
  else
    if _dot_update_no_base_pull; then
      :
    else
      sync_status=$?
    fi
  fi

  if [[ "$sync_status" -ne 0 ]]; then
    # Pull validation can reject an overlay after its previous links were
    # unstashed. Reconcile the manifest before returning so untracked links do
    # not remain attached to a checkout that no longer owns the configured URL.
    if ! _link_overlays; then
      _warn "  warning: overlay link cleanup failed after repository sync failure"
    fi
    return "$sync_status"
  fi

}

_dot_update_finalize() {
  local update_status="${1:-0}" shdeps_ready=0 provider_revision_before=''
  if [[ "${DOT_UI_TOTAL:-0}" -le 0 ]]; then
    _ui_begin 4
  fi
  if ! _dot_provider_consume_checkpoint; then
    _ui_done 1
    return 1
  fi
  _ensure_repo_config
  _link_overlays || update_status=1
  _dot_extension_modes_normalize || update_status=1
  if [[ "${DOT_DEPENDENCY_PROVIDER:-none}" == shdeps ]]; then
    if _ensure_shdeps; then
      shdeps_ready=1
    else
      update_status=1
    fi
  fi
  if [[ "${DOT_DEPENDENCY_PROVIDER:-none}" == none ]]; then
    _ui_stage_start "Tools" "checking configured dependencies"
    _ui_stage_finish ok "no dependency provider"
  elif [[ "$shdeps_ready" -eq 1 ]] && declare -f shdeps_update &>/dev/null; then
    _ui_stage_start "Tools" "checking configured dependencies"
    provider_revision_before=$(_dot_active_revision)
    if _run_shdeps_update_ui; then
      _ui_stage_finish "${DOT_UI_SHDEPS_STATUS:-ok}" "${DOT_UI_SHDEPS_SUMMARY:-dependencies checked}"
      _shdeps_print_group_summaries
      if ! _dot_provider_maybe_reexec "$provider_revision_before"; then
        _ui_done 1
        return 1
      fi
    else
      update_status=1
      _ui_stage_finish failed "${DOT_UI_SHDEPS_SUMMARY:-dependency update failed}"
      _shdeps_print_group_summaries
    fi
  else
    _ui_stage_start "Tools" "checking configured dependencies"
    update_status=1
    _ui_stage_finish failed "shdeps unavailable; dependency install skipped"
  fi
  _run_merges || update_status=1
  if _base_repo_exists; then
    _ui_stage_start "Cleanup" "normalizing worktree"
    _normalize_filtered
    _ui_stage_finish ok "worktree normalized"
  elif [[ "${DOT_UI_TOTAL:-0}" -gt 0 ]]; then
    _ui_stage_start "Cleanup" "normalizing worktree"
    _ui_stage_finish ok "no base repo"
  fi
  _ui_done "$update_status"
  return "$update_status"
}

_dot_update() {
  local cron_mode=0 update_status=0

  while [[ "${1:-}" == -* ]]; do
    case "$1" in
      --cron)
        cron_mode=1
        export DOT_QUIET=1
        export SHDEPS_QUIET=1
        shift
        ;;
      --quiet)
        export DOT_QUIET=1
        export SHDEPS_QUIET=1
        shift
        ;;
      -f | --force)
        export DOT_FORCE=1
        export SHDEPS_FORCE=1
        shift
        ;;
      -v | --verbose)
        export DOT_VERBOSE=1
        export SHDEPS_LOG_LEVEL=2
        shift
        ;;
      *) break ;;
    esac
  done

  # The launcher performs this before entering update. Keep a defensive gate
  # here for sourced callers so SSH, repository, and HOME mutations cannot
  # precede validation of an active filesystem overlay.
  _preflight_local_overlays || return 1

  # Existing trusted extensions must be safe before pre-sync code runs. A
  # second pass after repository/link convergence handles newly cloned files.
  _dot_extension_modes_normalize || return 1

  _ui_begin 5

  # Cron updates must never fight with active local edits. First discard
  # mtime-only or content-identical dirty files written by sync tooling; if
  # anything real remains dirty, exit silently just like the historical path.
  if [[ "$cron_mode" -eq 1 ]] && _is_worktree_dirty; then
    if ! _try_resolve_dirty; then
      exit 0
    fi
  fi

  _run_pre_sync_extensions || return 1

  _dot_update_sync_repos "$@" || update_status=1
  _dot_update_finalize "$update_status" || update_status=1
  return "$update_status"
}
