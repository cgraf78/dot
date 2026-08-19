# shellcheck shell=bash
# Durable client-repository initialization and recovery.

_dot_init_state_root() {
  dot_xdg_path state dot/init
}

_dot_init_transaction_dir() {
  _dot_init_state_root || return
  REPLY=$REPLY/transaction
}

_dot_init_completed_file() {
  _dot_init_state_root || return
  REPLY=$REPLY/completed
}

_dot_init_error() {
  printf 'dot init: %s\n' "$*" >&2
  return 1
}

_dot_init_safe_value() {
  [[ -n $1 && $1 != *$'\t'* && $1 != *$'\n'* && $1 != *$'\r'* ]]
}

_dot_init_safe_relative_path() {
  local path=$1 component
  _dot_init_safe_value "$path" || return 1
  case $path in
    '' | /* | . | .. | ./* | ../* | */./* | */../* | */. | */.. | */ | *//*)
      return 1
      ;;
  esac
  IFS=/ read -r -a _dot_init_path_parts <<<"$path"
  for component in "${_dot_init_path_parts[@]}"; do
    [[ ${component,,} != .git ]] || return 1
  done
}

# Client launchers live below HOME and may depend on tracked helpers that init
# moves before the replacement generation is ready. Select a regular Git from
# the host PATH outside both client-controlled roots, then bind that exact
# executable for the complete transaction instead of re-resolving PATH between
# conflict moves.
_dot_init_select_host_git() {
  local home_physical source_physical directory physical_directory candidate
  local -a path_directories=()

  REPLY=
  home_physical=$(cd -P -- "$HOME" 2>/dev/null && pwd -P) || return 1
  source_physical=$(cd -P -- "$DOT_SOURCE_ROOT" 2>/dev/null && pwd -P) ||
    return 1
  IFS=: read -r -a path_directories <<<"${PATH:-}"
  for directory in "${path_directories[@]}"; do
    [[ $directory == /* ]] || continue
    physical_directory=$(cd -P -- "$directory" 2>/dev/null && pwd -P) ||
      continue
    candidate=${physical_directory%/}/git
    [[ $physical_directory == / ]] && candidate=/git
    [[ -f $candidate && ! -L $candidate && -x $candidate ]] || continue
    if [[ $home_physical == / || $candidate == "$home_physical" ||
      $candidate == "$home_physical/"* ]]; then
      continue
    fi
    if [[ $source_physical == / || $candidate == "$source_physical" ||
      $candidate == "$source_physical/"* ]]; then
      continue
    fi
    REPLY=$candidate
    return 0
  done
  return 1
}

_dot_init_bind_host_git() {
  local host_git

  _dot_init_select_host_git ||
    _dot_init_error 'host Git is unavailable outside HOME and the Dot checkout' ||
    return
  host_git=$REPLY
  if declare -F git >/dev/null 2>&1; then
    _dot_init_error 'a shell function named git cannot be used during initialization'
    return 1
  fi
  set -h
  hash -p "$host_git" git
}

_dot_init_repo_identity() {
  local url=$1 rest host path userinfo

  _dot_init_safe_value "$url" || return 1
  case $url in
    file://*)
      path=${url#file://}
      [[ $path == /* ]] || return 1
      path=$(realpath "$path" 2>/dev/null) || return 1
      printf 'file://%s\n' "$path"
      ;;
    /*)
      path=$(realpath "$url" 2>/dev/null) || return 1
      printf 'file://%s\n' "$path"
      ;;
    http://* | https://*)
      rest=${url#*://}
      host=${rest%%/*}
      path=${rest#*/}
      [[ $rest != "$host" && -n $host && -n $path ]] || return 1
      case $host in *'@'* | *':'*) return 1 ;; esac
      while [[ $path == */ ]]; do path=${path%/}; done
      path=${path%.git}
      [[ -n $path ]] || return 1
      printf 'git://%s/%s\n' "${host,,}" "$path"
      ;;
    ssh://*)
      rest=${url#ssh://}
      host=${rest%%/*}
      path=${rest#*/}
      [[ $rest != "$host" && -n $host && -n $path ]] || return 1
      userinfo=${host%@*}
      [[ $host == *'@'* ]] && host=${host#*@}
      [[ $userinfo != *':'* && $host != *':'* ]] || return 1
      while [[ $path == */ ]]; do path=${path%/}; done
      path=${path%.git}
      [[ -n $path ]] || return 1
      printf 'git://%s/%s\n' "${host,,}" "$path"
      ;;
    *:*)
      host=${url%%:*}
      path=${url#*:}
      [[ -n $host && -n $path && $host != */* ]] || return 1
      [[ $host == *'@'* ]] && host=${host#*@}
      while [[ $path == /* ]]; do path=${path#/}; done
      while [[ $path == */ ]]; do path=${path%/}; done
      path=${path%.git}
      [[ -n $host && -n $path ]] || return 1
      printf 'git://%s/%s\n' "${host,,}" "$path"
      ;;
    *) return 1 ;;
  esac
}

_dot_init_branch_valid() {
  [[ -n $1 ]] && git check-ref-format --branch "$1" >/dev/null 2>&1
}

_dot_init_remote_default_branch() {
  local url=$1 stage advertised ref oid branch='' head_oid='' selected=''
  local clone_ok=0 line
  local -a branches=()

  _dot_cleanup_mktemp -d || return 1
  stage=$REPLY
  rmdir "$stage" || return 1
  if git clone --quiet --no-checkout -- "$url" "$stage" >/dev/null 2>&1; then
    clone_ok=1
    branch=$(git -C "$stage" symbolic-ref --short refs/remotes/origin/HEAD \
      2>/dev/null || true)
    branch=${branch#origin/}
    if _dot_init_branch_valid "$branch" &&
      git -C "$stage" show-ref --verify --quiet "refs/remotes/origin/$branch"; then
      selected=$branch
    fi
  fi

  if [[ -z $selected ]]; then
    _dot_cleanup_mktemp || {
      _dot_cleanup_remove_path "$stage" || true
      return 1
    }
    advertised=$REPLY
    if git ls-remote --symref --exit-code -- "$url" HEAD >"$advertised" 2>/dev/null; then
      branch=''
      head_oid=''
      while IFS=$'\t' read -r ref oid; do
        case $ref in
          'ref: refs/heads/'*) branch=${ref#ref: refs/heads/} ;;
          [0-9a-fA-F]*) [[ $oid == HEAD ]] && head_oid=$ref ;;
        esac
      done <"$advertised"
      if _dot_init_branch_valid "$branch" &&
        [[ $head_oid =~ ^[0-9a-fA-F]{40,64}$ ]]; then
        selected=$branch
      fi
    fi
    _dot_cleanup_remove_path "$advertised" || true
  fi

  if [[ -z $selected && $clone_ok -eq 1 ]]; then
    while IFS= read -r line; do
      [[ -n $line && $line != HEAD ]] || continue
      _dot_init_branch_valid "$line" || {
        _dot_cleanup_remove_path "$stage" || true
        return 1
      }
      branches+=("$line")
    done < <(git -C "$stage" for-each-ref --format='%(refname:strip=3)' \
      refs/remotes/origin)
    for line in "${branches[@]+"${branches[@]}"}"; do
      if [[ $line == main ]]; then
        selected=main
        break
      fi
    done
    if [[ -z $selected && ${#branches[@]} -eq 1 ]]; then
      selected=${branches[0]}
    fi
  fi

  _dot_cleanup_remove_path "$stage" || return 1
  _dot_init_branch_valid "$selected" || return 1
  printf '%s\n' "$selected"
}

_dot_init_private_directory() {
  local path=$1
  mkdir -p "$path" || return 1
  [[ -d $path && ! -L $path ]] || return 1
  chmod 0700 "$path"
}

_dot_init_prepare_transaction() {
  local transaction=$1 stage marker
  _dot_init_private_directory "${transaction%/*}" || return 1
  stage=$(mktemp -d "${transaction}.prepare.XXXXXX") || return 1
  chmod 0700 "$stage" || return 1
  # A matching pathname is not ownership evidence. Publish this exact private
  # marker before the directory can become an orphan so a later invocation
  # removes only a preparation generation that dot can prove it created.
  marker=$stage/.dot-transaction-stage-v1
  printf 'cgraf78 dot initialization preparation v1\n' >"$marker" || return 1
  chmod 0600 "$marker" || return 1
  REPLY=$stage
}

_dot_init_transaction_stage_owned() {
  local stage=$1 marker=$1/.dot-transaction-stage-v1 mode links

  [[ -d $stage && ! -L $stage && -O $stage ]] || return 1
  mode=$(stat -c '%a' "$stage" 2>/dev/null || stat -f '%Lp' "$stage" 2>/dev/null) ||
    return 1
  [[ $mode != *[!0-7]* ]] || return 1
  (((8#$mode & 077) == 0)) || return 1
  [[ -f $marker && ! -L $marker && -O $marker ]] || return 1
  mode=$(stat -c '%a' "$marker" 2>/dev/null || stat -f '%Lp' "$marker" 2>/dev/null) ||
    return 1
  links=$(stat -c '%h' "$marker" 2>/dev/null || stat -f '%l' "$marker" 2>/dev/null) ||
    return 1
  [[ $mode == 600 && $links == 1 ]] || return 1
  printf 'cgraf78 dot initialization preparation v1\n' |
    _dot_stdin_matches_file "$marker"
}

_dot_init_recover_transaction_stages() {
  local transaction=$1 stage nullglob_was_set=0
  local -a stages=()

  shopt -q nullglob && nullglob_was_set=1
  shopt -s nullglob
  stages=("$transaction".prepare.*)
  [[ $nullglob_was_set -eq 1 ]] || shopt -u nullglob
  for stage in "${stages[@]+"${stages[@]}"}"; do
    _dot_init_transaction_stage_owned "$stage" || continue
    rm -rf -- "$stage" || return 1
  done
}

_dot_init_publish_transaction() {
  local stage=$1 transaction=$2
  _dot_init_transaction_stage_owned "$stage" || return 1
  [[ -f $stage/record && ! -L $stage/record ]] ||
    return 1
  _dot_move_noreplace "$stage" "$transaction"
}

_dot_init_symlink_blob_safe() {
  local repo=$1 branch=$2 path=$3 raw size
  # Read the blob without command substitution: shell variables cannot retain
  # NUL, while tabs and newlines cannot be represented in the TSV journals.
  # Reject those bytes before any client path or Git directory is published.
  _dot_cleanup_mktemp || return 1
  raw=$REPLY
  if ! git -C "$repo" show "$branch:$path" >"$raw"; then
    _dot_cleanup_remove_path "$raw" || true
    return 1
  fi
  size=$(LC_ALL=C wc -c <"$raw" 2>/dev/null | tr -d '[:space:]') || size=''
  if [[ ! $size =~ ^[0-9]+$ || $size -eq 0 || $size -gt 4096 ]] ||
    ! LC_ALL=C od -An -tx1 "$raw" 2>/dev/null |
    awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^(00|09|0a|0d)$/) exit 1 }'; then
    _dot_cleanup_remove_path "$raw" || true
    return 1
  fi
  _dot_cleanup_remove_path "$raw"
}

_dot_init_write_record() {
  local destination=$1 phase=$2 origin=$3 identity=$4 branch=$5 backup=$6
  local git_dir=${7:-$HOME/.dotfiles} temporary dot_revision commit nonce git_dev git_ino

  dot_revision=$(_dot_source_git rev-parse HEAD 2>/dev/null) || return 1
  commit=${DOT_INIT_COMMIT:-0000000000000000000000000000000000000000}
  nonce=${DOT_INIT_NONCE:-legacy}
  git_dev=${DOT_INIT_GIT_DEV:--}
  git_ino=${DOT_INIT_GIT_INO:--}

  _dot_sibling_tmp_for "$destination" || return 1
  temporary=$REPLY
  {
    printf 'cgraf78 dot initialization transaction v1\n'
    printf 'phase=%s\n' "$phase"
    printf 'origin=%s\n' "$origin"
    printf 'identity=%s\n' "$identity"
    printf 'branch=%s\n' "$branch"
    printf 'commit=%s\n' "$commit"
    printf 'git_dir=%s\n' "$git_dir"
    printf 'worktree=%s\n' "$HOME"
    printf 'backup=%s\n' "$backup"
    printf 'dot=%s\n' "$DOT_BIN"
    printf 'dot_revision=%s\n' "$dot_revision"
    printf 'nonce=%s\n' "$nonce"
    printf 'git_dev=%s\n' "$git_dev"
    printf 'git_ino=%s\n' "$git_ino"
  } >"$temporary" || {
    rm -f "$temporary"
    return 1
  }
  chmod 0600 "$temporary" || return 1
  if [[ -e $destination || -L $destination ]]; then
    _dot_move_replace_nodir "$temporary" "$destination"
  else
    _dot_move_noreplace "$temporary" "$destination"
  fi
}

_dot_init_read_record() {
  local record=$1 line count=0 key value mode size
  local -A seen=()

  [[ -f $record && ! -L $record && -O $record ]] || return 1
  mode=$(stat -c '%a' "$record" 2>/dev/null || stat -f '%Lp' "$record" 2>/dev/null) ||
    return 1
  size=$(LC_ALL=C wc -c <"$record" 2>/dev/null | tr -d '[:space:]') || return 1
  [[ $mode != *[!0-7]* && $size =~ ^[0-9]+$ && $size -le 16384 ]] || return 1
  (((8#$mode & 077) == 0)) || return 1
  DOT_INIT_PHASE='' DOT_INIT_ORIGIN='' DOT_INIT_IDENTITY='' DOT_INIT_BRANCH=''
  DOT_INIT_COMMIT='' DOT_INIT_GIT_DIR='' DOT_INIT_WORKTREE='' DOT_INIT_BACKUP=''
  DOT_INIT_DOT='' DOT_INIT_DOT_REVISION='' DOT_INIT_NONCE=''
  DOT_INIT_GIT_DEV='' DOT_INIT_GIT_INO=''
  while IFS= read -r line || [[ -n $line ]]; do
    count=$((count + 1))
    if [[ $count -eq 1 ]]; then
      [[ $line == 'cgraf78 dot initialization transaction v1' ]] || return 1
      continue
    fi
    [[ $line == *=* ]] || return 1
    key=${line%%=*}
    value=${line#*=}
    _dot_init_safe_value "$value" || return 1
    [[ -z ${seen[$key]+x} ]] || return 1
    seen[$key]=1
    case $key in
      phase) DOT_INIT_PHASE=$value ;;
      origin) DOT_INIT_ORIGIN=$value ;;
      identity) DOT_INIT_IDENTITY=$value ;;
      branch) DOT_INIT_BRANCH=$value ;;
      commit) DOT_INIT_COMMIT=$value ;;
      git_dir) DOT_INIT_GIT_DIR=$value ;;
      worktree) DOT_INIT_WORKTREE=$value ;;
      backup) DOT_INIT_BACKUP=$value ;;
      dot) DOT_INIT_DOT=$value ;;
      dot_revision) DOT_INIT_DOT_REVISION=$value ;;
      nonce) DOT_INIT_NONCE=$value ;;
      git_dev) DOT_INIT_GIT_DEV=$value ;;
      git_ino) DOT_INIT_GIT_INO=$value ;;
      *) return 1 ;;
    esac
  done <"$record"
  [[ $count -eq 14 && -n $DOT_INIT_PHASE && -n $DOT_INIT_ORIGIN &&
    -n $DOT_INIT_IDENTITY && -n $DOT_INIT_BRANCH &&
    ($DOT_INIT_GIT_DIR == "$HOME/.dotfiles" ||
    $DOT_INIT_GIT_DIR == "$HOME/.git") &&
    $DOT_INIT_WORKTREE == "$HOME" && -n $DOT_INIT_BACKUP ]] || return 1
  case $DOT_INIT_PHASE in
    prepared | backing-up | backed-up | git-staging | git-staged | publishing | \
      checkout | converging | complete) ;;
    *) return 1 ;;
  esac
  _dot_init_branch_valid "$DOT_INIT_BRANCH" || return 1
  [[ $DOT_INIT_COMMIT =~ ^[0-9a-fA-F]{40}$|^[0-9a-fA-F]{64}$ ]] || return 1
  [[ $DOT_INIT_DOT == /* && $DOT_INIT_DOT != *//* &&
    $DOT_INIT_DOT != */./* && $DOT_INIT_DOT != */../* ]] || return 1
  [[ $DOT_INIT_DOT_REVISION =~ ^[0-9a-fA-F]{40}$|^[0-9a-fA-F]{64}$ ]] || return 1
  [[ $DOT_INIT_NONCE =~ ^[A-Za-z0-9._-]+$ ]] || return 1
  if [[ $DOT_INIT_GIT_DEV == - || $DOT_INIT_GIT_INO == - ]]; then
    [[ $DOT_INIT_GIT_DEV == - && $DOT_INIT_GIT_INO == - ]] || return 1
  else
    [[ $DOT_INIT_GIT_DEV =~ ^[0-9]+$ && $DOT_INIT_GIT_INO =~ ^[0-9]+$ ]] || return 1
  fi
  if [[ $DOT_INIT_BACKUP != - ]]; then
    case $DOT_INIT_BACKUP in
      "$HOME/.dot-backup/"*) ;;
      *) return 1 ;;
    esac
  fi
}

_dot_init_candidate_tree() {
  local repo=$1 branch=$2 output=$3 entry header mode type oid path count=0
  local raw valid=1

  : >"$output"
  _dot_cleanup_mktemp || return 1
  raw=$REPLY
  if ! git -C "$repo" ls-tree -rz --full-tree "$branch" >"$raw"; then
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
    if [[ $mode == 120000 ]] &&
      ! _dot_init_symlink_blob_safe "$repo" "$branch" "$path"; then
      valid=0
      break
    fi
    if dot_candidate_path_is_reserved "$HOME/$path"; then
      # The one client-owned control-plane exception is the generated regular
      # command adapter. Its exact bytes are checked against this release so a
      # repository cannot smuggle another executable into the reserved front
      # door during initialization.
      if [[ $path == .local/bin/dot && $mode == 100755 ]] &&
        git -C "$repo" show "$branch:$path" 2>/dev/null |
        _dot_stdin_matches_file "$DOT_SOURCE_ROOT/support/client-launcher.sh"; then
        :
      else
        valid=0
        break
      fi
    fi
    printf '%s\t%s\t%s\n' "$mode" "$oid" "$path" >>"$output" || {
      valid=0
      break
    }
    count=$((count + 1))
    [[ $count -le 100000 ]] || {
      valid=0
      break
    }
  done <"$raw"
  _dot_cleanup_remove_path "$raw" || return 1
  if [[ $valid -eq 1 && $count -gt 0 ]]; then
    return 0
  fi
  : >"$output"
  return 1
}

_dot_init_candidate_matches_path() {
  local repo=$1 branch=$2 mode=$3 path=$4 target actual_mode
  target=$HOME/$path

  [[ -e $target || -L $target ]] || return 1
  case $mode in
    120000)
      [[ -L $target ]] || return 1
      git -C "$repo" show "$branch:$path" 2>/dev/null |
        _dot_stdin_matches_file <(printf '%s' "$(readlink "$target")")
      ;;
    100644 | 100755)
      [[ -f $target && ! -L $target ]] || return 1
      git -C "$repo" show "$branch:$path" 2>/dev/null |
        _dot_stdin_matches_file "$target" || return 1
      actual_mode=$(stat -c '%a' "$target" 2>/dev/null || stat -f '%Lp' "$target" 2>/dev/null) ||
        return 1
      if [[ $mode == 100755 ]]; then
        (((8#$actual_mode & 0111) != 0))
      else
        (((8#$actual_mode & 0111) == 0))
      fi
      ;;
    *) return 1 ;;
  esac
}

_dot_init_snapshot_path() {
  local path=$1 kind dev ino mode size value identity

  if [[ ! -e $path && ! -L $path ]]; then
    printf 'absent\t-\t-\t-\t-\t-\n'
    return 0
  fi
  identity=$(_dot_path_identity "$path") || return 1
  dev=${identity%%:*}
  ino=${identity#*:}
  mode=$(stat -c '%a' "$path" 2>/dev/null || stat -f '%Lp' "$path" 2>/dev/null) ||
    return 1
  size=$(stat -c '%s' "$path" 2>/dev/null || stat -f '%z' "$path" 2>/dev/null) ||
    return 1
  if [[ -L $path ]]; then
    kind=symlink
    value=$(readlink "$path") || return 1
    _dot_init_safe_value "$value" || return 1
  elif [[ -f $path ]]; then
    kind=regular
    value=$(git hash-object --no-filters -- "$path" 2>/dev/null) || return 1
    [[ $value =~ ^[0-9a-fA-F]{40}$|^[0-9a-fA-F]{64}$ ]] || return 1
  elif [[ -d $path ]]; then
    kind=directory
    value=-
  else
    return 1
  fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$kind" "$dev" "$ino" "$mode" "$size" "$value"
}

_dot_init_path_state_matches() {
  local path=$1 kind=$2 dev=$3 ino=$4 mode=$5 size=$6 value=$7
  local identity current_mode current_size current_value

  if [[ $kind == absent ]]; then
    [[ ! -e $path && ! -L $path ]]
    return
  fi
  case $kind in
    regular) [[ -f $path && ! -L $path ]] || return 1 ;;
    symlink) [[ -L $path ]] || return 1 ;;
    directory) [[ -d $path && ! -L $path ]] || return 1 ;;
    *) return 1 ;;
  esac
  identity=$(_dot_path_identity "$path") || return 1
  [[ $identity == "$dev:$ino" ]] || return 1
  current_mode=$(stat -c '%a' "$path" 2>/dev/null || stat -f '%Lp' "$path" 2>/dev/null) ||
    return 1
  current_size=$(stat -c '%s' "$path" 2>/dev/null || stat -f '%z' "$path" 2>/dev/null) ||
    return 1
  [[ $current_mode == "$mode" ]] || return 1
  case $kind in
    regular)
      [[ $current_size == "$size" ]] || return 1
      current_value=$(git hash-object --no-filters -- "$path" 2>/dev/null) || return 1
      [[ $current_value == "$value" ]]
      ;;
    symlink)
      current_value=$(readlink "$path") || return 1
      [[ $current_size == "$size" && $current_value == "$value" ]]
      ;;
    directory) return 0 ;;
  esac
}

_dot_init_conflict_root() {
  local path=$1 current parent
  current=$path
  while [[ $current == */* ]]; do
    parent=${current%/*}
    if [[ -e $HOME/$parent || -L $HOME/$parent ]]; then
      [[ -d $HOME/$parent && ! -L $HOME/$parent ]] || {
        printf '%s\n' "$parent"
        return 0
      }
    fi
    current=$parent
  done
  printf '%s\n' "$path"
}

_dot_init_build_prior_and_conflicts() {
  local repo=$1 branch=$2 tree=$3 prior=$4 conflicts=$5 mode oid path root state
  local -A seen=()

  : >"$prior"
  : >"$conflicts"
  while IFS=$'\t' read -r mode oid path; do
    state=$(_dot_init_snapshot_path "$HOME/$path") || return 1
    printf '%s\t%s\n' "$path" "$state" >>"$prior" || return 1
    if [[ $state != absent$'\t'* ]] &&
      _dot_init_candidate_matches_path "$repo" "$branch" "$mode" "$path"; then
      continue
    fi
    root=$(_dot_init_conflict_root "$path") || return 1
    [[ $state != absent$'\t'* || $root != "$path" ]] || continue
    [[ -z ${seen[$root]+x} ]] || continue
    seen[$root]=1
    state=$(_dot_init_snapshot_path "$HOME/$root") || return 1
    [[ $state != absent$'\t'* ]] || return 1
    printf '%s\t%s\n' "$root" "$state" >>"$conflicts" || return 1
  done <"$tree"
  chmod 0600 "$prior" "$conflicts"
}

_dot_init_confirm() {
  local manifest=$1 yes=$2 answer
  [[ -s $manifest ]] || return 0
  printf 'dot init: conflicting paths will be backed up:\n' >&2
  cut -f1 "$manifest" | sed 's/^/  /' >&2
  [[ $yes == true ]] && return 0
  [[ -r /dev/tty && -w /dev/tty ]] ||
    _dot_init_error 'conflicts require --yes in a noninteractive session'
  printf 'Continue? [y/N] ' >/dev/tty
  IFS= read -r answer </dev/tty || return 1
  [[ $answer == y || $answer == Y || $answer == yes || $answer == YES ]]
}

_dot_init_plan_summary() {
  local candidate=$1 branch=$2 tree=$3 backup=$4 identity=$5
  local count provider=none extensions=disabled preview result

  count=$(LC_ALL=C wc -l <"$tree" | tr -d '[:space:]') || return 1
  preview=$candidate/dot-config.preview
  if git -C "$candidate" cat-file -e "$branch:.config/dot/config" 2>/dev/null; then
    git -C "$candidate" show "$branch:.config/dot/config" >"$preview" || return 1
    # shellcheck disable=SC2016  # The isolated child expands these variables.
    result=$(
      HOME=$HOME DOT_SOURCE_ROOT=$DOT_SOURCE_ROOT \
        "$BASH" --noprofile --norc -c '
          set -euo pipefail
          . "$DOT_SOURCE_ROOT/lib/dot/config.sh"
          dot_config_load "$1"
          printf "%s\t%s\n" "$DOT_DEPENDENCY_PROVIDER" \
            "${DOT_EXTENSION_API:+enabled}"
        ' -- "$preview"
    ) || return 1
    IFS=$'\t' read -r provider extensions <<<"$result"
    [[ -n $extensions ]] || extensions=disabled
  fi
  if [[ ${DOT_INIT_SKIP_PROVIDER:-0} == 1 && $provider != none ]]; then
    provider="$provider (skipped for this invocation)"
  fi
  printf 'dot init plan:\n' >&2
  printf '  repository: %s\n' "$identity" >&2
  printf '  branch: %s\n' "$branch" >&2
  printf '  tracked paths: %s\n' "$count" >&2
  printf '  backup: %s\n' "$backup" >&2
  printf '  dependency provider: %s\n' "$provider" >&2
  printf '  extensions: %s\n' "$extensions" >&2
}

_dot_init_move_conflicts() {
  local manifest=$1 backup=$2 path kind dev ino mode size value destination
  _dot_init_private_directory "$backup" || return 1
  if [[ ! -e $backup/manifest && ! -L $backup/manifest ]]; then
    cp "$manifest" "$backup/manifest" || return 1
    chmod 0600 "$backup/manifest" || return 1
  elif ! _dot_files_equal "$manifest" "$backup/manifest"; then
    return 1
  fi
  while IFS=$'\t' read -r path kind dev ino mode size value; do
    [[ -n $path ]] || continue
    destination=$backup/$path
    if _dot_init_path_state_matches "$destination" "$kind" "$dev" "$ino" \
      "$mode" "$size" "$value" &&
      [[ ! -e $HOME/$path && ! -L $HOME/$path ]]; then
      continue
    fi
    _dot_init_path_state_matches "$HOME/$path" "$kind" "$dev" "$ino" \
      "$mode" "$size" "$value" || return 1
    mkdir -p "${destination%/*}" || return 1
    [[ ! -e $destination && ! -L $destination ]] || return 1
    _dot_move_noreplace "$HOME/$path" "$destination" || return 1
  done <"$manifest"
}

_dot_init_restore_backups() {
  local backup=$1 path kind dev ino mode size value source parent
  [[ -d $backup && ! -L $backup && -f $backup/manifest ]] || return 0
  while IFS=$'\t' read -r path kind dev ino mode size value; do
    _dot_init_safe_relative_path "$path" || return 1
    source=$backup/$path
    [[ -e $source || -L $source ]] || continue
    _dot_init_path_state_matches "$source" "$kind" "$dev" "$ino" \
      "$mode" "$size" "$value" || return 1
    [[ ! -e $HOME/$path && ! -L $HOME/$path ]] || return 1
    if [[ $path == */* ]]; then
      parent=$HOME/${path%/*}
    else
      parent=$HOME
    fi
    mkdir -p "$parent" || return 1
    _dot_move_noreplace "$source" "$HOME/$path" || return 1
  done < <(LC_ALL=C sort -r "$backup/manifest")
}

_dot_init_publish_completed() {
  local record=$1 completed root temporary
  _dot_init_completed_file || return 1
  completed=$REPLY
  root=${completed%/*}
  _dot_init_private_directory "$root" || return 1
  temporary=$(mktemp "$root/.completed.XXXXXX") || return 1
  cp "$record" "$temporary" || return 1
  chmod 0600 "$temporary" || return 1
  if [[ -e $completed || -L $completed ]]; then
    [[ -f $completed && ! -L $completed && -O $completed ]] || return 1
    _dot_move_replace_nodir "$temporary" "$completed"
  else
    _dot_move_noreplace "$temporary" "$completed"
  fi
}

_dot_init_generation_marker() {
  printf '%s/dot-init-generation-v1\n' "$1"
}

_dot_init_write_generation_marker() {
  local git_dir=$1 marker temporary
  marker=$(_dot_init_generation_marker "$git_dir")
  _dot_sibling_tmp_for "$marker" || return 1
  temporary=$REPLY
  {
    printf 'cgraf78 dot client generation v1\n'
    printf 'nonce=%s\n' "$DOT_INIT_NONCE"
    printf 'commit=%s\n' "$DOT_INIT_COMMIT"
    printf 'identity=%s\n' "$DOT_INIT_IDENTITY"
  } >"$temporary" || return 1
  chmod 0600 "$temporary" || return 1
  _dot_move_noreplace "$temporary" "$marker"
}

_dot_init_generation_marker_matches() {
  local git_dir=$1 marker line count=0 nonce='' commit='' identity=''
  local -A seen=()

  [[ -d $git_dir && ! -L $git_dir ]] || return 1
  marker=$(_dot_init_generation_marker "$git_dir")
  [[ -f $marker && ! -L $marker && -O $marker ]] || return 1
  while IFS= read -r line || [[ -n $line ]]; do
    count=$((count + 1))
    if [[ $count -eq 1 ]]; then
      [[ $line == 'cgraf78 dot client generation v1' ]] || return 1
      continue
    fi
    [[ $line == *=* ]] || return 1
    local key=${line%%=*} value=${line#*=}
    [[ -z ${seen[$key]+x} ]] || return 1
    seen[$key]=1
    case $key in
      nonce) nonce=$value ;;
      commit) commit=$value ;;
      identity) identity=$value ;;
      *) return 1 ;;
    esac
  done <"$marker"
  [[ $count -eq 4 && $nonce == "$DOT_INIT_NONCE" &&
    $commit == "$DOT_INIT_COMMIT" && $identity == "$DOT_INIT_IDENTITY" ]]
}

_dot_init_generation_matches() {
  local git_dir=$1
  _dot_init_generation_marker_matches "$git_dir" || return 1
  [[ $(git --git-dir="$git_dir" rev-parse "refs/heads/$DOT_INIT_BRANCH" 2>/dev/null) == "$DOT_INIT_COMMIT" ]]
}

_dot_init_set_git_identity() {
  local identity
  identity=$(_dot_path_identity "$1") || return 1
  DOT_INIT_GIT_DEV=${identity%%:*}
  DOT_INIT_GIT_INO=${identity#*:}
}

_dot_init_record_phase() {
  local record=$1 phase=$2
  _dot_init_write_record "$record" "$phase" "$DOT_INIT_ORIGIN" \
    "$DOT_INIT_IDENTITY" "$DOT_INIT_BRANCH" "$DOT_INIT_BACKUP" \
    "$DOT_INIT_GIT_DIR" || return 1
  DOT_INIT_PHASE=$phase
}

_dot_init_stage_git() {
  local record=$1 container marker repo current
  container=$DOT_INIT_BACKUP/git-stage
  marker=$container/identity
  repo=$container/repo

  _dot_init_private_directory "$DOT_INIT_BACKUP" || return 1
  if [[ ! -e $container && ! -L $container ]]; then
    _dot_init_private_directory "$container" || return 1
    {
      printf 'cgraf78 dot Git stage v1\n'
      printf 'nonce=%s\ncommit=%s\nidentity=%s\n' \
        "$DOT_INIT_NONCE" "$DOT_INIT_COMMIT" "$DOT_INIT_IDENTITY"
    } >"$marker" || return 1
    chmod 0600 "$marker" || return 1
  fi
  [[ -d $container && ! -L $container && -f $marker && ! -L $marker ]] || return 1
  grep -Fqx "nonce=$DOT_INIT_NONCE" "$marker" || return 1
  grep -Fqx "commit=$DOT_INIT_COMMIT" "$marker" || return 1
  grep -Fqx "identity=$DOT_INIT_IDENTITY" "$marker" || return 1

  _dot_init_record_phase "$record" git-staging || return 1
  if [[ -e $DOT_INIT_GIT_DIR || -L $DOT_INIT_GIT_DIR ]]; then
    _dot_init_generation_matches "$DOT_INIT_GIT_DIR" || return 1
    _dot_init_set_git_identity "$DOT_INIT_GIT_DIR" || return 1
    _dot_init_record_phase "$record" git-staged
    return
  fi
  if [[ -e $repo || -L $repo ]]; then
    if _dot_init_generation_matches "$repo"; then
      _dot_init_set_git_identity "$repo" || return 1
      _dot_init_record_phase "$record" git-staged
      return
    fi
    [[ -d $repo && ! -L $repo ]] || return 1
    rm -rf -- "$repo" || return 1
  fi
  git clone --quiet --bare --branch "$DOT_INIT_BRANCH" --single-branch -- \
    "$DOT_INIT_ORIGIN" "$repo" || return 1
  current=$(git --git-dir="$repo" rev-parse "refs/heads/$DOT_INIT_BRANCH" 2>/dev/null) ||
    return 1
  [[ $current == "$DOT_INIT_COMMIT" ]] || return 1
  git --git-dir="$repo" config core.bare false || return 1
  git --git-dir="$repo" config core.worktree "$HOME" || return 1
  git --git-dir="$repo" config status.showUntrackedFiles no || return 1
  git --git-dir="$repo" config core.fsmonitor false || return 1
  git --git-dir="$repo" config remote.origin.fetch \
    '+refs/heads/*:refs/remotes/origin/*' || return 1
  git --git-dir="$repo" update-ref "refs/remotes/origin/$DOT_INIT_BRANCH" \
    "$DOT_INIT_COMMIT" || return 1
  git --git-dir="$repo" config "branch.$DOT_INIT_BRANCH.remote" origin || return 1
  git --git-dir="$repo" config "branch.$DOT_INIT_BRANCH.merge" \
    "refs/heads/$DOT_INIT_BRANCH" || return 1
  _dot_init_write_generation_marker "$repo" || return 1
  _dot_init_set_git_identity "$repo" || return 1
  _dot_init_record_phase "$record" git-staged
}

_dot_init_publish_git() {
  local record=$1 staged=$DOT_INIT_BACKUP/git-stage/repo git_dir=$DOT_INIT_GIT_DIR

  if [[ ! -e $git_dir && ! -L $git_dir ]]; then
    _dot_init_generation_matches "$staged" || return 1
    _dot_move_noreplace "$staged" "$git_dir" || return 1
  fi
  _dot_init_generation_matches "$git_dir" || return 1
  _dot_init_set_git_identity "$git_dir" || return 1
  _dot_init_record_phase "$record" publishing
}

_dot_init_candidate_matches_git() {
  local git_dir=$1 commit=$2 mode=$3 oid=$4 path=$5 target=$HOME/$5
  local actual_mode actual_oid link_target

  case $mode in
    120000)
      [[ -L $target ]] || return 1
      link_target=$(
        readlink "$target"
        printf .
      ) || return 1
      link_target=${link_target%.}
      actual_oid=$(printf '%s' "$link_target" |
        git --git-dir="$git_dir" hash-object --stdin) ||
        return 1
      [[ $actual_oid == "$oid" ]]
      ;;
    100644 | 100755)
      [[ -f $target && ! -L $target ]] || return 1
      actual_oid=$(git --git-dir="$git_dir" hash-object --no-filters -- \
        "$target" 2>/dev/null) || return 1
      [[ $actual_oid == "$oid" ]] || return 1
      actual_mode=$(stat -c '%a' "$target" 2>/dev/null || stat -f '%Lp' "$target" 2>/dev/null) ||
        return 1
      if [[ $mode == 100755 ]]; then
        (((8#$actual_mode & 0111) != 0))
      else
        (((8#$actual_mode & 0111) == 0))
      fi
      ;;
    *) return 1 ;;
  esac
}

_dot_init_write_private_line() {
  local file=$1 line=$2 replace=${3:-false} temporary
  _dot_sibling_tmp_for "$file" || return 1
  temporary=$REPLY
  printf '%s\n' "$line" >"$temporary" || return 1
  chmod 0600 "$temporary" || return 1
  if [[ $replace == true ]]; then
    _dot_move_replace_nodir "$temporary" "$file"
  else
    _dot_move_noreplace "$temporary" "$file"
  fi
}

_dot_init_entry_stage() {
  local path=$1 hash parent
  hash=$(printf '%s' "$path" | git hash-object --stdin) || return 1
  parent=${path%/*}
  [[ $parent != "$path" ]] || parent=''
  REPLY="$HOME${parent:+/$parent}/.dot-init-entry.$DOT_INIT_NONCE.$hash"
}

_dot_init_publish_intent() {
  local file=$1 mode=$2 oid=$3 path=$4 stage line
  _dot_init_entry_stage "$path" || return 1
  stage=${REPLY#"$HOME"/}
  line="pending"$'\t'"$mode"$'\t'"$oid"$'\t'"$path"$'\t'"$stage"$'\t-\t-\t-\t-'
  if [[ -e $file || -L $file ]]; then
    _dot_init_entry_intent "$file" "$mode" "$oid" "$path" >/dev/null
    return
  fi
  _dot_init_write_private_line "$file" "$line"
}

_dot_init_stage_claim_file() {
  REPLY=$1/.dot-init-stage-claim-v1
}

_dot_init_stage_claim_matches() {
  local stage=$1 kind=$2 path=$3 marker mode links

  case $kind in entry | parent) ;; *) return 1 ;; esac
  _dot_init_safe_relative_path "$path" || return 1
  _dot_init_stage_claim_file "$stage"
  marker=$REPLY
  [[ -f $marker && ! -L $marker && -O $marker ]] || return 1
  mode=$(stat -c '%a' "$marker" 2>/dev/null || stat -f '%Lp' "$marker" 2>/dev/null) ||
    return 1
  links=$(stat -c '%h' "$marker" 2>/dev/null || stat -f '%l' "$marker" 2>/dev/null) ||
    return 1
  [[ $mode == 600 && $links == 1 ]] || return 1
  {
    printf 'cgraf78 dot publication stage claim v1\n'
    printf 'kind=%s\nnonce=%s\npath=%s\n' "$kind" "$DOT_INIT_NONCE" "$path"
  } | _dot_stdin_matches_file "$marker"
}

_dot_init_stage_claim_write() {
  local stage=$1 kind=$2 path=$3 marker temporary

  _dot_init_stage_claim_file "$stage"
  marker=$REPLY
  [[ ! -e $marker && ! -L $marker ]] || return 1
  _dot_sibling_tmp_for "$marker" || return 1
  temporary=$REPLY
  {
    printf 'cgraf78 dot publication stage claim v1\n'
    printf 'kind=%s\nnonce=%s\npath=%s\n' "$kind" "$DOT_INIT_NONCE" "$path"
  } >"$temporary" || return 1
  chmod 0600 "$temporary" || return 1
  _dot_move_noreplace "$temporary" "$marker" || return 1
  _dot_init_stage_claim_matches "$stage" "$kind" "$path"
}

_dot_init_stage_claim_only() {
  local stage=$1 entry nullglob_was_set=0 dotglob_was_set=0
  local -a entries=()

  shopt -q nullglob && nullglob_was_set=1
  shopt -q dotglob && dotglob_was_set=1
  shopt -s nullglob dotglob
  entries=("$stage"/*)
  [[ $nullglob_was_set -eq 1 ]] || shopt -u nullglob
  [[ $dotglob_was_set -eq 1 ]] || shopt -u dotglob
  ((${#entries[@]} == 1)) || return 1
  [[ ${entries[0]##*/} == .dot-init-stage-claim-v1 ]]
}

