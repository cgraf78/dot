# shellcheck shell=bash
# Private command dispatcher. Public callers enter through bin/dot.

dot_command_dispatch() {
  local command=${1:-help} rc=0
  shift || true

  case $command in
    update)
      _dot_cleanup_install_owner_traps
      _dot_update_lock_acquire "$@" || return $?
      _dot_update "$@"
      ;;
    pull)
      dot_command_dispatch update "$@"
      ;;
    fetch)
      _dot_resolve_overlays fetch || return 1
      _repo_fetch_all "$@"
      ;;
    push)
      _dot_resolve_overlays inspect || return 1
      _repo_push_all "$@"
      ;;
    status)
      _dot_resolve_overlays inspect || return 1
      _repo_status_all "$@"
      ;;
    diff)
      _dot_resolve_overlays inspect || return 1
      _repo_diff_all "$@"
      ;;
    cron)
      crontab -l 2>/dev/null || printf '  no crontab installed\n'
      ;;
    doctor)
      _dot_cleanup_install_owner_traps
      # shellcheck disable=SC2034 # Read dynamically by overlay discovery.
      local DOT_OVERLAY_DISCOVERY_SILENT=1
      _dot_resolve_overlays inspect || true
      _dot_doctor
      ;;
    test)
      _dot_cleanup_install_owner_traps
      _dot_resolve_overlays inspect || return 1
      dot_test_command "$@" || rc=$?
      ;;
    init)
      _dot_cleanup_install_owner_traps
      case ${1:-} in
        --status | --help | -h) ;;
        *) _dot_update_lock_acquire || return $? ;;
      esac
      dot_init_command "$@"
      ;;
    *)
      printf 'dot: unknown command: %s\n' "$command" >&2
      rc=1
      ;;
  esac
  return "$rc"
}
