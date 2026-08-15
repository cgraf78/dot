# shellcheck shell=bash
# Dirty-worktree detection and normalization.
#
# `dot update --cron` depends on these helpers staying conservative: only
# content-clean mtime/filter noise and out-of-band writes that exactly match
# the configured upstream are repaired automatically. Real local edits must continue to
# block cron updates.

# Returns 0 (true) if there are uncommitted changes in any repo.
_is_worktree_dirty() {
  if _base_repo_exists; then
    if ! _base_git diff-index --quiet HEAD 2>/dev/null; then
      return 0
    fi
  fi
  local entry sync
  for entry in "${OVERLAYS[@]+"${OVERLAYS[@]}"}"; do
    local path
    IFS='|' read -r _ path _ _ _ sync <<<"$entry"
    sync="${sync:-git}"
    [[ "$sync" == "git" ]] || continue
    if _overlay_is_worktree "$path"; then
      if ! git -C "$path" diff-index --quiet HEAD 2>/dev/null; then
        return 0
      fi
    fi
  done
  return 1
}

# Revert only the currently-dirty tracked files, one at a time. The caller has
# already verified (via _dirty_files_match_ref) that every dirty file matches the
# remote, so scope the checkout to exactly that set instead of `checkout -- .`,
# which would revert anything else that happens to differ from HEAD.
_checkout_dirty_files() {
  local f
  while IFS= read -r f; do
    [[ -n "$f" ]] || continue
    "$@" checkout -- "$f" 2>/dev/null || true
  done < <("$@" diff-index --name-only HEAD 2>/dev/null)
}

# Attempt to resolve dirty worktrees caused by out-of-band writes that
# match what's on the remote. Returns 0 if all repos are clean after resolution.
_try_resolve_dirty() {
  local dirty=0 upstream remote
  if _base_repo_exists && ! _base_git diff-index --quiet HEAD 2>/dev/null; then
    upstream=$(_repo_configured_upstream _base_git) || upstream=''
    remote=${upstream%%/*}
    [[ -z $upstream ]] || _base_git fetch --quiet "$remote" 2>/dev/null || true
    if [[ -n $upstream ]] && _dirty_files_match_ref "$HOME" "$upstream" _base_git; then
      # shellcheck disable=SC2086  # _base_git is intentionally word-split
      _checkout_dirty_files _base_git
    else
      dirty=1
    fi
  fi
  local entry sync
  for entry in "${OVERLAYS[@]+"${OVERLAYS[@]}"}"; do
    local path
    IFS='|' read -r _ path _ _ _ sync <<<"$entry"
    sync="${sync:-git}"
    [[ "$sync" == "git" ]] || continue
    if _overlay_is_worktree "$path" &&
      ! git -C "$path" diff-index --quiet HEAD 2>/dev/null; then
      upstream=$(_repo_configured_upstream git -C "$path") || upstream=''
      remote=${upstream%%/*}
      [[ -z $upstream ]] || git -C "$path" fetch --quiet "$remote" 2>/dev/null || true
      if [[ -n $upstream ]] &&
        _dirty_files_match_ref "$path" "$upstream" git -C "$path"; then
        _checkout_dirty_files git -C "$path"
      else
        dirty=1
      fi
    fi
  done
  return "$dirty"
}

# Print the configured upstream for one repository command prefix.
_repo_configured_upstream() {
  local upstream remote
  upstream=$("$@" rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null) ||
    return 1
  remote=${upstream%%/*}
  [[ -n $remote && $remote != "$upstream" ]] || return 1
  printf '%s\n' "$upstream"
}

# Check if every dirty file in a repo matches content on its selected ref.
_dirty_files_match_ref() {
  local worktree="$1" remote_ref="$2"
  shift 2
  local dirty_files
  dirty_files=$("$@" diff-index --name-only HEAD 2>/dev/null) || return 1
  "$@" rev-parse --verify "$remote_ref" &>/dev/null || return 1
  while IFS= read -r f; do
    local work_hash remote_hash
    work_hash=$("$@" hash-object "$worktree/$f" 2>/dev/null) || return 1
    remote_hash=$("$@" rev-parse "$remote_ref:$f" 2>/dev/null) || return 1
    [[ "$work_hash" == "$remote_hash" ]] || return 1
  done <<<"$dirty_files"
  return 0
}

# Check if base repo dirty files match its configured upstream.
_dirty_files_match_remote() {
  local upstream
  upstream=$(_repo_configured_upstream _base_git) || return 1
  _dirty_files_match_ref "$HOME" "$upstream" _base_git
}

# Re-checkout the stat-dirty-but-content-clean (mtime-only) files in one repo.
# Only reverts files whose content matches HEAD, so real edits are left alone.
_normalize_dirty_files() {
  local dirty f
  dirty=$("$@" diff-files --name-only 2>/dev/null) || return 0
  [[ -n "$dirty" ]] || return 0
  while IFS= read -r f; do
    [[ -n "$f" ]] || continue
    if "$@" diff --quiet -- "$f" 2>/dev/null; then
      "$@" checkout -- "$f" 2>/dev/null || true
    fi
  done <<<"$dirty"
}

_normalize_repo() {
  local kind="$1" path="$2"
  case "$kind" in
    base)
      # shellcheck disable=SC2086 # _base_git is intentionally word-split.
      _normalize_dirty_files _base_git
      ;;
    overlay)
      _normalize_dirty_files git -C "$path"
      ;;
  esac
}

# Re-checkout files that are stat-dirty but content-clean across base + overlays.
_normalize_filtered() {
  local entry path record pid sync
  local -a records=() pids=()
  _base_repo_exists && records+=("base|")
  for entry in "${OVERLAYS[@]+"${OVERLAYS[@]}"}"; do
    IFS='|' read -r _ path _ _ _ sync <<<"$entry"
    sync="${sync:-git}"
    [[ "$sync" == "git" ]] || continue
    _overlay_is_worktree "$path" && records+=("overlay|$path")
  done

  # Keep the base-only path synchronous; there is no work to overlap and no
  # reason to pay for a subshell. Multiple repositories have independent Git
  # indexes, so their silent, best-effort normalization probes can safely run
  # together. Probe failures have always been ignored by _normalize_dirty_files.
  if ((${#records[@]} < 2)); then
    for record in "${records[@]+"${records[@]}"}"; do
      IFS='|' read -r entry path <<<"$record"
      _normalize_repo "$entry" "$path" || true
    done
    return 0
  fi

  for record in "${records[@]+"${records[@]}"}"; do
    IFS='|' read -r entry path <<<"$record"
    _dot_cleanup_begin_job_launch
    _normalize_repo "$entry" "$path" <&"$DOT_CLEANUP_LAUNCH_STDIN_FD" &
    pid=$!
    _dot_cleanup_finish_job_launch "$pid"
    pids+=("$pid")
  done
  for pid in "${pids[@]+"${pids[@]}"}"; do
    wait "$pid" 2>/dev/null || true
    _dot_cleanup_unregister_pid "$pid"
  done
}