_dot_init_stage_claim_remove() {
  local stage=$1 kind=$2 path=$3 marker

  _dot_init_stage_claim_matches "$stage" "$kind" "$path" || return 1
  _dot_init_stage_claim_file "$stage"
  marker=$REPLY
  rm -f -- "$marker"
}

_dot_init_parent_record() {
  local transaction=$1 relative=$2 hash file line phase record_parent stage_rel
  local dev ino mode extra expected
  hash=$(printf '%s' "$relative" | git hash-object --stdin) || return 1
  file=$transaction/parent-intent.$hash
  [[ -f $file && ! -L $file ]] || return 1
  line=$(<"$file")
  [[ $line != *$'\n'* ]] || return 1
  IFS=$'\t' read -r phase record_parent stage_rel dev ino mode extra <<<"$line"
  expected="$HOME/${relative%/*}"
  [[ ${relative%/*} != "$relative" ]] || expected=$HOME
  expected=${expected%/}/.dot-init-parent.$DOT_INIT_NONCE.$hash
  [[ -z $extra && ($phase == pending || $phase == prepared) &&
    $record_parent == "$relative" &&
    $stage_rel == "${expected#"$HOME"/}" ]] || return 1
  if [[ $phase == prepared ]]; then
    [[ $dev =~ ^[0-9]+$ && $ino =~ ^[0-9]+$ &&
      $mode =~ ^[0-7]+$ ]] || return 1
  else
    [[ $dev == - && $ino == - && $mode == - ]] || return 1
  fi
  REPLY="$phase"$'\t'"$stage_rel"$'\t'"$dev"$'\t'"$ino"$'\t'"$mode"
}

# Parent publication uses two durable phases. `pending` reserves the exact
# transaction-derived stage path before mkdir; `prepared` binds that path to
# its device, inode, and mode before rename. Recovery therefore never guesses
# that an unrelated directory merely resembling a stage belongs to dot.
_dot_init_private_directory_matches() {
  local path=$1 expected_identity=${2:-} expected_mode=${3:-}
  local mode

  [[ -d $path && ! -L $path && -O $path ]] || return 1
  mode=$(stat -c '%a' "$path" 2>/dev/null || stat -f '%Lp' "$path" 2>/dev/null) ||
    return 1
  [[ $mode != *[!0-7]* ]] || return 1
  (((8#$mode & 077) == 0)) || return 1
  [[ -z $expected_mode || $mode == "$expected_mode" ]] || return 1
  [[ -z $expected_identity ]] ||
    [[ $(_dot_path_identity "$path" 2>/dev/null || true) == "$expected_identity" ]] ||
    return 1
}

_dot_init_private_empty_directory_matches() {
  local path=$1 expected_identity=${2:-} expected_mode=${3:-}
  local nullglob_was_set=0 dotglob_was_set=0
  local -a entries=()

  _dot_init_private_directory_matches "$path" "$expected_identity" "$expected_mode" ||
    return 1

  shopt -q nullglob && nullglob_was_set=1
  shopt -q dotglob && dotglob_was_set=1
  shopt -s nullglob dotglob
  entries=("$path"/*)
  [[ $nullglob_was_set -eq 1 ]] || shopt -u nullglob
  [[ $dotglob_was_set -eq 1 ]] || shopt -u dotglob
  ((${#entries[@]} == 0))
}

_dot_init_parent_directories() {
  local transaction=$1 relative=$2 current=$HOME component parent parent_rel=''
  local hash intent stage stage_rel identity dev ino mode phase record extra
  local -a parts=()
  parent=${relative%/*}
  [[ $parent != "$relative" ]] || return 0
  IFS=/ read -r -a parts <<<"$parent"
  for component in "${parts[@]}"; do
    parent_rel=${parent_rel:+$parent_rel/}$component
    current=$HOME/$parent_rel
    hash=$(printf '%s' "$parent_rel" | git hash-object --stdin) || return 1
    intent=$transaction/parent-intent.$hash
    stage=${current%/*}/.dot-init-parent.$DOT_INIT_NONCE.$hash
    stage_rel=${stage#"$HOME"/}
    if [[ -e $intent || -L $intent ]]; then
      _dot_init_parent_record "$transaction" "$parent_rel" || return 1
      IFS=$'\t' read -r phase record dev ino mode <<<"$REPLY"
      [[ $record == "$stage_rel" ]] || return 1
    elif [[ -e $current || -L $current ]]; then
      [[ -d $current && ! -L $current ]] || return 1
      continue
    else
      [[ ! -e $stage && ! -L $stage ]] || return 1
      _dot_init_write_private_line "$intent" \
        "pending"$'\t'"$parent_rel"$'\t'"$stage_rel"$'\t-\t-\t-' || return 1
      phase=pending
    fi
    if [[ $phase == pending ]]; then
      [[ ! -e $current && ! -L $current ]] || return 1
      if [[ ! -e $stage && ! -L $stage ]]; then
        mkdir "$stage" || return 1
        chmod 0700 "$stage" || return 1
        _dot_init_stage_claim_write "$stage" parent "$parent_rel" || return 1
      else
        # The pending record reserves this pathname, but a directory that won
        # after record publication is still foreign unless it carries the
        # exact claim written by the engine that created it.
        _dot_init_stage_claim_matches "$stage" parent "$parent_rel" || return 1
      fi
      _dot_init_private_directory_matches "$stage" || return 1
      _dot_init_stage_claim_only "$stage" || return 1
      identity=$(_dot_path_identity "$stage") || return 1
      dev=${identity%%:*}
      ino=${identity#*:}
      mode=$(stat -c '%a' "$stage" 2>/dev/null || stat -f '%Lp' "$stage" 2>/dev/null) ||
        return 1
      _dot_init_write_private_line "$intent" \
        "prepared"$'\t'"$parent_rel"$'\t'"$stage_rel"$'\t'"$dev"$'\t'"$ino"$'\t'"$mode" true || return 1
      phase=prepared
    fi
    [[ $phase == prepared ]] || return 1
    if [[ -e $current || -L $current ]]; then
      [[ ! -e $stage && ! -L $stage ]] || return 1
      _dot_init_private_directory_matches "$current" "$dev:$ino" "$mode" ||
        return 1
      continue
    fi
    _dot_init_private_directory_matches "$stage" "$dev:$ino" "$mode" || return 1
    if [[ -e $stage/.dot-init-stage-claim-v1 || -L $stage/.dot-init-stage-claim-v1 ]]; then
      _dot_init_stage_claim_only "$stage" || return 1
      _dot_init_stage_claim_remove "$stage" parent "$parent_rel" || return 1
    fi
    _dot_init_private_empty_directory_matches "$stage" "$dev:$ino" "$mode" || return 1
    _dot_move_noreplace "$stage" "$current" || return 1
    _dot_init_private_directory_matches "$current" "$dev:$ino" "$mode" ||
      return 1
  done
}

_dot_init_entry_intent() {
  local file=$1 expected_mode=$2 expected_oid=$3 expected_path=$4
  local line phase mode oid path stage dev ino next_dev next_ino extra expected_stage
  [[ -f $file && ! -L $file ]] || return 1
  line=$(<"$file")
  [[ $line != *$'\n'* ]] || return 1
  IFS=$'\t' read -r phase mode oid path stage dev ino next_dev next_ino extra <<<"$line"
  _dot_init_entry_stage "$expected_path" || return 1
  expected_stage=${REPLY#"$HOME"/}
  [[ -z $extra && ($phase == pending || $phase == staged || $phase == prepared) &&
    $mode == "$expected_mode" && $oid == "$expected_oid" &&
    $path == "$expected_path" && $stage == "$expected_stage" ]] || return 1
  if [[ $phase == prepared ]]; then
    [[ $dev =~ ^[0-9]+$ && $ino =~ ^[0-9]+$ &&
      $next_dev =~ ^[0-9]+$ && $next_ino =~ ^[0-9]+$ ]] || return 1
  elif [[ $phase == staged ]]; then
    [[ $dev =~ ^[0-9]+$ && $ino =~ ^[0-9]+$ &&
      $next_dev == - && $next_ino == - ]] || return 1
  else
    [[ $dev == - && $ino == - && $next_dev == - && $next_ino == - ]] || return 1
  fi
  REPLY="$phase"$'\t'"$stage"$'\t'"$dev"$'\t'"$ino"$'\t'"$next_dev"$'\t'"$next_ino"
}

_dot_init_entry_stage_valid() {
  local stage=$1 expected_identity=${2:-} mode

  [[ -d $stage && ! -L $stage && -O $stage ]] || return 1
  mode=$(stat -c '%a' "$stage" 2>/dev/null || stat -f '%Lp' "$stage" 2>/dev/null) ||
    return 1
  [[ $mode != *[!0-7]* ]] || return 1
  (((8#$mode & 077) == 0)) || return 1
  [[ -z $expected_identity ]] ||
    [[ $(_dot_path_identity "$stage" 2>/dev/null || true) == "$expected_identity" ]]
}

_dot_init_entry_stage_only_next() {
  local stage=$1 entry nullglob_was_set=0 dotglob_was_set=0
  local -a entries=()

  shopt -q nullglob && nullglob_was_set=1
  shopt -q dotglob && dotglob_was_set=1
  shopt -s nullglob dotglob
  entries=("$stage"/*)
  [[ $nullglob_was_set -eq 1 ]] || shopt -u nullglob
  [[ $dotglob_was_set -eq 1 ]] || shopt -u dotglob
  for entry in "${entries[@]+"${entries[@]}"}"; do
    case ${entry##*/} in
      next | .dot-init-stage-claim-v1) ;;
      *) return 1 ;;
    esac
  done
}

