# shellcheck shell=bash
# Simple base-plus-overlay repo commands.
#
# These commands intentionally operate only on repos that already exist
# locally. Missing overlays are a pull/update concern; status, diff, fetch, and
# push should stay fast and should not unexpectedly clone new repos.

_repo_simple_header() {
  local op="$1" kind="$2" name="$3"

  case "$op:$kind" in
    fetch:base) _header "==> Fetching dotfiles..." ;;
    fetch:overlay) _header "==> Fetching $name dotfiles..." ;;
    push:base) _header "==> Pushing dotfiles..." ;;
    push:overlay) _header "==> Pushing $name dotfiles..." ;;
    diff:base | status:base) _header "==> dotfiles" ;;
    diff:overlay | status:overlay)
      echo ""
      _header "==> $name dotfiles"
      ;;
  esac
}

_repo_fetch_one() {
  local kind="$1" name="$2" path="$3"
  shift 4
  _repo_simple_header fetch "$kind" "$name"
  _repo_git_fetch "$kind" "$path" "$@"
}

_repo_push_one() {
  local kind="$1" name="$2" path="$3"
  shift 4
  _repo_simple_header push "$kind" "$name"
  if _repo_git "$kind" "$path" push "$@"; then
    return 0
  fi

  # A failed base push should keep the command's existing hard-fail behavior,
  # but overlay pushes have historically warned and continued so one stale
  # overlay cannot block publishing the base repo.
  [[ "$kind" == "base" ]] && return 1
  _warn "  warning: $name dotfiles push failed"
}

_repo_diff_one() {
  local kind="$1" name="$2" path="$3"
  shift 4
  _repo_simple_header diff "$kind" "$name"
  _repo_git "$kind" "$path" diff "$@"
}

_repo_status_one() {
  local kind="$1" name="$2" path="$3"
  shift 4
  _repo_simple_header status "$kind" "$name"
  _repo_git "$kind" "$path" status "$@"
}

_repo_fetch_all() {
  _repo_each_existing _repo_fetch_one "$@"
}

_repo_push_all() {
  _normalize_filtered
  _repo_each_existing _repo_push_one "$@"
}

_repo_diff_all() {
  _normalize_filtered
  _repo_each_existing _repo_diff_one "$@"
}

_repo_status_all() {
  _normalize_filtered
  _repo_each_existing _repo_status_one "$@"
}
