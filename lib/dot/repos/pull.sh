# shellcheck shell=bash
# Pull orchestration for the base repo and pull-eligible overlays.
#
# Pull is update's most sensitive repo operation: it may repair untracked-file
# adoption conflicts, clone missing overlays, and render live progress. Keep
# those rules together so update/cron behavior can be reviewed in one place.

_backup_dir() {
  local root="$HOME/.dot-backup/pull"
  mkdir -p "$root"
  local backup
  backup="$root/$(date +%Y%m%d%H%M%S)"
  if ! mkdir "$backup" 2>/dev/null; then
    backup=$(mktemp -d "$backup.XXXXXX" 2>/dev/null) || {
      REPLY=""
      return 1
    }
  fi
  REPLY="$backup"
}

_pull_conflicts_from_log() {
  local log="$1"
  awk '
    /untracked working tree files would be overwritten by/ {
      in_conflicts = 1
      next
    }
    in_conflicts && /^[[:space:]]+[^[:space:]]/ {
      sub(/^[[:space:]]+/, "")
      print
      next
    }
    in_conflicts { exit }
  ' "$log"
}

_backup_pull_conflicts() {
  local log="$1"
  local root="${2:-$HOME}"
  local files=""
  files=$(_pull_conflicts_from_log "$log") || true
  [[ -n "$files" ]] || return 1

  local backup=""
  if ! _backup_dir; then
    return 1
  fi
  backup="$REPLY"

  local file count=0
  while IFS= read -r file; do
    [[ -n "$file" ]] || continue
    if [[ -e "$root/$file" || -L "$root/$file" ]]; then
      mkdir -p "$backup/$(dirname "$file")"
      mv "$root/$file" "$backup/$file"
      ((count++)) || true
    fi
  done <<<"$files"

  if [[ "$count" -eq 0 ]]; then
    rmdir "$backup" 2>/dev/null || true
    return 1
  fi

  _warn "  backed up $count conflicting untracked files to $backup"
  REPLY="$backup"
  return 0
}

# Run a git pull, appending --quiet in cron mode.
# Remaining args: the full git pull command.
#
# Force C locale: the conflict-backup detector (_pull_conflicts_from_log) and the
# quiet-output filter both match literal English git messages ("untracked working
# tree files would be overwritten by", "Already up to date.", "Current branch ...
# is up to date."). git localizes these under a non-English LC_*, which would
# silently defeat the self-healing untracked-conflict backup/retry. LC_ALL is set
# (not LC_MESSAGES) so it wins even when the environment already exports LC_ALL.
_pull_cmd() {
  if [[ "$DOT_QUIET" -eq 1 ]]; then
    LC_ALL=C "$@" --quiet
  else
    LC_ALL=C "$@"
  fi
}

# Generic pull with optional logging, conflict backup, and retry.
# $1 = backup root for conflict resolution
# Remaining args: the full git pull command to run.
_pull_repo() {
  local backup_root="$1"
  shift
  local log=""
  if ! _logfile_create; then
    _pull_cmd "$@"
    return $?
  fi
  log="$REPLY"

  local rc=0
  _run_to_log_with_ticks "$log" _pull_cmd "$@" || rc=$?

  if [[ "$rc" -ne 0 ]] && _backup_pull_conflicts "$log" "$backup_root"; then
    : >"$log"
    rc=0
    _run_to_log_with_ticks "$log" _pull_cmd "$@" || rc=$?
  fi

  if [[ "$DOT_QUIET" -ne 1 && -s "$log" && ("${DOT_VERBOSE:-0}" -eq 1 || "$rc" -ne 0) ]]; then
    local visible_log
    visible_log=$(sed \
      -e '/^Already up to date\.$/d' \
      -e '/^Current branch .* is up to date\.$/d' \
      "$log")
    [[ -z "$visible_log" ]] || _log_dim "$visible_log"
  fi

  _dot_cleanup_remove_path "$log" || true
  return "$rc"
}

_repo_head() {
  "$@" rev-parse --verify HEAD 2>/dev/null || true
}

_repo_candidate_adapter_allowed() {
  local ref=$1 path=$2 mode=$3
  shift 3
  [[ $path == .local/bin/dot && $mode == 100755 ]] || return 1
  "$@" show "$ref:$path" 2>/dev/null |
    _dot_stdin_matches_file "$DOT_SOURCE_ROOT/support/client-launcher.sh"
}