_dot_init_discard_staged_next() {
  local stage=$1 next=$1/next

  _dot_init_entry_stage_only_next "$stage" || return 1
  [[ -e $next || -L $next ]] || return 0
  [[ (-f $next && ! -L $next) || -L $next ]] || return 1
  rm -f -- "$next"
}

_dot_init_publish_one() {
  local transaction=$1 intent=$2 git_dir=$3 commit=$4 mode=$5 oid=$6 path=$7
  local target=$HOME/$7 phase stage_rel stage stage_dev stage_ino next_dev next_ino
  local next link_target identity

  _dot_init_parent_directories "$transaction" "$path" || return 1
  _dot_init_entry_intent "$intent" "$mode" "$oid" "$path" || return 1
  IFS=$'\t' read -r phase stage_rel stage_dev stage_ino next_dev next_ino <<<"$REPLY"
  stage=$HOME/$stage_rel
  next=$stage/next
  if [[ $phase == pending ]]; then
    if [[ -e $stage || -L $stage ]]; then
      _dot_init_entry_stage_valid "$stage" || return 1
      _dot_init_stage_claim_matches "$stage" entry "$path" || return 1
      _dot_init_entry_stage_only_next "$stage" || return 1
      [[ ! -e $next && ! -L $next ]] || return 1
    else
      mkdir "$stage" || return 1
      chmod 0700 "$stage" || return 1
      _dot_init_stage_claim_write "$stage" entry "$path" || return 1
    fi
    identity=$(_dot_path_identity "$stage") || return 1
    stage_dev=${identity%%:*}
    stage_ino=${identity#*:}
    # Bind the private container before redirection can create a partial next
    # file. A crash during `git show` can then be rolled back without treating
    # the incomplete bytes as a completed candidate generation.
    _dot_init_write_private_line "$intent" \
      "staged"$'\t'"$mode"$'\t'"$oid"$'\t'"$path"$'\t'"$stage_rel"$'\t'"$stage_dev"$'\t'"$stage_ino"$'\t-\t-' true || return 1
    phase=staged
  fi
  if [[ $phase == staged ]]; then
    _dot_init_entry_stage_valid "$stage" "$stage_dev:$stage_ino" || return 1
    _dot_init_stage_claim_matches "$stage" entry "$path" || return 1
    _dot_init_discard_staged_next "$stage" || return 1
    case $mode in
      100644 | 100755)
        git --git-dir="$git_dir" show "$commit:$path" >"$next" || return 1
        _dot_apply_tracked_file_mode "$next" "$mode" || return 1
        ;;
      120000)
        link_target=$(
          git --git-dir="$git_dir" show "$commit:$path"
          printf .
        ) || return 1
        link_target=${link_target%.}
        _dot_init_safe_value "$link_target" || return 1
        ln -s "$link_target" "$next" || return 1
        ;;
      *) return 1 ;;
    esac
    _dot_init_candidate_matches_git "$git_dir" "$commit" "$mode" "$oid" \
      "${next#"$HOME"/}" || return 1
    identity=$(_dot_path_identity "$next") || return 1
    next_dev=${identity%%:*}
    next_ino=${identity#*:}
    _dot_init_write_private_line "$intent" \
      "prepared"$'\t'"$mode"$'\t'"$oid"$'\t'"$path"$'\t'"$stage_rel"$'\t'"$stage_dev"$'\t'"$stage_ino"$'\t'"$next_dev"$'\t'"$next_ino" true || return 1
  fi
  _dot_init_entry_stage_valid "$stage" "$stage_dev:$stage_ino" || return 1
  _dot_init_stage_claim_matches "$stage" entry "$path" || return 1
  _dot_init_entry_stage_only_next "$stage" || return 1
  [[ $(_dot_path_identity "$next" 2>/dev/null || true) == "$next_dev:$next_ino" ]] ||
    return 1
  _dot_init_candidate_matches_git "$git_dir" "$commit" "$mode" "$oid" \
    "${next#"$HOME"/}" ||
    return 1
  _dot_move_noreplace "$next" "$target" || return 1
  [[ $(_dot_path_identity "$target" 2>/dev/null || true) == "$next_dev:$next_ino" ]] ||
    return 1
  _dot_init_stage_claim_remove "$stage" entry "$path" || return 1
  rmdir "$stage" 2>/dev/null || return 1
  _dot_init_candidate_matches_git "$git_dir" "$commit" "$mode" "$oid" "$path"
}

