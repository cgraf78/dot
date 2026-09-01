# shellcheck shell=bash
# `dot update` orchestration.
#
# This module owns the update lifecycle end-to-end after init has loaded the
# core helpers. The launcher still performs the tiny pre-scan for flags that
# affect shdeps bootstrap before this file is sourced; everything else about
# update/pull behavior should live here so cron, reexec, repo sync, and final
# convergence stay in one readable state machine.

if ! declare -F _dot_profile_lifecycle_prepare >/dev/null 2>&1; then
  # shellcheck source=profile-lifecycle.sh
  . "${BASH_SOURCE[0]%/*}/profile-lifecycle.sh"
fi

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

_dot_update_pull_overlay_phase() {
  local label=$1 count rc=0
  shift
  if [[ ${DOT_REPO_STAGE_DEFERRED_ACTIVE:-0} != 1 ]]; then
    _pull_overlays "$@"
    return
  fi

  count=$(_pull_overlay_count)
  DOT_REPO_PROGRESS_DONE=${DOT_REPO_PROGRESS_DONE:-1}
  DOT_REPO_PROGRESS_TOTAL=$((DOT_REPO_PROGRESS_DONE + count))
  if [[ $count -gt 0 ]]; then
    _ui_stage_update "$(_dot_progress_detail \
      "$label" "$((DOT_REPO_PROGRESS_DONE + 1))" "$DOT_REPO_PROGRESS_TOTAL")"
  fi
  _pull_overlays "$@" || rc=$?
  DOT_REPO_AGG_CURRENT=$((${DOT_REPO_AGG_CURRENT:-0} + ${DOT_PULL_OVERLAY_CURRENT:-0}))
  DOT_REPO_AGG_CHANGED=$((${DOT_REPO_AGG_CHANGED:-0} + ${DOT_PULL_OVERLAY_CHANGED:-0}))
  DOT_REPO_AGG_FAILED=$((${DOT_REPO_AGG_FAILED:-0} + ${DOT_PULL_OVERLAY_FAILED:-0}))
  DOT_REPO_AGG_SKIPPED=$((${DOT_REPO_AGG_SKIPPED:-0} + ${DOT_PULL_OVERLAY_SKIPPED:-0}))
  DOT_REPO_AGG_CHANGED_ITEMS+=${DOT_PULL_OVERLAY_CHANGED_ITEMS:-}
  [[ $rc -eq 0 && ${DOT_PULL_OVERLAY_FAILED:-0} -eq 0 ]]
}

_dot_update_repo_stage_finish() {
  local forced_failure=${1:-0} status=ok summary item
  local -a parts=()
  [[ ${DOT_REPO_STAGE_DEFERRED_ACTIVE:-0} == 1 ]] || return 0
  if [[ $forced_failure -eq 1 || ${DOT_REPO_AGG_FAILED:-0} -gt 0 ]]; then
    status=failed
  elif [[ ${DOT_REPO_AGG_CHANGED:-0} -gt 0 ]]; then
    status=changed
  fi
  [[ ${DOT_REPO_AGG_CHANGED:-0} -eq 0 ]] ||
    parts+=("$(_ui_count_phrase "$DOT_REPO_AGG_CHANGED" repo repos) changed")
  if [[ ${DOT_REPO_AGG_CURRENT:-0} -gt 0 ||
    (${DOT_REPO_AGG_FAILED:-0} -eq 0 && ${DOT_REPO_AGG_SKIPPED:-0} -eq 0) ]]; then
    parts+=("$(_ui_count_phrase "${DOT_REPO_AGG_CURRENT:-0}" repo repos) current")
  fi
  [[ ${DOT_REPO_AGG_FAILED:-0} -eq 0 ]] ||
    parts+=("$(_ui_count_phrase "$DOT_REPO_AGG_FAILED" repo repos) failed")
  [[ ${DOT_REPO_AGG_SKIPPED:-0} -eq 0 ]] ||
    parts+=("$(_ui_count_phrase "$DOT_REPO_AGG_SKIPPED" repo repos) skipped")
  summary=$(_join_comma "${parts[@]}")
  _ui_stage_finish "$status" "$summary"
  if [[ ${DOT_VERBOSE:-0} -eq 0 ]]; then
    while IFS= read -r item; do
      [[ -n $item ]] || continue
      _ui_stage_note changed "$item"
    done <<<"${DOT_REPO_AGG_CHANGED_ITEMS:-}"
  fi
  unset DOT_REPO_STAGE_DEFERRED_ACTIVE DOT_REPO_AGG_CURRENT \
    DOT_REPO_AGG_CHANGED DOT_REPO_AGG_FAILED DOT_REPO_AGG_SKIPPED \
    DOT_REPO_AGG_CHANGED_ITEMS DOT_REPO_PROGRESS_DONE DOT_REPO_PROGRESS_TOTAL
}

