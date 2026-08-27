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

_dot_converge_overlays() {
  local entry name phase_status=0 final_status=0
  local -A phase_one_names=()
  local -a additions=()

  _dot_profiles_load_default || return 1
  if [[ $DOT_PROFILES_PRESENT -eq 0 ]]; then
    _discover_overlays || return 1
    _run_pre_sync_extensions reconcile \
      "${ELIGIBLE_OVERLAYS[@]+"${ELIGIBLE_OVERLAYS[@]}"}" || return 1
    _dot_overlay_use_set eligible
    _pull_overlays "$@" || phase_status=$?
    [[ ${DOT_PULL_OVERLAY_FAILED:-0} -eq 0 ]] || phase_status=1
    _discover_overlays || return 1
    _dot_overlay_use_set active
    return "$phase_status"
  fi

  _dot_profile_select_base || return 1
  _discover_overlays || return 1
  _preflight_local_overlays || return 1
  # shellcheck disable=SC2034 # Published for lifecycle doctor reporting.
  PHASE_ONE_SELECTED_OVERLAY_NAMES=(
    "${SELECTED_OVERLAY_NAMES[@]+"${SELECTED_OVERLAY_NAMES[@]}"}"
  )
  PHASE_ONE_ELIGIBLE_OVERLAYS=(
    "${ELIGIBLE_OVERLAYS[@]+"${ELIGIBLE_OVERLAYS[@]}"}"
  )
  _run_pre_sync_extensions prepare \
    "${PHASE_ONE_ELIGIBLE_OVERLAYS[@]+"${PHASE_ONE_ELIGIBLE_OVERLAYS[@]}"}" ||
    return 1
  _dot_overlay_use_set eligible
  _pull_overlays "$@" || phase_status=$?
  [[ ${DOT_PULL_OVERLAY_FAILED:-0} -eq 0 ]] || phase_status=1
  _discover_overlays || return 1
  # shellcheck disable=SC2034 # Published for lifecycle doctor reporting.
  PHASE_ONE_ACTIVE_OVERLAYS=(
    "${ACTIVE_OVERLAYS[@]+"${ACTIVE_OVERLAYS[@]}"}"
  )
  _dot_overlay_use_set active
  [[ $phase_status -eq 0 ]] || return "$phase_status"

  _dot_profile_resolve_default || return 1
  _discover_overlays || return 1
  _run_pre_sync_extensions reconcile \
    "${ELIGIBLE_OVERLAYS[@]+"${ELIGIBLE_OVERLAYS[@]}"}" || return 1
  for entry in "${PHASE_ONE_ELIGIBLE_OVERLAYS[@]+"${PHASE_ONE_ELIGIBLE_OVERLAYS[@]}"}"; do
    name=${entry%%|*}
    phase_one_names["$name"]=1
  done
  for entry in "${ELIGIBLE_OVERLAYS[@]+"${ELIGIBLE_OVERLAYS[@]}"}"; do
    name=${entry%%|*}
    [[ -n ${phase_one_names[$name]+x} ]] || additions+=("$entry")
  done
  OVERLAYS=("${additions[@]+"${additions[@]}"}")
  _pull_overlays "$@" || final_status=$?
  [[ ${DOT_PULL_OVERLAY_FAILED:-0} -eq 0 ]] || final_status=1
  _discover_overlays || return 1
  _dot_overlay_use_set active
  [[ $phase_status -eq 0 && $final_status -eq 0 ]]
}

_dot_update_sync_repos() {
  local sync_status=0
  if _base_repo_exists; then
    # shellcheck disable=SC2034 # Repository helpers consume the selected set dynamically.
    OVERLAYS=()
    if _repo_pull_all "$@"; then
      :
    else
      sync_status=$?
    fi
  else
    _ensure_repo_config
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

  # A base pull may replace profile and descriptor policy. Reload data from the
  # accepted generation before either phase is resolved or any overlay
  # transport preparation runs.
  dot_config_load || return 1
  if [[ ${DOT_INIT_SKIP_PROVIDER:-0} == 1 ]]; then
    DOT_DEPENDENCY_PROVIDER=none
    export DOT_DEPENDENCY_PROVIDER
  fi
  _dot_converge_overlays "$@"
}

_dot_update_finalize() {
  local update_status="${1:-0}" inputs_ready=1
  local shdeps_ready=0 provider_revision_before=''
  [[ $update_status -eq 0 ]] || inputs_ready=0
  if [[ "${DOT_UI_TOTAL:-0}" -le 0 ]]; then
    _ui_begin 4
  fi
  if ! _dot_provider_consume_checkpoint; then
    _ui_done 1
    return 1
  fi
  _ensure_repo_config
  if ! _link_overlays; then
    update_status=1
    inputs_ready=0
  fi
  if [[ $inputs_ready -eq 0 ]]; then
    _ui_stage_start "Tools" "skipping configured dependencies"
    _ui_stage_finish warning "repository synchronization failed; dependencies skipped"
    _ui_stage_start "Configs" "skipping config hooks"
    _ui_stage_finish warning "repository synchronization failed; config hooks skipped"
  else
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
      update_status=1
      _ui_stage_start "Tools" "checking configured dependencies"
      _ui_stage_finish failed "shdeps unavailable; dependency install skipped"
    fi
    _run_merges || update_status=1
  fi
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

  _ui_begin 5

  # Cron updates must never fight with active local edits. First discard
  # mtime-only or content-identical dirty files written by sync tooling; if
  # anything real remains dirty, exit silently just like the historical path.
  if [[ "$cron_mode" -eq 1 ]] && _is_worktree_dirty; then
    if ! _try_resolve_dirty; then
      exit 0
    fi
  fi

  if _dot_update_sync_repos "$@"; then
    # Keep a final defensive reload for sourced/test callers that replace the
    # repository-sync adapter. The real adapter already reloads before profile
    # resolution, but provider selection must never continue with stale policy.
    if ! dot_config_load; then
      _ui_done 1
      return 1
    fi
    if [[ ${DOT_INIT_SKIP_PROVIDER:-0} == 1 ]]; then
      DOT_DEPENDENCY_PROVIDER=none
      export DOT_DEPENDENCY_PROVIDER
    fi
  else
    update_status=1
  fi
  _dot_update_finalize "$update_status" || update_status=1
  return "$update_status"
}