_dot_init_prior_record() {
  local prior=$1 wanted=$2 path kind dev ino mode size value
  while IFS=$'\t' read -r path kind dev ino mode size value; do
    [[ $path == "$wanted" ]] || continue
    REPLY="$kind"$'\t'"$dev"$'\t'"$ino"$'\t'"$mode"$'\t'"$size"$'\t'"$value"
    return 0
  done <"$prior"
  return 1
}

_dot_init_published_stage_matches() {
  local stage=$1 expected_identity=$2 path=$3 marker

  # Once the leaf is published, the prepared intent's recorded stage inode is
  # durable authority. The claim may still exist, or it may have been consumed
  # immediately before an interrupted rmdir; only an unchanged empty mode-700
  # directory is valid in the latter state.
  _dot_init_private_directory_matches "$stage" "$expected_identity" 700 || return 1
  _dot_init_entry_stage_only_next "$stage" || return 1
  [[ ! -e $stage/next && ! -L $stage/next ]] || return 1
  _dot_init_stage_claim_file "$stage"
  marker=$REPLY
  if [[ -e $marker || -L $marker ]]; then
    _dot_init_stage_claim_matches "$stage" entry "$path"
  else
    _dot_init_private_empty_directory_matches "$stage" "$expected_identity" 700
  fi
}