# Validate a fetched candidate before any checkout, rebase, or overlay link can
# expose its tree beneath HOME. Base paths map directly; overlay paths map only
# the repository's home/ subtree.
_repo_validate_candidate_tree() {
  local kind=$1 ref=$2 entry header mode type oid path relative count=0
  local raw valid=1 reserved_roots reserved_roots_after
  shift 2

  _dot_reserved_roots_snapshot || return 1
  reserved_roots=$REPLY
  _dot_cleanup_mktemp || return 1
  raw=$REPLY
  if ! "$@" ls-tree -rz --full-tree "$ref" >"$raw"; then
    _dot_cleanup_remove_path "$raw" || true
    return 1
  fi
  while IFS= read -r -d '' entry; do
    [[ $entry == *$'\t'* ]] || {
      valid=0
      break
    }
    header=${entry%%$'\t'*}
    path=${entry#*$'\t'}
    read -r mode type oid <<<"$header"
    [[ $type == blob && $mode =~ ^(100644|100755|120000)$ &&
      $oid =~ ^[0-9a-fA-F]{40,64}$ ]] || {
      valid=0
      break
    }
    _dot_init_safe_relative_path "$path" || {
      valid=0
      break
    }
    if [[ $kind == overlay ]]; then
      case $path in
        home/*) relative=${path#home/} ;;
        home)
          valid=0
          break
          ;;
        *) continue ;;
      esac
    else
      relative=$path
    fi
    if _dot_candidate_path_is_reserved_from_roots \
      "$HOME/$relative" "$reserved_roots" &&
      ! _repo_candidate_adapter_allowed "$ref" "$path" "$mode" "$@"; then
      _warn "  warning: candidate repository owns reserved path: $relative"
      valid=0
      break
    fi
    count=$((count + 1))
    [[ $count -le 100000 ]] || {
      valid=0
      break
    }
  done <"$raw"
  if [[ $valid -eq 1 ]]; then
    if ! _dot_reserved_roots_snapshot; then
      valid=0
    else
      reserved_roots_after=$REPLY
      [[ $reserved_roots_after == "$reserved_roots" ]] || valid=0
    fi
  fi
  _dot_cleanup_remove_path "$raw" || return 1
  [[ $valid -eq 1 ]]
}

_repo_prepare_base_upstream() {
  local upstream remote
  upstream=$(_base_git rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null) ||
    return 1
  remote=${upstream%%/*}
  [[ -n $remote && $remote != "$upstream" ]] || return 1
  _base_git fetch --quiet "$remote" || return 2
  _repo_validate_candidate_tree base "$upstream" _base_git || return 3
  REPLY=$upstream
}

_repo_prepare_overlay_upstream() {
  local path=$1 quiet_errors=${2:-false} upstream remote
  upstream=$(git -C "$path" rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null) ||
    return 1
  remote=${upstream%%/*}
  [[ -n $remote && $remote != "$upstream" ]] || return 1
  if [[ $quiet_errors == true ]]; then
    git -C "$path" fetch --quiet "$remote" >/dev/null 2>&1 || return 2
  else
    git -C "$path" fetch --quiet "$remote" || return 2
  fi
  _repo_validate_candidate_tree overlay "$upstream" git -C "$path" || return 3
  REPLY=$upstream
}

_repo_apply_path_parent_modes() {
  local root=$1 relative=$2 parent current component
  local -a components=()

  parent=${relative%/*}
  [[ $parent != "$relative" ]] || return 0
  current=$root
  IFS=/ read -r -a components <<<"$parent"
  for component in "${components[@]}"; do
    current=$current/$component
    [[ -d $current && ! -L $current ]] || return 1
    _dot_apply_umask_ceiling "$current" || return 1
  done
}

_repo_normalize_updated_path() {
  local root=$1 kind=$2 relative=$3 mode=$4 oid=$5 after=$6
  local target prepared prepared_identity final_mode current_oid ceiling
  shift 6

  _dot_init_safe_relative_path "$relative" || return 1
  _repo_apply_path_parent_modes "$root" "$relative" || return 1
  [[ $mode == 120000 ]] && return 0
  [[ $mode == 100644 || $mode == 100755 ]] || return 1
  if [[ $kind == base ]] && _overlay_active_link_matches "$relative"; then
    return 0
  fi
  target=$root/$relative
  [[ -f $target && ! -L $target ]] || return 1
  "$@" diff --cached --quiet "$after" -- "$relative" || return 1
  "$@" diff --quiet "$after" -- "$relative" || return 1
  current_oid=$("$@" hash-object --no-filters -- "$target" 2>/dev/null) || return 1
  [[ $current_oid == "$oid" ]] || return 1

  # The checkout may have been born group-writable under a default ACL. Build
  # an exact replacement from the captured commit, then atomically replace the
  # exposed inode so a writer that opened the unsafe generation cannot retain
  # authority after the mode clamp.
  _dot_sibling_tmp_for "$target" || return 1
  prepared=$REPLY
  _dot_cleanup_register_path "$prepared"
  "$@" show "$after:$relative" >"$prepared" || return 1
  current_oid=$("$@" hash-object --no-filters -- "$prepared" 2>/dev/null) || return 1
  [[ $current_oid == "$oid" ]] || return 1

  if [[ $mode == 100755 ]]; then
    ceiling=0777
  else
    ceiling=0666
  fi
  _dot_apply_umask_ceiling "$target" "$ceiling" || return 1
  final_mode=$(stat -c '%a' "$target" 2>/dev/null || stat -f '%Lp' "$target" 2>/dev/null) ||
    return 1
  chmod "$final_mode" "$prepared" || return 1
  prepared_identity=$(_dot_path_identity "$prepared") || return 1
  current_oid=$("$@" hash-object --no-filters -- "$target" 2>/dev/null) || return 1
  [[ $current_oid == "$oid" ]] || return 1
  _dot_publish_prepared_regular "$prepared" "$target" || return 1
  [[ $(_dot_path_identity "$target" 2>/dev/null || true) == "$prepared_identity" ]] ||
    return 1
  _dot_cleanup_unregister_path "$prepared"
  current_oid=$("$@" hash-object --no-filters -- "$target" 2>/dev/null) || return 1
  [[ $current_oid == "$oid" ]]
}

_repo_normalize_updated_paths() {
  local root=$1 kind=$2 before=$3 after=$4
  local inventory header relative old_mode new_mode old_oid new_oid status extra
  local valid=1
  shift 4

  [[ $(_repo_head "$@") == "$after" ]] || return 1
  _dot_cleanup_mktemp || return 1
  inventory=$REPLY
  if ! "$@" diff --raw --no-renames --abbrev=64 -z "$before" "$after" -- >"$inventory"; then
    _dot_cleanup_remove_path "$inventory" || true
    return 1
  fi
  while IFS= read -r -d '' header; do
    IFS= read -r -d '' relative || {
      valid=0
      break
    }
    [[ $header == :* ]] || {
      valid=0
      break
    }
    read -r old_mode new_mode old_oid new_oid status extra <<<"${header#:}"
    [[ -z $extra && $old_mode =~ ^(000000|100644|100755|120000)$ &&
      $new_mode =~ ^(000000|100644|100755|120000)$ &&
      $old_oid =~ ^[0-9a-fA-F]+$ && $new_oid =~ ^[0-9a-fA-F]+$ &&
      (${#old_oid} -eq 40 || ${#old_oid} -eq 64) &&
      ${#new_oid} -eq ${#old_oid} && $status =~ ^[AMDT]$ ]] || {
      valid=0
      break
    }
    [[ $new_mode != 000000 ]] || continue
    _repo_normalize_updated_path \
      "$root" "$kind" "$relative" "$new_mode" "$new_oid" "$after" "$@" || {
      valid=0
      break
    }
  done <"$inventory"
  if [[ $valid -eq 1 && $(_repo_head "$@") != "$after" ]]; then
    valid=0
  fi
  _dot_cleanup_remove_path "$inventory" || return 1
  [[ $valid -eq 1 ]]
}

# Pull the base repo. Reports the outcome through the REPLY_STATUS enum
# (changed|current|skipped|failed|blocked) rather than a display string, so callers
# branch on a stable key instead of parsing prose.
_pull_base() {
  REPLY_STATUS=""
  if ! _repo_has_upstream _base_git; then
    REPLY_STATUS="skipped"
    return 0
  fi
  local head_before head_after
  head_before=$(_repo_head _base_git)
  local pull_rc=0
  local upstream
  _repo_prepare_base_upstream || {
    REPLY_STATUS=failed
    return 1
  }
  upstream=$REPLY
  _pull_repo "$HOME" _base_git rebase --autostash "$upstream" "$@" || pull_rc=$?
  if [[ "$pull_rc" -eq 0 ]]; then
    head_after=$(_repo_head _base_git)
    if [[ -n "$head_before" && -n "$head_after" && "$head_before" != "$head_after" ]]; then
      if ! _repo_normalize_updated_paths "$HOME" base "$head_before" "$head_after" _base_git; then
        REPLY_STATUS="failed"
        return 1
      fi
      REPLY_STATUS="changed"
    else
      REPLY_STATUS="current"
    fi
    return 0
  fi
  REPLY_STATUS="failed"
  return 1
}

# Explain an origin mismatch and provide the command appropriate to its state.
# Adoption is deliberately explicit: update never rewrites this ownership
# boundary itself.
_overlay_origin_mismatch() {
  local name="$1" path="$2" expected="$3" actual="$4"
  local adopt_command
  case "$actual" in
    "<missing>")
      printf -v adopt_command 'git -C %q remote add origin %q' "$path" "$expected"
      ;;
    "<multiple origin URLs>")
      printf -v adopt_command 'git -C %q config --replace-all remote.origin.url %q' \
        "$path" "$expected"
      ;;
    *)
      printf -v adopt_command 'git -C %q remote set-url origin %q' "$path" "$expected"
      ;;
  esac

  if [[ "${DOT_UI_TOTAL:-0}" -gt 0 ]]; then
    _ui_status warning "$name overlay origin mismatch: expected $expected, found $actual"
    _ui_status warning "verify the checkout, then adopt it with: $adopt_command"
  else
    _warn "  warning: $name overlay origin does not match its configured URL"
    _warn "    expected: $expected"
    _warn "    found:    $actual"
    _warn "    verify the checkout, then adopt it with: $adopt_command"
  fi
}

_pull_overlay_active() {
  local _name="$1" path="$2" url="$3" optional="$4"
  if _overlay_is_worktree "$path"; then
    return 0
  fi
  [[ -n "$url" ]]
}

_pull_overlay_count() {
  local entry count=0
  for entry in "${OVERLAYS[@]+"${OVERLAYS[@]}"}"; do
    local name path url _conf optional sync
    IFS='|' read -r name path url _conf optional sync <<<"$entry"
    sync="${sync:-git}"
    [[ "$sync" == "git" ]] || continue
    if _pull_overlay_active "$name" "$path" "$url" "$optional"; then
      count=$((count + 1))
    fi
  done
  printf '%s' "$count"
}

_dot_progress_detail() {
  local label="$1" done="$2" total="$3"
  [[ "$total" -gt 0 ]] || return 0
  _ui_progress_detail_with_label "$label" "$done" "$total"
}

_dot_maybe_stage_progress() {
  local label="$1" done="$2" total="$3"
  [[ "${DOT_UI_TOTAL:-0}" -gt 0 ]] || return 0
  [[ "${DOT_VERBOSE:-0}" -eq 0 ]] || return 0
  _ui_stage_update "$(_dot_progress_detail "$label" "$done" "$total")"
}

_repo_cloned_overlay_path_modes() {
  local root=$1 relative=$2 target executable=0

  _dot_init_safe_relative_path "$relative" || return 1
  _repo_apply_path_parent_modes "$root" "$relative" || return 1

  target=$root/$relative
  [[ -L $target ]] && return 0
  [[ -f $target && ! -L $target ]] || return 1
  [[ -x $target ]] && executable=1
  if [[ $executable -eq 1 ]]; then
    _dot_apply_tracked_file_mode "$target" 100755
  else
    _dot_apply_tracked_file_mode "$target" 100644
  fi
}

_repo_normalize_cloned_overlay_modes() {
  local root=$1 relative inventory valid=1

  # The staged checkout is not yet public, so every tracked byte is the exact
  # validated clone generation. Reapply the retained umask to its worktree
  # once; this removes default-ACL grants without a recurring warm-path scan.
  chmod 0700 "$root" || return 1
  _dot_cleanup_mktemp || return 1
  inventory=$REPLY
  if ! git -C "$root" ls-files -z >"$inventory"; then
    _dot_cleanup_remove_path "$inventory" || true
    return 1
  fi
  while IFS= read -r -d '' relative; do
    _repo_cloned_overlay_path_modes "$root" "$relative" || {
      valid=0
      break
    }
  done <"$inventory"
  _dot_cleanup_remove_path "$inventory" || return 1
  [[ $valid -eq 1 ]]
}

_repo_clone_overlay_staged() {
  local url=$1 path=$2 parent name stage
  parent=${path%/*}
  name=${path##*/}
  [[ -n $parent && $parent != "$path" ]] || return 1
  mkdir -p "$parent" || return 1
  stage=$(mktemp -d "$parent/.$name.clone.XXXXXX") || return 1
  rmdir "$stage" || return 1
  _dot_cleanup_register_path "$stage"
  if ! git clone --quiet -- "$url" "$stage"; then
    _dot_cleanup_remove_path "$stage" || true
    return 1
  fi
  if ! _repo_validate_candidate_tree overlay HEAD git -C "$stage"; then
    _dot_cleanup_remove_path "$stage" || true
    return 1
  fi
  if ! _repo_normalize_cloned_overlay_modes "$stage"; then
    _dot_cleanup_remove_path "$stage" || true
    return 1
  fi
  if ! _dot_move_noreplace "$stage" "$path"; then
    _dot_cleanup_remove_path "$stage" || true
    return 1
  fi
  _dot_cleanup_unregister_path "$stage"
}

