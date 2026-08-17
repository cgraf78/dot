# shellcheck shell=bash
# Private command dispatcher. Public callers enter through bin/dot.

dot_command_dispatch() {
  local command=${1:-help} rc=0
  shift || true

  case $command in
    update)
      _dot_cleanup_install_owner_traps
      _dot_update_lock_acquire "$@" || return $?
      _discover_overlays || return 1
      _preflight_local_overlays || return 1
      _dot_update "$@"
      ;;
    pull)
      dot_command_dispatch update "$@"
      ;;
    fetch)
      _discover_overlays || return 1
      _repo_fetch_all "$@"
      ;;
    push)
      _discover_overlays || return 1
      _repo_push_all "$@"
      ;;
    status)
      _discover_overlays || return 1
      _repo_status_all "$@"
      ;;
    diff)
      _discover_overlays || return 1
      _repo_diff_all "$@"
      ;;
    cron)
      crontab -l 2>/dev/null || printf '  no crontab installed\n'
      ;;
    doctor)
      _dot_cleanup_install_owner_traps
      _discover_overlays || true
      _dot_doctor
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