_dot_init_published_intent_matches() {
  local file=$1 mode=$2 oid=$3 path=$4 phase stage_rel stage_dev stage_ino next_dev next_ino
  local stage=$HOME
  _dot_init_entry_intent "$file" "$mode" "$oid" "$path" || return 1
  IFS=$'\t' read -r phase stage_rel stage_dev stage_ino next_dev next_ino <<<"$REPLY"
  [[ $phase == prepared &&
    $(_dot_path_identity "$HOME/$path" 2>/dev/null || true) == "$next_dev:$next_ino" ]] ||
    return 1
  stage=$HOME/$stage_rel
  if [[ -e $stage || -L $stage ]]; then
    _dot_init_published_stage_matches "$stage" "$stage_dev:$stage_ino" "$path" ||
      return 1
  fi
  REPLY="$stage"$'\t'"$stage_dev:$stage_ino"
}

_dot_init_cleanup_published_stage() {
  local stage=$1 expected_identity=$2 path=$3 marker
  [[ ! -e $stage && ! -L $stage ]] && return 0
  _dot_init_published_stage_matches "$stage" "$expected_identity" "$path" || return 1
  _dot_init_stage_claim_file "$stage"
  marker=$REPLY
  if [[ -e $marker || -L $marker ]]; then
    _dot_init_stage_claim_remove "$stage" entry "$path" || return 1
  fi
  _dot_init_private_empty_directory_matches "$stage" "$expected_identity" 700 || return 1
  rmdir "$stage"
}

