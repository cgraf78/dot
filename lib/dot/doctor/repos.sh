# shellcheck shell=bash
# Core client-repository health checks.

_dr_completed_identity_matches_home() {
  local marker line recorded_git_dir='' recorded_worktree=''

  dot_xdg_path state dot/init/completed || return 1
  marker=$REPLY
  [[ -f $marker && ! -L $marker ]] || return 1
  while IFS= read -r line || [[ -n $line ]]; do
    case $line in
      git_dir=*) recorded_git_dir=${line#git_dir=} ;;
      worktree=*) recorded_worktree=${line#worktree=} ;;
    esac
  done <"$marker"
  [[ $recorded_worktree == "$HOME" ]] || return 1
  [[ $recorded_git_dir == "$HOME/.git" ]]
}

_dr_is_client_checkout() {
  local root home_real root_real

  root=$(git -C "$HOME" rev-parse --show-toplevel 2>/dev/null) || return 1
  home_real=$(cd "$HOME" 2>/dev/null && pwd -P) || return 1
  root_real=$(cd "$root" 2>/dev/null && pwd -P) || return 1
  [[ $root_real == "$home_real" ]] || return 1
  _dr_completed_identity_matches_home ||
    [[ $(git -C "$HOME" config --local --get dot.clientRepository 2>/dev/null) == true ]]
}

_dr_check_base_repo() {
  local is_bare has_worktree resolved dirty_count head_ref upstream counts ahead behind

  _dr_section 'Client repository'
  if ! _base_repo_exists; then
    if _dr_is_client_checkout; then
      _dr_ok 'client checkout exists' "ordinary checkout rooted at \$HOME"
    else
      _dr_fail 'client repository is missing' 'run dot init REPOSITORY_URL'
    fi
    return 0
  fi
  _dr_ok 'client Git directory exists' "$(_dr_tilde "$DOT_CLIENT_GIT_DIR")"

  if [[ $DOT_BASE_TOPOLOGY == ordinary ]]; then
    # The typed selector already proved that this is the recorded HOME checkout;
    # its worktree identity is the shared `git -C HOME` show-toplevel check
    # below, not core.worktree (which ordinary repositories normally omit).
    _dr_ok 'ordinary client layout'
  else
    # shellcheck disable=SC2086 # Historical command prefix, intentionally split.
    is_bare=$(_base_git config --get core.bare 2>/dev/null || printf false)
    # shellcheck disable=SC2086 # Historical command prefix, intentionally split.
    has_worktree=$(_base_git config --get core.worktree 2>/dev/null || true)
    if [[ $is_bare == true ]]; then
      _dr_ok 'legacy bare client layout'
    elif [[ -n $has_worktree ]]; then
      _dr_ok 'explicit-worktree client layout' "$(_dr_tilde "$has_worktree")"
    else
      _dr_fail 'client Git directory has no worktree identity'
    fi
  fi

  # shellcheck disable=SC2086 # Historical command prefix, intentionally split.
  resolved=$(_base_git rev-parse --show-toplevel 2>/dev/null || true)
  if [[ $resolved == "$HOME" ]]; then
    _dr_ok "client worktree resolves to \$HOME"
  else
    _dr_fail 'client worktree mismatch' "expected $HOME, got ${resolved:-<missing>}"
  fi

  # shellcheck disable=SC2086 # Historical command prefix, intentionally split.
  dirty_count=$(_base_git status --porcelain 2>/dev/null | grep -cvE '^\?\?' || true)
  if [[ $dirty_count -eq 0 ]]; then
    _dr_ok 'no tracked client changes'
  else
    _dr_warn "$dirty_count tracked client change(s)" "run dot status to inspect"
  fi
  # shellcheck disable=SC2086 # Historical command prefix, intentionally split.
  head_ref=$(_base_git symbolic-ref --short HEAD 2>/dev/null || true)
  if [[ -n $head_ref ]]; then
    _dr_ok 'client HEAD on branch' "$head_ref"
  else
    _dr_warn 'client HEAD is detached'
  fi

  upstream=$(_base_git rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null || true)
  if [[ -z $upstream ]]; then
    _dr_warn 'client upstream is not configured'
    return 0
  fi
  counts=$(_base_git rev-list --left-right --count "HEAD...$upstream" 2>/dev/null || true)
  if [[ $counts == *$'\t'* ]]; then
    IFS=$'\t' read -r ahead behind <<<"$counts"
  else
    ahead='' behind=''
  fi
  if [[ $ahead =~ ^[0-9]+$ && $behind =~ ^[0-9]+$ ]]; then
    if [[ $ahead -eq 0 && $behind -eq 0 ]]; then
      _dr_ok 'client upstream' "$upstream (current)"
    elif [[ $ahead -eq 0 ]]; then
      _dr_warn 'client is behind upstream' "$upstream: $behind commit(s) behind"
    elif [[ $behind -eq 0 ]]; then
      _dr_warn 'client is ahead of upstream' "$upstream: $ahead commit(s) ahead"
    else
      _dr_warn 'client upstream has diverged' \
        "$upstream: $ahead ahead, $behind behind"
    fi
  else
    _dr_warn 'client upstream could not be compared' "$upstream"
  fi
}
