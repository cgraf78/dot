# shellcheck shell=bash
# Core health checks plus isolated, versioned client doctor extensions.

_DOT_DOCTOR_DIR=${BASH_SOURCE[0]%/*}/doctor
_DOT_DOCTOR_LOADED=0

if ! declare -F _dot_source_git >/dev/null 2>&1; then
  # shellcheck source=temp.sh
  . "${BASH_SOURCE[0]%/*}/temp.sh"
fi

if ! declare -F _dot_extension_worker_exec >/dev/null 2>&1; then
  # shellcheck source=extension-worker-launch.sh
  . "${BASH_SOURCE[0]%/*}/extension-worker-launch.sh"
fi

_dot_doctor_load() {
  [[ $_DOT_DOCTOR_LOADED -eq 0 ]] || return 0
  # shellcheck source=doctor/runtime.sh
  . "$_DOT_DOCTOR_DIR/runtime.sh"
  # shellcheck source=doctor/paths.sh
  . "$_DOT_DOCTOR_DIR/paths.sh"
  # shellcheck source=doctor/repos.sh
  . "$_DOT_DOCTOR_DIR/repos.sh"
  # shellcheck source=doctor/lock.sh
  . "$_DOT_DOCTOR_DIR/lock.sh"
  # shellcheck source=doctor/provider.sh
  . "$_DOT_DOCTOR_DIR/provider.sh"
  # shellcheck source=doctor/overlays.sh
  . "$_DOT_DOCTOR_DIR/overlays.sh"
  # shellcheck source=doctor/merges.sh
  . "$_DOT_DOCTOR_DIR/merges.sh"
  _DOT_DOCTOR_LOADED=1
}

_dr_check_runtime() {
  local git_version checkout_root source_root
  _dr_section 'dot runtime'
  if [[ ${BASH_VERSINFO[0]} -ge 4 ]]; then
    _dr_ok 'Bash runtime' "${BASH_VERSION}"
  else
    _dr_fail 'Bash runtime is too old' 'Bash 4 or newer is required'
  fi
  checkout_root=$(_dot_source_git rev-parse --show-toplevel 2>/dev/null || true)
  checkout_root=$(cd -P -- "$checkout_root" 2>/dev/null && pwd -P || true)
  source_root=$(cd -P -- "$DOT_SOURCE_ROOT" 2>/dev/null && pwd -P || true)
  if [[ -n $checkout_root && $checkout_root == "$source_root" ]]; then
    _dr_ok 'dot checkout exists' "$(_dr_tilde "$DOT_SOURCE_ROOT")"
  else
    _dr_fail 'dot checkout is unavailable' "$DOT_SOURCE_ROOT"
  fi
  git_version=$(git --version 2>/dev/null || true)
  if [[ -n $git_version ]]; then
    _dr_ok 'Git runtime' "$git_version"
  else
    _dr_fail 'Git runtime is unavailable'
  fi
  _dr_ok 'configuration version' "${DOT_CONFIG_VERSION:-1}"
  _dr_check_engine_source
}

_dr_check_engine_source() {
  local source managed development source_real managed_real='' development_real=''

  source=$DOT_SOURCE_ROOT
  managed=${SHDEPS_INSTALL_DIR:-$HOME/.local/share}/cgraf78/dot
  development=${SHDEPS_GIT_DEV_DIR:-$HOME/git}/dot
  source_real=$(cd -P -- "$source" 2>/dev/null && pwd -P) || source_real=$source
  [[ ! -d $managed ]] || managed_real=$(cd -P -- "$managed" 2>/dev/null && pwd -P)
  [[ ! -d $development ]] || development_real=$(cd -P -- "$development" 2>/dev/null && pwd -P)

  if [[ ${DOT_IGNORE_DEV_CHECKOUT:-0} == 1 ]]; then
    _dr_warn 'development checkout bypass enabled' \
      'the provider will use the managed checkout for this invocation'
  fi
  if [[ -n $development_real && $source_real == "$development_real" ]]; then
    _dr_ok 'dot engine source' "development checkout: $(_dr_tilde "$development")"
  elif [[ -n $managed_real && $source_real == "$managed_real" ]]; then
    _dr_ok 'dot engine source' "managed checkout: $(_dr_tilde "$managed")"
  else
    # Running doctor from a source checkout is useful during development and
    # does not by itself make the runtime unhealthy. Keep the location visible
    # without turning every repository test checkout into a false failure.
    _dr_warn 'dot engine source is outside managed locations' "$source_real"
  fi
}

_dot_doctor_extension_specs() {
  local root script key identity
  local -A seen=()

  [[ ${DOT_EXTENSION_API:-} == 1 && -n ${DOT_EXTENSIONS_DIR:-} ]] || return 0
  _dot_extension_root_validate || return 1
  root=$DOT_EXTENSIONS_DIR/doctor.d
  if [[ ! -e $root && ! -L $root ]]; then
    return 0
  fi
  _dot_extension_directory_validate "$root" || return 1
  for script in "$root"/*.sh; do
    [[ -e $script || -L $script ]] || continue
    _dot_extension_file_validate "$script" || {
      printf 'dot: unsafe doctor extension: %s\n' "$script" >&2
      return 1
    }
    key=${script##*/}
    key=${key%.sh}
    [[ $key =~ ^([0-9]+[-_])?([a-z][a-z0-9-]*)$ ]] || {
      printf 'dot: invalid doctor extension identity: %s\n' "${script##*/}" >&2
      return 1
    }
    identity=${BASH_REMATCH[2]}
    [[ -z ${seen[$identity]+x} ]] || {
      printf 'dot: duplicate doctor extension identity: %s\n' "$identity" >&2
      return 1
    }
    seen[$identity]=1
    printf '%s\t%s\n' "$key" "$script"
  done | LC_ALL=C sort
}

