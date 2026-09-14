# shellcheck shell=bash
# Persist and retire overlay-owned side effects across profile changes.

DOT_PROFILE_LIFECYCLE_RECORDS=()

_dot_profile_lifecycle_file_safe() {
  local path=$1 size
  _overlay_private_regular_file "$path" || return 1
  size=$(LC_ALL=C wc -c <"$path" 2>/dev/null | tr -d '[:space:]') || return 1
  [[ $size =~ ^[0-9]+$ && $size -le 1048576 ]]
}

_dot_profile_lifecycle_load() {
  DOT_PROFILE_LIFECYCLE_RECORDS=()
  local ledger=${DOT_PROFILE_LIFECYCLE_LEDGER:-} line first=1 name
  local -A seen=()

  [[ -n $ledger ]] || return 1
  [[ -e $ledger || -L $ledger ]] || return 0
  _dot_profile_lifecycle_file_safe "$ledger" || {
    _warn "  warning: unsafe profile lifecycle ledger: $ledger"
    return 1
  }
  while IFS= read -r line || [[ -n $line ]]; do
    if [[ $first -eq 1 ]]; then
      first=0
      [[ $line == version=1 ]] || {
        _warn "  warning: unsupported profile lifecycle ledger: $ledger"
        return 1
      }
      continue
    fi
    [[ -n $line ]] || {
      _warn "  warning: malformed profile lifecycle ledger: $ledger"
      return 1
    }
    _dot_overlay_record_validate "$line" || {
      _warn "  warning: invalid profile lifecycle record"
      return 1
    }
    name=${line%%|*}
    [[ -z ${seen[$name]+x} ]] || {
      _warn "  warning: duplicate profile lifecycle record: $name"
      return 1
    }
    seen["$name"]=1
    DOT_PROFILE_LIFECYCLE_RECORDS+=("$line")
  done <"$ledger"
  [[ $first -eq 0 ]] || {
    _warn "  warning: empty profile lifecycle ledger: $ledger"
    return 1
  }
}

