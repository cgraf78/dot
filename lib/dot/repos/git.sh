# shellcheck shell=bash
# Repo-set iteration and Git invocation helpers.
#
# The base client may use a separate Git directory with $HOME as its work tree
# or an ordinary checkout rooted at $HOME, while overlays are ordinary Git
# repositories. Keep topology dispatch centralized so higher-level operations
# can work with repo records instead of reimplementing those command shapes.

# Run a callback for every repo that already exists locally: the base client
# repo first, followed by cloned overlays in discovery order. Missing overlays
# are deliberately skipped here; cloning is pull/update behavior, while simple
# commands like status, diff, fetch, and push should only operate on installed
# repos.
_repo_each_existing() {
  local callback="$1"
  shift

  if _base_repo_exists; then
    "$callback" base dotfiles "$HOME" "" "$@" || return $?
  fi

  local entry name path url sync
  for entry in "${OVERLAYS[@]+"${OVERLAYS[@]}"}"; do
    IFS='|' read -r name path url _ _ sync <<<"$entry"
    sync="${sync:-git}"
    [[ "$sync" == "git" ]] || continue
    _overlay_is_worktree "$path" || continue
    "$callback" overlay "$name" "$path" "$url" "$@" || return $?
  done
}

# Execute git for a repo record emitted by _repo_each_existing. Keeping this as
# a helper avoids scattering base-topology dispatch beside `git -C` overlay
# commands.
_repo_git() {
  local kind="$1" path="$2"
  shift 2

  case "$kind" in
    base)
      # shellcheck disable=SC2086  # _base_git is intentionally word-split.
      _base_git "$@"
      ;;
    overlay)
      git -C "$path" "$@"
      ;;
    *)
      return 2
      ;;
  esac
}