_dot_converge_overlays() {
  local entry name phase_status=0 final_status=0
  local -A phase_one_names=()
  local -a additions=()

  _dot_profiles_load_default || return 1
  if [[ $DOT_PROFILES_PRESENT -eq 0 ]]; then
    _discover_overlays || return 1
    _preflight_local_overlays || return 1
    _run_pre_sync_extensions reconcile \
      "${ELIGIBLE_OVERLAYS[@]+"${ELIGIBLE_OVERLAYS[@]}"}" || return 1
    _dot_overlay_use_set eligible
    _dot_update_pull_overlay_phase overlays "$@" || phase_status=$?
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
  _dot_update_pull_overlay_phase phase-one "$@" || phase_status=$?
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
  _preflight_local_overlays || return 1
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
  _dot_update_pull_overlay_phase selected "$@" || final_status=$?
  [[ ${DOT_PULL_OVERLAY_FAILED:-0} -eq 0 ]] || final_status=1
  _discover_overlays || return 1
  _dot_overlay_use_set active
  [[ $phase_status -eq 0 && $final_status -eq 0 ]]
}

_dot_update_sync_repos() {
  local sync_status=0
  if _base_repo_exists; then
    # Sourced callers may run more than one update in a process. The pull
    # adapter fills these only when a fetched base generation can actually
    # mutate HOME; never let a prior run authorize rollback in a later one.
    # shellcheck disable=SC2034 # Rollback helpers consume these dynamically.
    DOT_OVERLAY_ROLLBACK_PATHS=()
    # shellcheck disable=SC2034 # Rollback helpers consume these dynamically.
    DOT_OVERLAY_ROLLBACK_TARGETS=()
    # shellcheck disable=SC2034 # Repository helpers consume these dynamically.
    OVERLAYS=()
    # shellcheck disable=SC2034 # Read dynamically by _repo_pull_all.
    local DOT_PULL_DEFER_FINISH=1
    if _repo_pull_all "$@"; then
      :
    else
      sync_status=$?
    fi
  else
    _ensure_repo_config
  fi

  if [[ "$sync_status" -ne 0 ]]; then
    _dot_update_repo_stage_finish 1
    _overlay_restore_installed_links ||
      _warn '  warning: could not restore the previous overlay-link generation'
    DOT_OVERLAY_LINKS_FROZEN=1
    return "$sync_status"
  fi

  # A base pull may replace profile and descriptor policy. Reload data from the
  # accepted generation before either phase is resolved or any overlay
  # transport preparation runs.
  if ! dot_config_load; then
    _dot_update_repo_stage_finish 1
    _overlay_restore_installed_links ||
      _warn '  warning: could not restore the previous overlay-link generation'
    DOT_OVERLAY_LINKS_FROZEN=1
    return 1
  fi
  if [[ ${DOT_INIT_SKIP_PROVIDER:-0} == 1 ]]; then
    DOT_DEPENDENCY_PROVIDER=none
    export DOT_DEPENDENCY_PROVIDER
  fi
  if ! _dot_converge_overlays "$@"; then
    _dot_update_repo_stage_finish 1
    _overlay_restore_installed_links ||
      _warn '  warning: could not restore the previous overlay-link generation'
    DOT_OVERLAY_LINKS_FROZEN=1
    return 1
  fi
  if ! _dot_profile_lifecycle_prepare; then
    _dot_update_repo_stage_finish 1
    if _base_repo_exists; then
      _overlay_restore_installed_links ||
        _warn '  warning: could not restore the previous overlay-link generation'
    fi
    DOT_OVERLAY_LINKS_FROZEN=1
    return 1
  fi
  _dot_update_repo_stage_finish 0
}

_dot_update_skip_inputs() {
  local reason=$1
  _ui_stage_start "Tools" "skipping configured dependencies"
  _ui_stage_finish warning "$reason; dependencies skipped"
  _ui_stage_start "Configs" "skipping config hooks"
  _ui_stage_finish warning "$reason; config hooks skipped"
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
  if [[ ${DOT_OVERLAY_LINKS_FROZEN:-0} == 1 ]]; then
    _ui_stage_start "Overlays" "preserving installed overlay links"
    _ui_stage_finish warning "profile resolution or repository sync failed"
    update_status=1
    inputs_ready=0
  elif ! _link_overlays; then
    update_status=1
    inputs_ready=0
  fi
  if [[ $inputs_ready -eq 0 ]]; then
    _dot_update_skip_inputs 'repository synchronization failed'
  else
    if ! _dot_profile_lifecycle_retire; then
      update_status=1
      inputs_ready=0
      _dot_update_skip_inputs 'profile deactivation failed'
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
  fi
  if [[ $inputs_ready -eq 1 && $update_status -eq 0 ]]; then
    if ! _dot_profile_lifecycle_commit; then
      _warn '  warning: could not commit profile lifecycle state'
      update_status=1
    fi
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
  unset DOT_OVERLAY_LINKS_FROZEN

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