_dot_doctor_render_records() {
  local record=$1 kind message detail
  while IFS=$'\t' read -r kind message detail || [[ -n $kind ]]; do
    case $kind in
      section) _dr_section "$message" ;;
      ok) _dr_ok "$message" "$detail" ;;
      warn) _dr_warn "$message" "$detail" ;;
      fail) _dr_fail "$message" "$detail" ;;
      skip) _dr_skip "$message" "$detail" ;;
      *) _dr_fail 'doctor extension emitted an invalid result' "$kind" ;;
    esac
  done <"$record"
}

_dot_doctor_run_extension() {
  local key=$1 script=$2 temporary result log rc=0 worker_pid

  if ! _dot_cleanup_mktemp -d; then
    _dr_fail "$key doctor extension temporary directory unavailable" \
      'check TMPDIR permissions and free space'
    return 1
  fi
  temporary=$REPLY
  result=$temporary/results
  log=$temporary/output
  : >"$result"
  chmod 0600 "$result"
  _dot_cleanup_begin_job_launch closed-stdin
  _dot_extension_worker_exec doctor "$script" "$temporary" "$result" \
    <&"$DOT_CLEANUP_LAUNCH_STDIN_FD" >"$log" 2>&1 &
  worker_pid=$!
  _dot_cleanup_finish_job_launch "$worker_pid"
  if wait "$worker_pid"; then
    rc=0
  else
    rc=$?
  fi
  _dot_cleanup_unregister_pid "$worker_pid"
  _dot_doctor_render_records "$result"
  if [[ $rc -ne 0 ]]; then
    _dr_fail "$key doctor extension failed" "$(tr '\n' ' ' <"$log")"
  elif [[ -s $log ]]; then
    _dr_warn "$key doctor extension wrote outside the result API" \
      "$(tr '\n' ' ' <"$log")"
  fi
  _dot_cleanup_remove_path "$temporary" || true
  return "$rc"
}

_dot_doctor() {
  local spec key script specs='' status=0 summary color

  _dot_doctor_load
  dot_ui_title 'dot doctor'
  _dr_check_runtime
  _dr_check_base_repo
  _dr_check_update_lock
  _dr_check_provider
  _dr_check_overlays
  _dr_check_merges
  if ! specs=$(_dot_doctor_extension_specs); then
    _dr_fail 'doctor extension discovery failed'
    status=1
  else
    while IFS= read -r spec; do
      [[ -n $spec ]] || continue
      IFS=$'\t' read -r key script <<<"$spec"
      _dot_doctor_run_extension "$key" "$script" || status=1
    done <<<"$specs"
  fi

  summary=$(printf '%d passed · %d warnings · %d failed' \
    "$_DR_PASS_COUNT" "$_DR_WARN_COUNT" "$_DR_FAIL_COUNT")
  if [[ $_DR_FAIL_COUNT -gt 0 ]]; then
    color=red
  elif [[ $_DR_WARN_COUNT -gt 0 ]]; then
    color=yellow
  else
    color=green
  fi
  printf '\n'
  dot_ui_summary_box "$color" "$summary"
  [[ $_DR_FAIL_COUNT -eq 0 && $status -eq 0 ]]
}