# Pull a single overlay repo, cloning it first if missing.
# $1 = name, $2 = path, $3 = url, $4 = optional
# Remaining args are forwarded to git pull.
# Pull (or clone) a single overlay. Reports the outcome through REPLY_STATUS
# (changed|cloned|current|skipped|failed, or empty when the overlay is a
# no-op) so the caller tallies on a stable key, not on parsed display text.
_pull_overlay() {
  local name="$1" path="$2" url="$3" optional="$4"
  shift 4
  _overlay_effective_url "$url"
  url="$REPLY"
  REPLY_STATUS=""
  if [[ ! -e "$path" && ! -L "$path" ]]; then
    [[ -n "$url" ]] || return 0
    if [[ "$optional" == "true" ]]; then
      if _repo_clone_overlay_staged "$url" "$path" >/dev/null 2>&1; then
        REPLY_STATUS="cloned"
      fi
      return 0
    fi
    if [[ "${DOT_UI_TOTAL:-0}" -gt 0 ]]; then
      [[ "${DOT_VERBOSE:-0}" -eq 1 ]] && _ui_status running "$name dotfiles: cloning"
    else
      _log_header "==> Cloning $name dotfiles..."
    fi
    if ! _repo_clone_overlay_staged "$url" "$path" >/dev/null 2>&1; then
      if [[ "${DOT_UI_TOTAL:-0}" -gt 0 ]]; then
        _ui_status warning "$name dotfiles clone failed"
      else
        _warn "  warning: $name dotfiles clone failed"
      fi
      REPLY_STATUS="failed"
      return 0
    fi
    [[ "${DOT_UI_TOTAL:-0}" -gt 0 && "${DOT_VERBOSE:-0}" -eq 1 ]] && _ui_status changed "$name dotfiles cloned"
    REPLY_STATUS="cloned"
    return 0
  fi

  # Do not replace existing paths during unattended updates. A linked worktree
  # has a `.git` file, and any other path may contain user-owned data.
  if ! _overlay_is_worktree "$path"; then
    if [[ "${DOT_UI_TOTAL:-0}" -gt 0 ]]; then
      _ui_status warning "$name overlay path exists but is not a Git worktree"
    else
      _warn "  warning: $name overlay path exists but is not a Git worktree; leaving it untouched: $path"
    fi
    REPLY_STATUS="failed"
    return 0
  fi

  local actual_origin
  if ! _overlay_origin_matches "$path" "$url"; then
    actual_origin="$REPLY"
    _overlay_origin_mismatch "$name" "$path" "$url" "$actual_origin"
    REPLY_STATUS="failed"
    return 0
  fi

  if ! _repo_has_upstream git -C "$path"; then
    [[ "${DOT_UI_TOTAL:-0}" -gt 0 && "${DOT_VERBOSE:-0}" -eq 1 ]] &&
      _ui_status skipped "$name dotfiles pull skipped (no upstream)"
    REPLY_STATUS="skipped"
    return 0
  fi
  local upstream
  local prepare_rc=0
  _repo_prepare_overlay_upstream "$path" "$optional" || prepare_rc=$?
  if [[ $prepare_rc -ne 0 ]]; then
    [[ "$optional" == true ]] && return 0
    if [[ $prepare_rc -eq 3 ]]; then
      _warn "  warning: $name overlay candidate failed reserved-path validation"
    elif [[ "${DOT_UI_TOTAL:-0}" -gt 0 ]]; then
      _ui_status warning "$name dotfiles pull failed"
    else
      _warn "  warning: $name dotfiles pull failed"
    fi
    REPLY_STATUS=failed
    return 0
  fi
  upstream=$REPLY

  if [[ "$optional" == "true" ]]; then
    local head_before head_after quiet_before quiet_was_set=0
    head_before=$(_repo_head git -C "$path")
    if [[ -n "${DOT_QUIET+x}" ]]; then
      quiet_was_set=1
      quiet_before="$DOT_QUIET"
    fi
    DOT_QUIET=1
    if ! _pull_repo "$path" git -C "$path" rebase --autostash "$upstream" "$@"; then
      if [[ "$quiet_was_set" -eq 1 ]]; then
        DOT_QUIET="$quiet_before"
      else
        unset DOT_QUIET
      fi
      return 0
    fi
    if [[ "$quiet_was_set" -eq 1 ]]; then
      DOT_QUIET="$quiet_before"
    else
      unset DOT_QUIET
    fi
    head_after=$(_repo_head git -C "$path")
    if [[ -n "$head_before" && -n "$head_after" && "$head_before" != "$head_after" ]]; then
      if ! _repo_normalize_updated_paths \
        "$path" overlay "$head_before" "$head_after" git -C "$path"; then
        REPLY_STATUS="failed"
        return 0
      fi
      REPLY_STATUS="changed"
    else
      REPLY_STATUS="current"
    fi
    return 0
  fi

  if [[ "${DOT_UI_TOTAL:-0}" -gt 0 ]]; then
    [[ "${DOT_VERBOSE:-0}" -eq 1 ]] && _ui_status running "$name dotfiles: pulling"
  else
    _log_header "==> Pulling $name dotfiles..."
  fi
  local head_before head_after
  head_before=$(_repo_head git -C "$path")
  if ! _pull_repo "$path" git -C "$path" rebase --autostash "$upstream" "$@"; then
    if [[ "${DOT_UI_TOTAL:-0}" -gt 0 ]]; then
      _ui_status warning "$name dotfiles pull failed"
    else
      _warn "  warning: $name dotfiles pull failed"
    fi
    REPLY_STATUS="failed"
    return 0
  fi
  head_after=$(_repo_head git -C "$path")
  if [[ -n "$head_before" && -n "$head_after" && "$head_before" != "$head_after" ]]; then
    if ! _repo_normalize_updated_paths \
      "$path" overlay "$head_before" "$head_after" git -C "$path"; then
      _warn "  warning: $name dotfiles mode normalization failed"
      REPLY_STATUS="failed"
      return 0
    fi
    [[ "${DOT_UI_TOTAL:-0}" -gt 0 && "${DOT_VERBOSE:-0}" -eq 1 ]] && _ui_status changed "$name dotfiles updated"
    REPLY_STATUS="changed"
  else
    [[ "${DOT_UI_TOTAL:-0}" -gt 0 && "${DOT_VERBOSE:-0}" -eq 1 ]] && _ui_status ok "$name dotfiles current"
    REPLY_STATUS="current"
  fi
  return 0
}