_dot_init_publish_worktree() {
  local transaction=$1 tree prior
  local intents=$transaction/publish-intent mode oid path record kind dev ino old_mode size value
  local intent_hash intent_file published_stage published_stage_identity
  local git_dir=$DOT_INIT_GIT_DIR

  tree=$transaction/tree.tsv
  prior=$transaction/prior.tsv

  [[ -f $tree && -f $prior ]] || return 1
  while IFS=$'\t' read -r mode oid path; do
    _dot_init_prior_record "$prior" "$path" || return 1
    record=$REPLY
    IFS=$'\t' read -r kind dev ino old_mode size value <<<"$record"
    intent_hash=$(printf '%s' "$path" | git hash-object --stdin) || return 1
    intent_file=$intents.$intent_hash
    if _dot_init_candidate_matches_git "$git_dir" "$DOT_INIT_COMMIT" \
      "$mode" "$oid" "$path"; then
      if _dot_init_path_state_matches "$HOME/$path" "$kind" "$dev" "$ino" \
        "$old_mode" "$size" "$value"; then
        continue
      fi
      if _dot_init_published_intent_matches "$intent_file" "$mode" "$oid" "$path"; then
        IFS=$'\t' read -r published_stage published_stage_identity <<<"$REPLY"
        _dot_init_cleanup_published_stage "$published_stage" \
          "$published_stage_identity" "$path" || return 1
        continue
      fi
      return 1
    fi
    [[ ! -e $HOME/$path && ! -L $HOME/$path ]] || return 1
    if [[ $kind != absent ]]; then
      local conflict_root found=0 ckind cdev cino cmode csize cvalue
      while IFS=$'\t' read -r conflict_root ckind cdev cino cmode csize cvalue; do
        if [[ $path == "$conflict_root" || $path == "$conflict_root"/* ]]; then
          _dot_init_path_state_matches "$DOT_INIT_BACKUP/$conflict_root" \
            "$ckind" "$cdev" "$cino" "$cmode" "$csize" "$cvalue" || return 1
          found=1
          break
        fi
      done <"$transaction/conflicts.tsv"
      [[ $found -eq 1 ]] || return 1
    fi
    _dot_init_publish_intent "$intent_file" \
      "$mode" "$oid" "$path" || return 1
    _dot_init_publish_one "$transaction" "$intent_file" "$git_dir" \
      "$DOT_INIT_COMMIT" "$mode" "$oid" "$path" || return 1
  done <"$tree"

  git --git-dir="$git_dir" read-tree "$DOT_INIT_COMMIT" || return 1
  git --git-dir="$git_dir" update-ref "refs/heads/$DOT_INIT_BRANCH" \
    "$DOT_INIT_COMMIT" || return 1
  git --git-dir="$git_dir" symbolic-ref HEAD "refs/heads/$DOT_INIT_BRANCH" || return 1
  git --git-dir="$git_dir" update-index --refresh >/dev/null 2>&1 || true
}

_dot_init_forward_converge() {
  local status=0
  _dot_client_select
  dot_config_load || return 1
  if [[ ${DOT_INIT_SKIP_PROVIDER:-0} == 1 ]]; then
    # Shared bootstrap environments install their explicit prerequisites
    # separately from client policy. Keep this override invocation-scoped: the
    # committed config is still parsed and remains authoritative on later runs.
    local DOT_DEPENDENCY_PROVIDER=none
    export DOT_DEPENDENCY_PROVIDER
  fi
  _discover_overlays || return 1
  _preflight_local_overlays || return 1
  _ui_begin 5
  _run_pre_sync_extensions || return 1
  _dot_update_sync_repos || status=1
  _dot_update_finalize "$status"
}

_dot_init_single_origin() {
  local command_kind=$1 line
  local -a urls=()

  if [[ $command_kind == separate ]]; then
    mapfile -t urls < <(_base_git config --local --get-all remote.origin.url 2>/dev/null)
  else
    mapfile -t urls < <(git -C "$HOME" config --local --get-all remote.origin.url 2>/dev/null)
  fi
  [[ ${#urls[@]} -eq 1 ]] || return 1
  line=${urls[0]}
  _dot_init_safe_value "$line" || return 1
  printf '%s\n' "$line"
}

# Adopt a previously supported client layout only after the exact origin and
# active branch have been bound to the requested initialization identity.
# Returns 1 when no repository exists and 2 for a present but untrusted shape.
_dot_init_adopt_existing() {
  local origin=$1 identity=$2 branch=$3 topology='' git_dir='' recorded_origin=''
  local recorded_identity='' active_branch='' transaction record commit git_identity stage

  if _base_repo_exists; then
    topology=separate
    git_dir=$HOME/.dotfiles
  elif [[ -d $HOME/.git && ! -L $HOME/.git ]] &&
    [[ $(git -C "$HOME" rev-parse --show-toplevel 2>/dev/null) == "$HOME" ]]; then
    topology=ordinary
    git_dir=$HOME/.git
  else
    return 1
  fi
  recorded_origin=$(_dot_init_single_origin "$topology") || return 2
  recorded_identity=$(_dot_init_repo_identity "$recorded_origin") || return 2
  [[ $recorded_identity == "$identity" ]] || return 2
  if [[ $topology == separate ]]; then
    active_branch=$(_base_git symbolic-ref --short HEAD 2>/dev/null || true)
  else
    active_branch=$(git -C "$HOME" symbolic-ref --short HEAD 2>/dev/null || true)
  fi
  [[ $active_branch == "$branch" ]] || return 2
  if [[ $topology == separate ]]; then
    commit=$(_base_git rev-parse HEAD 2>/dev/null) || return 2
  else
    commit=$(git -C "$HOME" rev-parse HEAD 2>/dev/null) || return 2
  fi
  [[ $commit =~ ^[0-9a-fA-F]{40}$|^[0-9a-fA-F]{64}$ ]] || return 2
  git_identity=$(_dot_path_identity "$git_dir") || return 2
  DOT_INIT_COMMIT=$commit
  DOT_INIT_NONCE=adopted
  DOT_INIT_GIT_DEV=${git_identity%%:*}
  DOT_INIT_GIT_INO=${git_identity#*:}

  _dot_init_transaction_dir || return 2
  transaction=$REPLY
  _dot_init_prepare_transaction "$transaction" || return 2
  stage=$REPLY
  record=$stage/record
  _dot_init_write_record "$record" converging "$origin" "$identity" "$branch" - "$git_dir" ||
    return 2
  _dot_init_publish_transaction "$stage" "$transaction" || return 2
  record=$transaction/record
  # shellcheck disable=SC2034 # Consumed dynamically by repository helpers.
  DOT_BASE_TOPOLOGY=$topology
  # shellcheck disable=SC2034 # Consumed dynamically by repository helpers.
  DOT_CLIENT_GIT_DIR=$git_dir
  _dot_init_forward_converge || return 2
  _dot_init_write_record "$record" complete "$origin" "$identity" "$branch" - "$git_dir" ||
    return 2
  _dot_init_publish_completed "$record" || return 2
  rm -rf "$transaction"
}

_dot_init_usage() {
  cat <<'EOF'
usage: dot init [--branch BRANCH] [--yes] REPOSITORY_URL
       dot init --status
       dot init --rollback
EOF
}

_dot_init_status() {
  local transaction completed
  _dot_init_transaction_dir || return 1
  transaction=$REPLY
  _dot_init_completed_file || return 1
  completed=$REPLY
  if [[ -e $transaction || -L $transaction ]]; then
    _dot_init_read_record "$transaction/record" ||
      _dot_init_error "malformed initialization transaction: $transaction" || return
    printf 'initialization: incomplete\nphase: %s\norigin: %s\nbranch: %s\nbackup: %s\n' \
      "$DOT_INIT_PHASE" "$DOT_INIT_ORIGIN" "$DOT_INIT_BRANCH" "$DOT_INIT_BACKUP"
  elif [[ -e $completed || -L $completed ]]; then
    _dot_init_read_record "$completed" ||
      _dot_init_error "malformed completion record: $completed" || return
    printf 'initialization: complete\norigin: %s\nbranch: %s\n' \
      "$DOT_INIT_ORIGIN" "$DOT_INIT_BRANCH"
  else
    printf 'initialization: not started\n'
  fi
}

_dot_init_delete_park_path() {
  local target=$1 kind=$2 key=$3 hash parent

  case $kind in leaf | parent | git) ;; *) return 1 ;; esac
  hash=$(printf '%s\t%s' "$kind" "$key" | git hash-object --stdin) || return 1
  parent=${target%/*}
  [[ -n $parent && $parent != "$target" ]] || return 1
  REPLY=$parent/.dot-init-delete.$DOT_INIT_NONCE.$kind.$hash
}

_dot_init_leaf_delete_matches() {
  local candidate=$1 expected_identity=$2 git_dir=$3 commit=$4 mode=$5 oid=$6
  local relative

  [[ $(_dot_path_identity "$candidate" 2>/dev/null || true) == "$expected_identity" ]] ||
    return 1
  [[ $candidate == "$HOME/"* ]] || return 1
  relative=${candidate#"$HOME"/}
  _dot_init_candidate_matches_git "$git_dir" "$commit" "$mode" "$oid" "$relative"
}

_dot_init_parent_delete_matches() {
  local candidate=$1 expected_identity=$2 expected_mode=$3
  _dot_init_private_empty_directory_matches "$candidate" \
    "$expected_identity" "$expected_mode"
}

_dot_init_git_delete_matches() {
  local candidate=$1 expected_identity=$2
  [[ $(_dot_path_identity "$candidate" 2>/dev/null || true) == "$expected_identity" ]] ||
    return 1
  _dot_init_generation_matches "$candidate"
}

# Deletion by pathname is unsafe after validation because another process can
# replace that pathname before rm/rmdir runs. Select one generation first with
# an exclusive same-parent rename, validate the parked inode and contents, and
# remove only that parked generation. If validation fails, restore a generation
# parked by this invocation only while the original destination remains vacant.
_dot_init_delete_parked_generation() {
  local target=$1 park=$2 remover=$3 verifier=$4
  local parked_now=0 parked_identity='' target_won=0
  shift 4

  if [[ ! -e $park && ! -L $park ]]; then
    [[ -e $target || -L $target ]] || return 0
    _dot_move_noreplace "$target" "$park" || return 1
    parked_now=1
  fi
  if ! "$verifier" "$park" "$@"; then
    if [[ $parked_now -eq 1 && ! -e $target && ! -L $target ]]; then
      parked_identity=$(_dot_path_identity "$park") || return 1
      _dot_move_noreplace "$park" "$target" || return 1
      [[ $(_dot_path_identity "$target" 2>/dev/null || true) == "$parked_identity" ]] ||
        return 1
    fi
    return 1
  fi

  [[ ! -e $target && ! -L $target ]] || target_won=1
  # Revalidate immediately before removal so a changed parked generation is
  # preserved rather than deleted merely because it occupies the park name.
  "$verifier" "$park" "$@" || return 1
  case $remover in
    leaf) rm -f -- "$park" || return 1 ;;
    parent) rmdir "$park" || return 1 ;;
    tree) rm -rf -- "$park" || return 1 ;;
    *) return 1 ;;
  esac
  [[ ! -e $park && ! -L $park && $target_won -eq 0 ]]
}

_dot_init_rollback_entry() {
  local intent=$1 mode=$2 oid=$3 path=$4 phase stage_rel stage_dev stage_ino
  local next_dev next_ino stage target=$HOME/$4 park
  _dot_init_entry_intent "$intent" "$mode" "$oid" "$path" || return 1
  IFS=$'\t' read -r phase stage_rel stage_dev stage_ino next_dev next_ino <<<"$REPLY"
  stage=$HOME/$stage_rel
  _dot_init_delete_park_path "$target" leaf "$path" || return 1
  park=$REPLY
  if [[ -e $target || -L $target || -e $park || -L $park ]]; then
    [[ $phase == prepared ]] || return 1
    _dot_init_delete_parked_generation "$target" "$park" leaf \
      _dot_init_leaf_delete_matches "$next_dev:$next_ino" \
      "$DOT_INIT_GIT_DIR" "$DOT_INIT_COMMIT" "$mode" "$oid" || return 1
  fi
  if [[ -e $stage || -L $stage ]]; then
    case $phase in
      pending)
        _dot_init_entry_stage_valid "$stage" || return 1
        _dot_init_stage_claim_matches "$stage" entry "$path" || return 1
        _dot_init_entry_stage_only_next "$stage" || return 1
        [[ ! -e $stage/next && ! -L $stage/next ]] || return 1
        ;;
      staged)
        _dot_init_entry_stage_valid "$stage" "$stage_dev:$stage_ino" || return 1
        _dot_init_stage_claim_matches "$stage" entry "$path" || return 1
        _dot_init_discard_staged_next "$stage" || return 1
        ;;
      prepared)
        _dot_init_entry_stage_valid "$stage" "$stage_dev:$stage_ino" || return 1
        _dot_init_stage_claim_matches "$stage" entry "$path" || return 1
        _dot_init_entry_stage_only_next "$stage" || return 1
        if [[ -e $stage/next || -L $stage/next ]]; then
          [[ $(_dot_path_identity "$stage/next" 2>/dev/null || true) == "$next_dev:$next_ino" ]] ||
            return 1
          _dot_init_candidate_matches_git "$DOT_INIT_GIT_DIR" "$DOT_INIT_COMMIT" \
            "$mode" "$oid" "${stage#"$HOME"/}/next" || return 1
          rm -f -- "$stage/next" || return 1
        fi
        ;;
      *) return 1 ;;
    esac
    _dot_init_stage_claim_remove "$stage" entry "$path" || return 1
    rmdir "$stage" || return 1
  fi
}

_dot_init_rollback_parents() {
  local transaction=$1 file line parent phase stage_rel dev ino mode target stage park
  local nullglob_was_set=0
  local -a files=() records=()
  shopt -q nullglob && nullglob_was_set=1
  shopt -s nullglob
  files=("$transaction"/parent-intent.*)
  [[ $nullglob_was_set -eq 1 ]] || shopt -u nullglob
  for file in "${files[@]+"${files[@]}"}"; do
    [[ ${file##*/} != parent-intent.*.tmp.* ]] || continue
    line=$(<"$file")
    IFS=$'\t' read -r phase parent _ <<<"$line"
    [[ $phase == pending || $phase == prepared ]] || return 1
    _dot_init_safe_relative_path "$parent" || return 1
    records+=("$parent"$'\t'"$file")
  done
  while IFS=$'\t' read -r parent file; do
    [[ -n $parent && -n $file ]] || continue
    _dot_init_parent_record "$transaction" "$parent" || return 1
    IFS=$'\t' read -r phase stage_rel dev ino mode <<<"$REPLY"
    target=$HOME/$parent
    stage=$HOME/$stage_rel
    _dot_init_delete_park_path "$target" parent "$parent" || return 1
    park=$REPLY
    if [[ -e $target || -L $target || -e $park || -L $park ]]; then
      [[ $phase == prepared && ! -e $stage && ! -L $stage ]] || return 1
      _dot_init_delete_parked_generation "$target" "$park" parent \
        _dot_init_parent_delete_matches "$dev:$ino" "$mode" || return 1
    fi
    if [[ -e $stage || -L $stage ]]; then
      [[ ! -e $target && ! -L $target ]] || return 1
      if [[ $phase == prepared ]]; then
        _dot_init_private_directory_matches "$stage" "$dev:$ino" "$mode" || return 1
        if [[ -e $stage/.dot-init-stage-claim-v1 || -L $stage/.dot-init-stage-claim-v1 ]]; then
          _dot_init_stage_claim_only "$stage" || return 1
          _dot_init_stage_claim_remove "$stage" parent "$parent" || return 1
        fi
        _dot_init_private_empty_directory_matches "$stage" "$dev:$ino" "$mode" || return 1
      else
        _dot_init_private_directory_matches "$stage" || return 1
        _dot_init_stage_claim_only "$stage" || return 1
        _dot_init_stage_claim_remove "$stage" parent "$parent" || return 1
        _dot_init_private_empty_directory_matches "$stage" || return 1
      fi
      rmdir "$stage" || return 1
    fi
  done < <(printf '%s\n' "${records[@]+"${records[@]}"}" | LC_ALL=C sort -r)
}