_dot_profile_lifecycle_write() {
  local ledger=${DOT_PROFILE_LIFECYCLE_LEDGER:-} directory temporary record
  local -a records=("$@")

  [[ -n $ledger ]] || return 1
  directory=${ledger%/*}
  if [[ ! -d $directory ]]; then
    (umask 077 && mkdir -p "$directory") || return 1
  fi
  _dot_overlay_context_directory_safe "$directory" || return 1
  _dot_cleanup_mktemp "$directory/.profile-overlay-lifecycle.XXXXXXXX" ||
    return 1
  temporary=$REPLY
  chmod 0600 "$temporary" || {
    _dot_cleanup_remove_path "$temporary" || true
    return 1
  }
  {
    printf 'version=1\n'
    for record in "${records[@]}"; do
      printf '%s\n' "$record"
    done
  } >"$temporary" || {
    _dot_cleanup_remove_path "$temporary" || true
    return 1
  }
  mv -f -- "$temporary" "$ledger" || {
    _dot_cleanup_remove_path "$temporary" || true
    return 1
  }
  _dot_cleanup_remove_path "$temporary" || true
}

_dot_profile_deactivation_script() {
  local record=$1 name path _url _descriptor _optional _sync script
  _dot_overlay_record_validate "$record" || return 1
  IFS='|' read -r name path _url _descriptor _optional _sync <<<"$record"
  script=$path/dot/profile-deactivate
  [[ -e $script || -L $script ]] || return 2
  _dot_profile_deactivation_validate "$record" "$script" || return 1
  REPLY=$script
}

_dot_profile_lifecycle_prepare() {
  [[ ${DOT_PROFILES_PRESENT:-0} -eq 1 ]] || return 0
  _dot_profile_lifecycle_load || return 1

  local record name script_rc
  local -a prepared=()
  local -A current=() retained=() eligible=()
  for name in "${ELIGIBLE_OVERLAY_NAMES[@]}"; do
    eligible["$name"]=1
  done
  for record in "${PHASE_ONE_ACTIVE_OVERLAYS[@]}" "${ACTIVE_OVERLAYS[@]}"; do
    name=${record%%|*}
    current["$name"]=$record
  done
  for record in "${DOT_PROFILE_LIFECYCLE_RECORDS[@]}"; do
    name=${record%%|*}
    retained["$name"]=$record
  done
  if ! _dot_extensions_enabled; then
    for name in "${!retained[@]}"; do
      [[ -n ${eligible[$name]+x} ]] && continue
      _warn "  warning: profile deactivation pending while extensions are disabled: $name"
      return 1
    done
    return 0
  fi
  for name in "${!retained[@]}"; do
    [[ -n ${eligible[$name]+x} ]] && continue
    if ! _dot_profile_deactivation_script "${retained[$name]}" >/dev/null; then
      _warn "  warning: unsafe retiring overlay entrypoint: $name"
      return 1
    fi
  done
  for name in "${!current[@]}"; do
    record=${current[$name]}
    script_rc=0
    _dot_profile_deactivation_script "$record" >/dev/null || script_rc=$?
    case $script_rc in
      0) retained["$name"]=$record ;;
      2)
        if [[ -n ${retained[$name]+x} ]]; then
          _warn "  warning: active overlay removed profile deactivation entrypoint: $name"
          return 1
        fi
        ;;
      *)
        _warn "  warning: unsafe profile deactivation entrypoint: $name"
        return 1
        ;;
    esac
  done
  while IFS= read -r name; do
    [[ -n $name ]] && prepared+=("${retained[$name]}")
  done < <(printf '%s\n' "${!retained[@]}" | LC_ALL=C sort)
  _dot_profile_lifecycle_write "${prepared[@]}" || return 1
  DOT_PROFILE_LIFECYCLE_RECORDS=("${prepared[@]}")
}

_dot_profile_lifecycle_run_one() {
  local record=$1 script result_dir context token result_file output rc=0
  _dot_profile_deactivation_script "$record" || return 1
  script=$REPLY
  _dot_cleanup_mktemp -d || return 1
  result_dir=$REPLY
  result_file=$result_dir/has-deactivate
  if _dot_overlay_context_create "$result_dir" deactivate retiring none "$record"; then
    context=$REPLY_PATH
    token=$REPLY_TOKEN
    output=$(
      _dot_extension_worker_run deactivate "$script" "$result_dir" \
        "$result_file" "$context" "$token" 2>&1
    ) || rc=$?
  else
    rc=1
    output='could not create deactivation context'
  fi
  _dot_cleanup_remove_path "$result_dir" || true
  if [[ $rc -ne 0 ]]; then
    [[ -z $output ]] || _warn "$output"
    return "$rc"
  fi
  [[ -z $output || ${DOT_VERBOSE:-0} -ne 1 ]] || _log "$output"
}

_dot_profile_lifecycle_retire() {
  [[ ${DOT_PROFILES_PRESENT:-0} -eq 1 ]] && _dot_extensions_enabled || return 0
  local record name failed=0
  local -A eligible=()
  for name in "${ELIGIBLE_OVERLAY_NAMES[@]}"; do
    eligible["$name"]=1
  done
  for record in "${DOT_PROFILE_LIFECYCLE_RECORDS[@]}"; do
    name=${record%%|*}
    [[ -z ${eligible[$name]+x} ]] || continue
    if ! _dot_profile_lifecycle_run_one "$record"; then
      _warn "  warning: profile deactivation failed: $name"
      failed=1
    fi
  done
  return "$failed"
}

_dot_profile_lifecycle_commit() {
  [[ ${DOT_PROFILES_PRESENT:-0} -eq 1 ]] && _dot_extensions_enabled || return 0
  local record name
  local -a committed=()
  local -A eligible=() active=()
  for name in "${ELIGIBLE_OVERLAY_NAMES[@]}"; do
    eligible["$name"]=1
  done
  for record in "${ACTIVE_OVERLAYS[@]}"; do
    name=${record%%|*}
    active["$name"]=$record
  done
  for record in "${DOT_PROFILE_LIFECYCLE_RECORDS[@]}"; do
    name=${record%%|*}
    [[ -n ${eligible[$name]+x} ]] || continue
    [[ -z ${active[$name]+x} ]] || continue
    committed+=("$record")
  done
  while IFS= read -r name; do
    [[ -n $name ]] || continue
    record=${active[$name]}
    if _dot_profile_deactivation_script "$record" >/dev/null; then
      committed+=("$record")
    elif [[ $? -ne 2 ]]; then
      return 1
    fi
  done < <(printf '%s\n' "${!active[@]}" | LC_ALL=C sort)
  _dot_profile_lifecycle_write "${committed[@]}"
}