# Active Git overlays are separate synchronization units, so their pulls can
# overlap within the bound from _dot_update_jobs. A worker cannot return shell
# state to its parent, so each writes indexed log, status, and exit-code files in
# parent-owned scratch; the parent then replays declaration order for stable UI
# and tallies the structured statuses. If scratch allocation is unavailable, the
# serial path preserves the same pull and status behavior without parallel state.
_pull_overlay_result_prefix() {
  local _dir="$1" _idx="$2"
  printf '%s/%03d' "$_dir" "$_idx"
}

_pull_overlay_capture() {
  local _idx="$1" _result_dir="$2" name="$3" path="$4" url="$5" optional="$6"
  shift 6
  local _prefix _rc=0
  # Capture-worker helpers may allocate their own logs. Contain them in the
  # parent-owned result root so forced worker termination cannot leak storage.
  local TMPDIR="$_result_dir"
  export TMPDIR
  _dot_cleanup_prepare_subshell
  _prefix="$(_pull_overlay_result_prefix "$_result_dir" "$_idx")"
  REPLY_STATUS=""
  _pull_overlay "$name" "$path" "$url" "$optional" "$@" >"$_prefix.log" 2>&1 || _rc=$?
  printf '%s' "$_rc" >"$_prefix.rc"
  printf '%s' "${REPLY_STATUS:-}" >"$_prefix.status"
}

