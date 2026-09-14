# shellcheck shell=bash
# Read-only health reporting for dot's process-wide mutation lock.

_dr_check_update_lock() {
  local lock_dir

  _dr_section 'Update lock'
  if ! _dot_update_lock_path; then
    _dr_fail 'update lock path cannot be resolved'
    return 0
  fi
  lock_dir=$REPLY
  if [[ ! -e $lock_dir && ! -L $lock_dir ]]; then
    _dr_ok 'update lock is clear'
    return 0
  fi
  if [[ ! -d $lock_dir || -L $lock_dir ]]; then
    _dr_fail 'update lock path is unsafe' "$lock_dir"
    return 0
  fi

  if _dot_update_lock_read_owner "$lock_dir"; then
    if _dot_update_lock_owner_is_active; then
      _dr_warn 'update is currently running' "pid $DOT_UPDATE_LOCK_OWNER_PID"
    else
      _dr_warn 'update lock owner is stale' \
        'the next mutating command will reclaim it'
    fi
  elif _dot_update_lock_is_initializing "$lock_dir"; then
    _dr_warn 'update lock is being initialized'
  else
    _dr_warn 'update lock record is incomplete' \
      'the next mutating command will attempt recovery'
  fi
}