_dot_init_rollback_published() {
  local transaction=$1 tree mode oid path hash intent git_park
  tree=$transaction/tree.tsv
  if [[ -f $tree ]]; then
    while IFS=$'\t' read -r mode oid path; do
      hash=$(printf '%s' "$path" | git hash-object --stdin) || return 1
      intent=$transaction/publish-intent.$hash
      [[ -f $intent && ! -L $intent ]] || continue
      _dot_init_rollback_entry "$intent" "$mode" "$oid" "$path" || return 1
    done < <(LC_ALL=C sort -r -t $'\t' -k3,3 "$tree")
    _dot_init_rollback_parents "$transaction" || return 1
  fi
  _dot_init_delete_park_path "$DOT_INIT_GIT_DIR" git "$DOT_INIT_GIT_DIR" || return 1
  git_park=$REPLY
  if [[ -e $DOT_INIT_GIT_DIR || -L $DOT_INIT_GIT_DIR ||
    -e $git_park || -L $git_park ]]; then
    _dot_init_delete_parked_generation "$DOT_INIT_GIT_DIR" "$git_park" tree \
      _dot_init_git_delete_matches "$DOT_INIT_GIT_DEV:$DOT_INIT_GIT_INO" || return 1
  fi
  local container=$DOT_INIT_BACKUP/git-stage
  if [[ -e $container || -L $container ]]; then
    [[ -d $container && ! -L $container && -f $container/identity ]] || return 1
    grep -Fqx "nonce=$DOT_INIT_NONCE" "$container/identity" || return 1
    rm -rf -- "$container" || return 1
  fi
}

_dot_init_rollback() {
  local transaction record
  _dot_init_transaction_dir || return 1
  transaction=$REPLY
  record=$transaction/record
  _dot_init_read_record "$record" || _dot_init_error 'no recoverable transaction'
  [[ $DOT_INIT_PHASE != checkout && $DOT_INIT_PHASE != converging &&
    $DOT_INIT_PHASE != complete ]] ||
    _dot_init_error 'checkout is committed; rerun the original init command to resume'
  _dot_init_rollback_published "$transaction" ||
    _dot_init_error 'transaction-owned paths changed; refusing rollback' || return
  _dot_init_restore_backups "$DOT_INIT_BACKUP" || return 1
  rm -rf "$transaction"
}