_pull_overlay_drain_workers() {
  local _result_dir="$1" _pid _log
  shift
  for _pid in "$@"; do
    wait "$_pid" 2>/dev/null || true
    _dot_cleanup_unregister_pid "$_pid"
  done
  # Preserve diagnostics already captured by siblings when a coordinator
  # worker itself fails before the normal ordered rendering pass.
  for _log in "$_result_dir"/*.log; do
    [[ -f "$_log" ]] || continue
    [[ ! -s "$_log" ]] || cat "$_log"
  done
  _dot_cleanup_remove_path "$_result_dir" || true
}

_pull_overlay_record_status() {
  local name="$1" status="$2"
  [[ -n "$status" ]] || return 0

  # The terse "<name> <status>" summary is display text, rebuilt here at the
  # boundary; the tally below keys off the structured status, not this string.
  _summaries+=("$name $status")
  case "$status" in
    failed)
      DOT_PULL_OVERLAY_FAILED=$((DOT_PULL_OVERLAY_FAILED + 1))
      ;;
    changed)
      DOT_PULL_OVERLAY_CHANGED=$((DOT_PULL_OVERLAY_CHANGED + 1))
      DOT_PULL_OVERLAY_CHANGED_ITEMS+="${name} dotfiles updated"$'\n'
      ;;
    cloned)
      DOT_PULL_OVERLAY_CHANGED=$((DOT_PULL_OVERLAY_CHANGED + 1))
      DOT_PULL_OVERLAY_CHANGED_ITEMS+="${name} dotfiles cloned"$'\n'
      ;;
    skipped)
      DOT_PULL_OVERLAY_SKIPPED=$((DOT_PULL_OVERLAY_SKIPPED + 1))
      ;;
    current)
      DOT_PULL_OVERLAY_CURRENT=$((DOT_PULL_OVERLAY_CURRENT + 1))
      ;;
  esac
}

_pull_overlays_serial() {
  local entry name path url _conf optional sync status
  for entry in "${_active_entries[@]+"${_active_entries[@]}"}"; do
    IFS='|' read -r name path url _conf optional sync <<<"$entry"
    _done=$((_done + 1))
    _dot_maybe_stage_progress "$name" "$_done" "$_total"
    _pull_overlay "$name" "$path" "$url" "$optional" "$@"
    status="${REPLY_STATUS:-}"
    _pull_overlay_record_status "$name" "$status"
  done
}

_pull_overlays() {
  local entry
  local _summaries=()
  local _done="${DOT_REPO_PROGRESS_DONE:-0}"
  local _total="${DOT_REPO_PROGRESS_TOTAL:-0}"
  DOT_PULL_OVERLAY_CURRENT=0
  DOT_PULL_OVERLAY_CHANGED=0
  DOT_PULL_OVERLAY_CHANGED_ITEMS=""
  DOT_PULL_OVERLAY_FAILED=0
  DOT_PULL_OVERLAY_SKIPPED=0
  local name path url _conf optional sync status
  local -a _active_entries=()
  for entry in "${OVERLAYS[@]+"${OVERLAYS[@]}"}"; do
    IFS='|' read -r name path url _conf optional sync <<<"$entry"
    sync="${sync:-git}"
    [[ "$sync" == "git" ]] || continue
    if ! _pull_overlay_active "$name" "$path" "$url" "$optional"; then
      continue
    fi
    _active_entries+=("$entry")
  done
  if ((${#_active_entries[@]} == 0)); then
    DOT_REPO_PROGRESS_DONE="$_done"
    REPLY=""
    return 0
  fi

  local _result_dir=""
  if ! _dot_cleanup_mktemp -d 2>/dev/null; then
    _pull_overlays_serial "$@"
    DOT_REPO_PROGRESS_DONE="$_done"
    REPLY=$(_join_comma "${_summaries[@]}")
    return 0
  fi
  _result_dir=$REPLY

  local _jobs _running=0 _idx=0 _pid
  local -a _pids=()
  _jobs="$(_dot_update_jobs)"

  for entry in "${_active_entries[@]+"${_active_entries[@]}"}"; do
    IFS='|' read -r name path url _conf optional sync <<<"$entry"
    _done=$((_done + 1))
    _dot_maybe_stage_progress "$name" "$_done" "$_total"
    _idx=$((_idx + 1))
    _dot_cleanup_begin_job_launch
    _pull_overlay_capture "$_idx" "$_result_dir" "$name" "$path" "$url" "$optional" "$@" \
      <&"$DOT_CLEANUP_LAUNCH_STDIN_FD" &
    _pid=$!
    _dot_cleanup_finish_job_launch "$_pid"
    _pids+=("$_pid")
    _running=$((_running + 1))
    if [[ "$_running" -ge "$_jobs" ]]; then
      _pid="${_pids[0]}"
      _pids=("${_pids[@]:1}")
      if ! wait "$_pid" 2>/dev/null; then
        _dot_cleanup_unregister_pid "$_pid"
        _pull_overlay_drain_workers "$_result_dir" "${_pids[@]}"
        return 1
      fi
      _dot_cleanup_unregister_pid "$_pid"
      _running=$((_running - 1))
    fi
  done

  while ((${#_pids[@]} > 0)); do
    _pid="${_pids[0]}"
    _pids=("${_pids[@]:1}")
    if ! wait "$_pid" 2>/dev/null; then
      _dot_cleanup_unregister_pid "$_pid"
      _pull_overlay_drain_workers "$_result_dir" "${_pids[@]}"
      return 1
    fi
    _dot_cleanup_unregister_pid "$_pid"
  done

  _idx=0
  for entry in "${_active_entries[@]+"${_active_entries[@]}"}"; do
    IFS='|' read -r name path url _conf optional sync <<<"$entry"
    _idx=$((_idx + 1))
    local _prefix _rc
    _prefix="$(_pull_overlay_result_prefix "$_result_dir" "$_idx")"
    [[ ! -s "$_prefix.log" ]] || cat "$_prefix.log"
    status="$(cat "$_prefix.status" 2>/dev/null || true)"
    _rc="$(cat "$_prefix.rc" 2>/dev/null || printf '1')"
    if [[ -z "$status" && "$_rc" -ne 0 ]]; then
      status=failed
    fi
    # An empty status means the overlay was a no-op (inactive/missing key);
    # leave it out of the tally and the summary entirely.
    _pull_overlay_record_status "$name" "$status"
  done
  _dot_cleanup_remove_path "$_result_dir" || true
  DOT_REPO_PROGRESS_DONE="$_done"
  REPLY=$(_join_comma "${_summaries[@]}")
}

# Pull all repos that update treats as part of the synchronized repo set: base
# first, then pull-eligible overlays. This owns only repository synchronization;
# `_dot_update_finalize` still handles dependency updates, overlay linking, merge
# hooks, and cron installation after the repo set is current.
_repo_pull_all() {
  _ensure_repo_config
  _normalize_filtered
  if ! _unstash_overlay_overrides; then
    return 1
  fi

  local _repo_total
  _repo_total=$((1 + $(_pull_overlay_count)))
  local _repo_detail="pulling repositories"
  if [[ "${DOT_VERBOSE:-0}" -eq 0 ]]; then
    _repo_detail="$(_dot_progress_detail "dotfiles" 1 "$_repo_total")"
  fi
  _ui_stage_start "Repos" "$_repo_detail"

  local _repo_status=ok
  local _repo_current=0
  local _repo_changed=0
  local _repo_failed=0
  local _repo_skipped=0
  local -a _repo_changed_items=()

  [[ "${DOT_VERBOSE:-0}" -eq 1 ]] && _ui_status running "dotfiles: pulling"
  REPLY_STATUS=""
  if ! _pull_base "$@"; then
    if [[ "$DOT_QUIET" -eq 1 && "${REPLY_STATUS:-}" != "blocked" ]]; then
      _warn "  warning: dotfiles pull failed"
    else
      [[ "${DOT_VERBOSE:-0}" -eq 1 ]] && _ui_status failed "dotfiles: pull failed"
      _repo_status=failed
      _repo_failed=$((_repo_failed + 1))
    fi
  else
    if [[ "${REPLY_STATUS:-}" == "skipped" ]]; then
      [[ "${DOT_VERBOSE:-0}" -eq 1 ]] &&
        _ui_status skipped "dotfiles pull skipped (no upstream)"
      _repo_skipped=$((_repo_skipped + 1))
    elif [[ "${REPLY_STATUS:-}" == "changed" ]]; then
      [[ "${DOT_VERBOSE:-0}" -eq 1 ]] && _ui_status changed "dotfiles updated"
      _repo_changed=$((_repo_changed + 1))
      _repo_changed_items+=("dotfiles updated")
    else
      [[ "${DOT_VERBOSE:-0}" -eq 1 ]] && _ui_status ok "dotfiles current"
      _repo_current=$((_repo_current + 1))
    fi
  fi

  # _pull_overlays owns per-overlay result classification. Seed its progress
  # counters with the already-rendered base repo so non-verbose live progress
  # stays on the same dashboard row.
  # shellcheck disable=SC2034  # read by _pull_overlays while this stage is active.
  DOT_REPO_PROGRESS_DONE=1
  # shellcheck disable=SC2034  # read by _pull_overlays while this stage is active.
  DOT_REPO_PROGRESS_TOTAL="$_repo_total"
  local _pull_overlays_rc=0
  _pull_overlays "$@" || _pull_overlays_rc=$?
  _repo_current=$((_repo_current + ${DOT_PULL_OVERLAY_CURRENT:-0}))
  _repo_changed=$((_repo_changed + ${DOT_PULL_OVERLAY_CHANGED:-0}))
  _repo_failed=$((_repo_failed + ${DOT_PULL_OVERLAY_FAILED:-0}))
  _repo_skipped=$((_repo_skipped + ${DOT_PULL_OVERLAY_SKIPPED:-0}))
  if [[ "$_pull_overlays_rc" -ne 0 ]]; then
    _repo_failed=$((_repo_failed + 1))
  fi
  [[ "$_repo_failed" -eq 0 ]] || _repo_status=failed
  if [[ "$_repo_failed" -eq 0 && "$_repo_changed" -gt 0 ]]; then
    _repo_status=changed
  fi
  if [[ -n "${DOT_PULL_OVERLAY_CHANGED_ITEMS:-}" ]]; then
    local _changed_item
    while IFS= read -r _changed_item; do
      [[ -n "$_changed_item" ]] || continue
      _repo_changed_items+=("$_changed_item")
    done <<<"$DOT_PULL_OVERLAY_CHANGED_ITEMS"
  fi
  unset DOT_REPO_PROGRESS_DONE DOT_REPO_PROGRESS_TOTAL

  local _summary
  local _repo_parts=()
  [[ "$_repo_changed" -gt 0 ]] &&
    _repo_parts+=("$(_ui_count_phrase "$_repo_changed" repo repos) changed")
  [[ "$_repo_current" -gt 0 || ("$_repo_failed" -eq 0 && "$_repo_skipped" -eq 0) ]] &&
    _repo_parts+=("$(_ui_count_phrase "$_repo_current" repo repos) current")
  [[ "$_repo_failed" -gt 0 ]] &&
    _repo_parts+=("$(_ui_count_phrase "$_repo_failed" repo repos) failed")
  [[ "$_repo_skipped" -gt 0 ]] &&
    _repo_parts+=("$(_ui_count_phrase "$_repo_skipped" repo repos) skipped")
  _summary=$(_join_comma "${_repo_parts[@]}")
  _ui_stage_finish "$_repo_status" "$_summary"
  if [[ "${DOT_VERBOSE:-0}" -eq 0 ]]; then
    local _repo_item
    for _repo_item in "${_repo_changed_items[@]+"${_repo_changed_items[@]}"}"; do
      _ui_stage_note changed "$_repo_item"
    done
  fi
  [[ "$_repo_status" != "failed" ]]
}
