# shellcheck shell=bash
# Typed client repository selection. Repository commands use this function API
# instead of evaluating a multi-word Git command string.

DOT_CLIENT_GIT_DIR=${DOT_CLIENT_GIT_DIR:-$HOME/.dotfiles}
DOT_BASE_TOPOLOGY=missing

_dot_client_record() {
  local completed transaction
  dot_xdg_path state dot/init/completed || return 1
  completed=$REPLY
  transaction=${completed%/*}/transaction/record
  if [[ -e $completed || -L $completed ]]; then
    _dot_init_read_record "$completed" || return 2
    [[ $DOT_INIT_PHASE == complete ]] || return 2
    REPLY=$completed
    return 0
  fi
  if [[ -e $transaction || -L $transaction ]]; then
    _dot_init_read_record "$transaction" || return 2
    REPLY=$transaction
    return 0
  fi
  return 1
}

_dot_client_single_origin_identity() {
  local git_dir=$1 line
  local -a urls=()
  mapfile -t urls < <(git --git-dir="$git_dir" config --get-all remote.origin.url 2>/dev/null)
  [[ ${#urls[@]} -eq 1 ]] || return 1
  line=${urls[0]}
  _dot_init_repo_identity "$line"
}

_dot_client_validate_legacy_separate() {
  local git_dir=$HOME/.dotfiles absolute expected root branch bare worktree

  [[ -d $git_dir && ! -L $git_dir ]] || return 1
  absolute=$(git --git-dir="$git_dir" rev-parse --absolute-git-dir 2>/dev/null) ||
    return 1
  expected=$(cd -P -- "$git_dir" 2>/dev/null && pwd -P) || return 1
  root=$(cd -P -- "$absolute" 2>/dev/null && pwd -P) || return 1
  [[ $root == "$expected" ]] || return 1
  _dot_client_single_origin_identity "$git_dir" >/dev/null || return 1
  branch=$(git --git-dir="$git_dir" symbolic-ref --short HEAD 2>/dev/null) || return 1
  _dot_init_branch_valid "$branch" || return 1
  bare=$(git --git-dir="$git_dir" config --bool core.bare 2>/dev/null || true)
  case $bare in
    true) ;;
    false)
      worktree=$(git --git-dir="$git_dir" config core.worktree 2>/dev/null) || return 1
      [[ $worktree == "$HOME" ]] || return 1
      ;;
    *) return 1 ;;
  esac
}

_dot_client_select() {
  local record_rc=0 recorded_git_dir=''

  DOT_BASE_TOPOLOGY=missing
  _dot_client_record >/dev/null || record_rc=$?
  if [[ $record_rc -eq 2 ]]; then
    printf 'dot: malformed initialization identity record\n' >&2
    return 1
  fi
  if [[ $record_rc -eq 0 ]]; then
    recorded_git_dir=$DOT_INIT_GIT_DIR
    if [[ $recorded_git_dir == "$HOME/.dotfiles" ]]; then
      DOT_CLIENT_GIT_DIR=$recorded_git_dir
      if [[ -e $recorded_git_dir || -L $recorded_git_dir ]]; then
        _dot_init_live_git_matches_record || {
          printf 'dot: client Git directory no longer matches initialization identity\n' >&2
          return 1
        }
        DOT_BASE_TOPOLOGY=separate
      fi
      return 0
    fi
    if [[ $recorded_git_dir == "$HOME/.git" ]]; then
      _dot_init_live_git_matches_record || {
        printf 'dot: ordinary HOME checkout no longer matches initialization identity\n' >&2
        return 1
      }
      DOT_CLIENT_GIT_DIR=$recorded_git_dir
      DOT_BASE_TOPOLOGY=ordinary
      return 0
    fi
    printf 'dot: initialization identity names an unsupported Git directory\n' >&2
    return 1
  fi

  if [[ -e $HOME/.dotfiles || -L $HOME/.dotfiles ]]; then
    _dot_client_validate_legacy_separate || {
      printf 'dot: unsupported or foreign client Git directory: %s\n' "$HOME/.dotfiles" >&2
      return 1
    }
    DOT_CLIENT_GIT_DIR=$HOME/.dotfiles
    DOT_BASE_TOPOLOGY=separate
  elif [[ -e $HOME/.git || -L $HOME/.git ]]; then
    if [[ ${DOT_ORIGINAL_ARGV[0]:-} == init ]]; then
      return 0
    fi
    printf 'dot: ordinary HOME checkout requires a completed dot init identity\n' >&2
    return 1
  fi
}

_base_repo_exists() {
  [[ $DOT_BASE_TOPOLOGY != missing ]]
}

_base_git() {
  case $DOT_BASE_TOPOLOGY in
    separate)
      command git --git-dir="$DOT_CLIENT_GIT_DIR" --work-tree="$HOME" "$@"
      ;;
    ordinary)
      command git -C "$HOME" "$@"
      ;;
    *) return 128 ;;
  esac
}

_dot_client_select
DOTFILES=$DOT_CLIENT_GIT_DIR
export DOT_CLIENT_GIT_DIR DOT_BASE_TOPOLOGY DOTFILES