_dot_init_live_git_matches_record() {
  local git_dir=$DOT_INIT_GIT_DIR identity urls=() origin branch commit current_identity
  local worktree home_real worktree_real

  [[ -d $git_dir && ! -L $git_dir ]] || return 1
  current_identity=$(_dot_path_identity "$git_dir") || return 1
  [[ $current_identity == "$DOT_INIT_GIT_DEV:$DOT_INIT_GIT_INO" ]] || return 1
  if [[ $DOT_INIT_NONCE != adopted ]]; then
    _dot_init_generation_marker_matches "$git_dir" || return 1
  fi
  mapfile -t urls < <(git --git-dir="$git_dir" config --get-all remote.origin.url 2>/dev/null)
  [[ ${#urls[@]} -eq 1 ]] || return 1
  origin=${urls[0]}
  identity=$(_dot_init_repo_identity "$origin") || return 1
  [[ $identity == "$DOT_INIT_IDENTITY" ]] || return 1
  branch=$(git --git-dir="$git_dir" symbolic-ref --short HEAD 2>/dev/null) || return 1
  [[ $branch == "$DOT_INIT_BRANCH" ]] || return 1
  commit=$(git --git-dir="$git_dir" rev-parse HEAD 2>/dev/null) || return 1
  [[ $commit =~ ^[0-9a-fA-F]{40}$|^[0-9a-fA-F]{64}$ ]] || return 1
  if [[ $git_dir == "$HOME/.dotfiles" ]]; then
    case $(git --git-dir="$git_dir" config --bool core.bare 2>/dev/null) in
      true) ;;
      false)
        [[ $(git --git-dir="$git_dir" config core.worktree 2>/dev/null) == "$HOME" ]] ||
          return 1
        ;;
      *) return 1 ;;
    esac
  elif [[ $git_dir == "$HOME/.git" ]]; then
    worktree=$(git -C "$HOME" rev-parse --show-toplevel 2>/dev/null) || return 1
    home_real=$(cd -P -- "$HOME" 2>/dev/null && pwd -P) || return 1
    worktree_real=$(cd -P -- "$worktree" 2>/dev/null && pwd -P) || return 1
    [[ $worktree_real == "$home_real" ]] || return 1
  else
    return 1
  fi
}

_dot_init_resume_transaction() {
  local transaction=$1 record=$2

  case $DOT_INIT_PHASE in
    prepared | backing-up | backed-up | git-staging | git-staged | publishing)
      [[ -f $transaction/tree.tsv && -f $transaction/prior.tsv &&
        -f $transaction/conflicts.tsv ]] || return 1
      _dot_init_record_phase "$record" backing-up || return 1
      _dot_init_move_conflicts "$transaction/conflicts.tsv" "$DOT_INIT_BACKUP" || return 1
      _dot_init_record_phase "$record" backed-up || return 1
      _dot_init_stage_git "$record" || return 1
      _dot_init_publish_git "$record" || return 1
      _dot_init_publish_worktree "$transaction" || return 1
      _dot_init_record_phase "$record" checkout || return 1
      if [[ -d $DOT_INIT_BACKUP/git-stage &&
        -f $DOT_INIT_BACKUP/git-stage/identity ]]; then
        grep -Fqx "nonce=$DOT_INIT_NONCE" "$DOT_INIT_BACKUP/git-stage/identity" ||
          return 1
        rm -rf -- "$DOT_INIT_BACKUP/git-stage" || return 1
      fi
      ;;
    checkout | converging)
      _dot_init_live_git_matches_record || return 1
      ;;
    complete)
      _dot_init_live_git_matches_record || return 1
      _dot_init_publish_completed "$record" || return 1
      rm -rf -- "$transaction"
      return 0
      ;;
    *) return 1 ;;
  esac

  _dot_init_record_phase "$record" converging || return 1
  _dot_init_forward_converge || return 1
  _dot_init_record_phase "$record" complete || return 1
  _dot_init_publish_completed "$record" || return 1
  rm -rf -- "$transaction"
}

dot_init_command() {
  local branch='' yes=false mode=run origin='' identity transaction record
  local state_root candidate tree prior conflicts backup completed commit

  while (($#)); do
    case $1 in
      --help | -h)
        _dot_init_usage
        return 0
        ;;
      --yes)
        yes=true
        shift
        ;;
      --branch)
        (($# >= 2)) || return 2
        branch=$2
        shift 2
        ;;
      --status)
        mode=status
        shift
        ;;
      --rollback)
        mode=rollback
        shift
        ;;
      --*)
        _dot_init_error "unknown option: $1"
        return 2
        ;;
      *)
        [[ -z $origin ]] || return 2
        origin=$1
        shift
        ;;
    esac
  done
  case $mode in
    status)
      [[ -z $origin && -z $branch ]] || return 2
      _dot_init_status
      return
      ;;
    rollback)
      [[ -z $origin && -z $branch ]] || return 2
      _dot_init_rollback
      return
      ;;
  esac
  case ${DOT_INIT_SKIP_PROVIDER:-0} in
    0 | 1) ;;
    *)
      printf 'dot init: DOT_INIT_SKIP_PROVIDER must be 0 or 1\n' >&2
      return 2
      ;;
  esac
  [[ -n $origin ]] || {
    _dot_init_usage >&2
    return 2
  }
  identity=$(_dot_init_repo_identity "$origin") ||
    _dot_init_error "unsupported repository URL: $origin" || return
  if [[ -z $branch ]]; then
    branch=$(_dot_init_remote_default_branch "$origin") ||
      _dot_init_error 'could not resolve a non-empty remote default branch' || return
  fi
  _dot_init_branch_valid "$branch" || _dot_init_error "invalid branch: $branch" || return

  _dot_init_transaction_dir || return 1
  transaction=$REPLY
  record=$transaction/record
  _dot_init_recover_transaction_stages "$transaction" || return 1
  if [[ -e $transaction || -L $transaction ]]; then
    _dot_init_read_record "$record" ||
      _dot_init_error "malformed initialization transaction: $transaction" || return
    [[ $DOT_INIT_IDENTITY == "$identity" && $DOT_INIT_BRANCH == "$branch" ]] ||
      _dot_init_error 'existing transaction belongs to a different repository or branch' || return
    _dot_init_resume_transaction "$transaction" "$record" ||
      _dot_init_error 'initialization transaction could not be resumed safely' || return
    return 0
  fi

  _dot_init_completed_file || return 1
  completed=$REPLY
  if [[ -e $completed || -L $completed ]]; then
    _dot_init_read_record "$completed" ||
      _dot_init_error "malformed completion record: $completed" || return
    [[ $DOT_INIT_IDENTITY == "$identity" && $DOT_INIT_BRANCH == "$branch" ]] ||
      _dot_init_error 'initialized client belongs to a different repository or branch' || return
    [[ $DOT_INIT_PHASE == complete ]] ||
      _dot_init_error 'completion record is not in the complete phase' || return
    if [[ $DOT_INIT_GIT_DIR == "$HOME/.dotfiles" &&
      ! -e $DOT_INIT_GIT_DIR && ! -L $DOT_INIT_GIT_DIR &&
      ! -e $HOME/.git && ! -L $HOME/.git ]]; then
      # Removing the separate Git directory is the documented manual recovery
      # boundary. Retire only this already-validated completion record, then
      # let the normal transaction rebuild the client while preserving every
      # existing worktree path through the conflict backup.
      rm -f -- "$completed" || return 1
    else
      _dot_init_live_git_matches_record ||
        _dot_init_error 'initialized client Git generation no longer matches its record' || return
      _dot_init_forward_converge
      return
    fi
  fi
  local adoption_rc=0
  _dot_init_adopt_existing "$origin" "$identity" "$branch" || adoption_rc=$?
  if [[ $adoption_rc -eq 0 ]]; then
    return 0
  elif [[ $adoption_rc -eq 2 ]]; then
    _dot_init_error 'existing client repository does not match the requested origin and branch'
    return 1
  fi

  _dot_init_state_root || return 1
  state_root=$REPLY
  _dot_init_private_directory "$state_root" || return 1
  candidate=$(mktemp -d "$state_root/.candidate.XXXXXX") || return 1
  chmod 0700 "$candidate"
  git clone --quiet --no-checkout --branch "$branch" --single-branch -- "$origin" "$candidate" || {
    rm -rf "$candidate"
    return 1
  }
  tree=$candidate/tree.tsv
  prior=$candidate/prior.tsv
  conflicts=$candidate/conflicts.tsv
  _dot_init_candidate_tree "$candidate" "$branch" "$tree" || {
    rm -rf "$candidate"
    _dot_init_error 'candidate tree is empty, unsafe, or contains unsupported entries'
    return 1
  }
  commit=$(git -C "$candidate" rev-parse "$branch^{commit}" 2>/dev/null) || {
    rm -rf "$candidate"
    return 1
  }
  [[ $commit =~ ^[0-9a-fA-F]{40}$|^[0-9a-fA-F]{64}$ ]] || return 1
  _dot_init_build_prior_and_conflicts "$candidate" "$branch" "$tree" "$prior" \
    "$conflicts" || return 1
  backup=$HOME/.dot-backup/$(date +%Y%m%d%H%M%S)-$$
  _dot_init_plan_summary "$candidate" "$branch" "$tree" "$backup" "$identity" || {
    rm -rf "$candidate"
    _dot_init_error 'candidate configuration is invalid'
    return 1
  }
  _dot_init_confirm "$conflicts" "$yes" || {
    rm -rf "$candidate"
    return 1
  }

  local stage
  _dot_init_prepare_transaction "$transaction" || return 1
  stage=$REPLY
  cp "$tree" "$stage/tree.tsv" || return 1
  cp "$prior" "$stage/prior.tsv" || return 1
  cp "$conflicts" "$stage/conflicts.tsv" || return 1
  chmod 0600 "$stage/tree.tsv" "$stage/prior.tsv" \
    "$stage/conflicts.tsv" || return 1
  DOT_INIT_PHASE=prepared
  DOT_INIT_ORIGIN=$origin
  DOT_INIT_IDENTITY=$identity
  DOT_INIT_BRANCH=$branch
  DOT_INIT_COMMIT=$commit
  DOT_INIT_GIT_DIR=$HOME/.dotfiles
  DOT_INIT_WORKTREE=$HOME
  DOT_INIT_BACKUP=$backup
  DOT_INIT_NONCE="$(date +%s).$$.$RANDOM"
  DOT_INIT_GIT_DEV=-
  DOT_INIT_GIT_INO=-
  _dot_init_write_record "$stage/record" prepared "$origin" "$identity" \
    "$branch" "$backup" || return 1
  _dot_init_publish_transaction "$stage" "$transaction" || return 1
  record=$transaction/record
  rm -rf "$candidate"
  _dot_init_resume_transaction "$transaction" "$record"
}
