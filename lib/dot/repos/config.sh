# shellcheck shell=bash
# Repository identity and model configuration.
#
# These helpers are kept separate from pull/command code because they encode
# durable repo policy. The standalone engine never rewrites a client remote;
# the exact origin selected during initialization remains authoritative.

_repo_has_upstream() {
  "$@" rev-parse --abbrev-ref --symbolic-full-name '@{u}' >/dev/null 2>&1
}

_overlay_is_worktree() {
  local path="$1" checkout_root git_root
  [[ -d "$path" ]] && [[ -d "$path/.git" || -f "$path/.git" ]] || return 1
  checkout_root=$(cd -P -- "$path" 2>/dev/null && pwd -P) || return 1
  git_root=$(git -C "$path" rev-parse --show-toplevel 2>/dev/null) || return 1
  git_root=$(cd -P -- "$git_root" 2>/dev/null && pwd -P) || return 1
  [[ "$checkout_root" == "$git_root" ]]
}

# Resolve local relative URLs from HOME before both clone and validation. Git
# rewrites a relative source to a cwd-dependent absolute origin during clone;
# making the base explicit keeps later updates stable from every working
# directory. Colon-bearing SSH/scp URLs, schemes, absolute paths, and Windows
# drive paths keep their configured spelling.
_overlay_effective_url() {
  local url="$1"
  case "$url" in
    \~) REPLY="$HOME" ;;
    \~/*) REPLY="$HOME/${url#\~/}" ;;
    /* | [A-Za-z]:[\\/]* | *:*) REPLY="$url" ;;
    *) REPLY="$HOME/$url" ;;
  esac
}

# Compare the one authoritative origin URL with the configured spelling. REPLY
# contains the recorded URL, or a diagnostic placeholder when origin is absent
# or ambiguous.
_overlay_origin_matches() {
  local path="$1" expected="$2"
  local -a urls=()
  mapfile -t urls < <(git -C "$path" config --get-all remote.origin.url 2>/dev/null)

  case "${#urls[@]}" in
    0)
      REPLY="<missing>"
      return 1
      ;;
    1)
      REPLY="${urls[0]}"
      [[ "$REPLY" == "$expected" ]]
      ;;
    *)
      REPLY="<multiple origin URLs>"
      return 1
      ;;
  esac
}

_overlay_checkout_matches() {
  local path="$1" url="$2"
  _overlay_is_worktree "$path" || {
    REPLY="<not a Git worktree>"
    return 1
  }
  _overlay_effective_url "$url"
  local expected="$REPLY"
  _overlay_origin_matches "$path" "$expected"
}

# Preserve only the repository properties required by the model. A client
# worktree rooted at HOME must not recursively scan every untracked file, and
# fsmonitor must not watch the whole home directory. Pull strategy, filters,
# and remote transport remain client-owned Git policy.
_ensure_repo_config() {
  if _base_repo_exists; then
    if [[ $(_base_git config --bool core.fsmonitor 2>/dev/null || true) != false ]]; then
      _base_git config core.fsmonitor false 2>/dev/null || true
    fi
    if [[ $(_base_git config status.showUntrackedFiles 2>/dev/null || true) != no ]]; then
      _base_git config status.showUntrackedFiles no 2>/dev/null || true
    fi
  fi
}
