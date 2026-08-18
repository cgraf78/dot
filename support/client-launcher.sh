#!/usr/bin/env bash
# Generated client-owned adapter for deployments that retain a regular
# ~/.local/bin/dot file. During the migration window it prefers the validated
# standalone checkout and otherwise invokes the retained embedded launcher.
# After that fallback is retired, the same bytes print the recovery installer.

set -euo pipefail
CDPATH=
shopt -u nocasematch 2>/dev/null || true

dot_client_error() {
  printf 'dot: %s\n' "$*" >&2
}

dot_client_normalized_absolute() {
  case $1 in
    '' | / | */ | *//* | */./* | */. | */../* | */.. | *$'\n'* | *$'\r'*)
      return 1
      ;;
    /*) return 0 ;;
    *) return 1 ;;
  esac
}

dot_client_state_home() {
  local value=${XDG_STATE_HOME:-}

  case $value in
    /) DOT_CLIENT_STATE_HOME=/ ;;
    /*)
      dot_client_normalized_absolute "$value" || return 1
      DOT_CLIENT_STATE_HOME=$value
      ;;
    *)
      DOT_CLIENT_STATE_HOME=$HOME/.local/state
      dot_client_normalized_absolute "$DOT_CLIENT_STATE_HOME"
      ;;
  esac
}

dot_client_private_file() {
  local path=$1 output uid mode links

  [[ -f $path && ! -L $path ]] || return 1
  if output=$(stat -c '%u %a %h' "$path" 2>/dev/null); then
    :
  elif output=$(stat -f '%u %Lp %l' "$path" 2>/dev/null); then
    :
  else
    return 1
  fi
  read -r uid mode links <<<"$output"
  [[ $uid == "$(id -u)" && $mode == 600 && $links == 1 ]]
}

dot_client_config_path() {
  local value=${XDG_CONFIG_HOME:-}

  case $value in
    /) DOT_CLIENT_CONFIG_PATH=/dot/config ;;
    /*)
      dot_client_normalized_absolute "$value" || return 1
      DOT_CLIENT_CONFIG_PATH=$value/dot/config
      ;;
    *)
      DOT_CLIENT_CONFIG_PATH=$HOME/.config/dot/config
      dot_client_normalized_absolute "$DOT_CLIENT_CONFIG_PATH"
      ;;
  esac
}

dot_client_resolve_checkout() {
  local install_home=${SHDEPS_INSTALL_DIR:-$HOME/.local/share}

  while [[ $install_home != / && $install_home == */ ]]; do
    install_home=${install_home%/}
  done
  case $install_home in
    '' | *//* | */./* | */. | */../* | */.. | *$'\n'* | *$'\r'*)
      return 1
      ;;
    /) DOT_CLIENT_CHECKOUT=/cgraf78/dot ;;
    /*) DOT_CLIENT_CHECKOUT=$install_home/cgraf78/dot ;;
    *) return 1 ;;
  esac
  DOT_CLIENT_RUNTIME=$DOT_CLIENT_CHECKOUT/bin/dot
}

dot_client_read_lock() {
  local LC_ALL=C
  local lock=$1 size header phase_line revision_line generation_line extra=''

  [[ -f "$lock" && ! -L "$lock" ]] || return 1
  size=$(wc -c <"$lock" 2>/dev/null) || return 2
  size=${size//[[:space:]]/}
  case $size in
    '' | *[!0-9]*) return 2 ;;
  esac
  [[ ${#size} -le 4 && $size -le 1024 ]] || return 2
  {
    IFS= read -r header || return 2
    IFS= read -r phase_line || return 2
    IFS= read -r revision_line || return 2
    IFS= read -r generation_line || return 2
    if IFS= read -r extra || [[ -n $extra ]]; then
      return 2
    fi
  } <"$lock"
  [[ $header == 'cgraf78 dot client cutover v4' &&
    $phase_line == phase=* &&
    $revision_line == minimum_revision=* &&
    $generation_line == readiness_generation=* ]] || return 2
  DOT_CLIENT_PHASE=${phase_line#phase=}
  case $DOT_CLIENT_PHASE in
    prepare | active) ;;
    *) return 2 ;;
  esac
  DOT_CLIENT_MINIMUM_REVISION=${revision_line#minimum_revision=}
  [[ $DOT_CLIENT_MINIMUM_REVISION =~ ^[0-9a-f]{40}$ ]] || return 2
  DOT_CLIENT_READINESS_GENERATION=${generation_line#readiness_generation=}
  [[ $DOT_CLIENT_READINESS_GENERATION =~ ^[0-9a-z][0-9a-z-]{0,63}$ ]] || return 2
}

dot_client_path_within() {
  local path=$1 root=$2

  [[ $root == / || $path == "$root" || $path == "$root/"* ]]
}

dot_client_select_host_git() {
  local checkout_physical=$1 home_physical directory physical_directory candidate
  local -a path_directories=()

  home_physical=$(cd -P -- "$HOME" 2>/dev/null && pwd -P) || return 1
  if [[ $home_physical != / ]]; then
    dot_client_normalized_absolute "$home_physical" || return 1
  fi
  IFS=: read -r -a path_directories <<<"${PATH:-}"
  for directory in "${path_directories[@]}"; do
    # PATH remains the host/toolchain trust boundary. During this temporary
    # migration, skip relative entries and anything physically below HOME or
    # the managed checkout, where client launchers and unchecked code live.
    [[ $directory == /* ]] || continue
    physical_directory=$(cd -P -- "$directory" 2>/dev/null && pwd -P) ||
      continue
    if [[ $physical_directory == / ]]; then
      candidate=/git
    else
      dot_client_normalized_absolute "$physical_directory" || continue
      candidate=$physical_directory/git
    fi
    dot_client_normalized_absolute "$candidate" || continue
    # A symlink target cannot be inspected without another external tool.
    # Skip it and continue scanning for a native host Git; this keeps macOS,
    # Linux, and Termux path discovery data-driven and fails closed otherwise.
    [[ -f $candidate && ! -L $candidate && -x $candidate ]] || continue
    dot_client_path_within "$candidate" "$home_physical" && continue
    dot_client_path_within "$candidate" "$checkout_physical" && continue
    DOT_CLIENT_GIT=$candidate
    return 0
  done
  return 1
}

dot_client_git() (
  unset GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR GIT_OBJECT_DIRECTORY
  unset GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_INDEX_FILE GIT_CONFIG
  unset GIT_CONFIG_GLOBAL GIT_CONFIG_SYSTEM GIT_CONFIG_COUNT
  unset GIT_CONFIG_PARAMETERS GIT_EXEC_PATH GIT_NAMESPACE GIT_SHALLOW_FILE
  unset GIT_GRAFT_FILE GIT_REPLACE_REF_BASE
  unset GIT_EXTERNAL_DIFF GIT_DIFF_OPTS GIT_PREFIX GIT_INTERNAL_SUPER_PREFIX
  export GIT_NO_LAZY_FETCH=1 GIT_NO_REPLACE_OBJECTS=1
  exec "$DOT_CLIENT_GIT" "$@"
)

dot_client_git_metadata_safe() {
  local checkout=$1 git_dir=$1/.git commondir=$1/.git/commondir
  local grafts=$1/.git/info/grafts replacements

  # A managed install is a standalone clone, never a linked worktree. Keep its
  # admin directory local and reject both replacement mechanisms. The Git
  # environment still disables replace refs so even the metadata probe cannot
  # consume them before this explicit fail-closed check.
  [[ -d $git_dir && ! -L $git_dir && -O $git_dir ]] || return 1
  [[ ! -e $commondir && ! -L $commondir ]] || return 1
  [[ ! -e $grafts && ! -L $grafts ]] || return 1
  replacements=$(dot_client_git -C "$checkout" for-each-ref --count=1 \
    --format='%(refname)' refs/replace/ 2>/dev/null) || return 1
  [[ -z $replacements ]]
}

dot_client_exact_file() {
  local expected=$1 actual=$2

  # Git is the launcher's existing trust primitive and is present on minimal
  # bootstrap images that omit coreutils `cmp`. Suppress configurable diff
  # delegates so this remains a literal byte comparison.
  dot_client_git --no-pager diff --no-index --quiet --no-ext-diff --no-textconv \
    -- "$expected" "$actual" 2>/dev/null
}

dot_client_exact_link() {
  local path=$1 expected=$2 actual

  [[ -L $path ]] || return 1
  actual=$(readlink "$path") || return 1
  [[ $actual == "$expected" ]]
}

dot_client_read_ready() {
  local ready=$1 size header config_line oid_line extra='' actual_oid

  dot_client_private_file "$ready" || return 1
  size=$(wc -c <"$ready" 2>/dev/null) || return 1
  size=${size//[[:space:]]/}
  case $size in
    '' | *[!0-9]*) return 1 ;;
  esac
  [[ ${#size} -le 4 && $size -le 4096 ]] || return 1
  {
    IFS= read -r header || return 1
    IFS= read -r config_line || return 1
    IFS= read -r oid_line || return 1
    if IFS= read -r extra || [[ -n $extra ]]; then
      return 1
    fi
  } <"$ready"
  [[ $header == 'cgraf78 dot client readiness v4' &&
    $config_line == config_path=* && $oid_line == config_oid=* ]] || return 1
  [[ ${config_line#config_path=} == "$DOT_CLIENT_CONFIG_PATH" ]] || return 1
  DOT_CLIENT_CONFIG_OID=${oid_line#config_oid=}
  [[ $DOT_CLIENT_CONFIG_OID =~ ^[0-9a-f]{40}$ ]] || return 1
  # The writer validated a regular config file. Recheck that type before Git
  # hashes current bytes; hash-object follows symlinks, while the runtime's
  # strict config parser deliberately rejects them.
  [[ -f $DOT_CLIENT_CONFIG_PATH && ! -L $DOT_CLIENT_CONFIG_PATH ]] || return 1
  actual_oid=$(dot_client_git -C "$DOT_CLIENT_CHECKOUT" hash-object \
    --no-filters -- "$DOT_CLIENT_CONFIG_PATH" 2>/dev/null) || return 1
  [[ $actual_oid == "$DOT_CLIENT_CONFIG_OID" ]]
}

dot_client_safe_tracked_path() {
  local path=$1 component
  local -a components=()

  case $path in
    '' | /* | . | .. | ./* | ../* | */./* | */../* | */. | */.. | */ | *//* | *$'\t'* | *$'\n'* | *$'\r'*)
      return 1
      ;;
  esac
  IFS=/ read -r -a components <<<"$path"
  for component in "${components[@]}"; do
    [[ $component != .git ]] || return 1
  done
}

dot_client_tracked_parents_regular() {
  local checkout=$1 path=$2 component index limit prefix=''
  local -a components=()

  IFS=/ read -r -a components <<<"$path"
  limit=$((${#components[@]} - 1))
  for ((index = 0; index < limit; index++)); do
    component=${components[$index]}
    if [[ -n $prefix ]]; then
      prefix=$prefix/$component
    else
      prefix=$component
    fi
    [[ -d $checkout/$prefix && ! -L $checkout/$prefix ]] || return 1
  done
}

dot_client_tracked_tree_matches() {
  local LC_ALL=C
  local checkout=$1 head=$2 entry header path mode type oid extra=''
  local actual hash_complete=0 hash_count=0 tree_complete=0
  local count=0 valid=1
  local -a paths=() expected_oids=()

  # The managed runtime currently tracks regular files only. Keep that narrow
  # contract fail-closed so a new type or pathname requires deliberate review.
  # The final empty record proves that ls-tree completed; process substitution
  # otherwise does not expose whether its producer yielded only a partial tree.
  while IFS= read -r -d '' entry; do
    if [[ -z $entry ]]; then
      tree_complete=1
      continue
    fi
    [[ $tree_complete -eq 0 && $entry == *$'\t'* ]] || return 1
    header=${entry%%$'\t'*}
    path=${entry#*$'\t'}
    read -r mode type oid extra <<<"$header"
    [[ -z $extra && $type == blob &&
      $mode =~ ^(100644|100755)$ && $oid =~ ^[0-9a-f]{40}$ ]] || return 1
    dot_client_safe_tracked_path "$path" || return 1
    dot_client_tracked_parents_regular "$checkout" "$path" || return 1
    actual=$checkout/$path
    [[ -f $actual && ! -L $actual && -O $actual ]] || return 1
    case $mode in
      100644) [[ ! -x $actual ]] || return 1 ;;
      100755) [[ -x $actual ]] || return 1 ;;
    esac
    paths[count]=$path
    expected_oids[count]=$oid
    count=$((count + 1))
  done < <(
    if dot_client_git -C "$checkout" ls-tree -r -z --full-tree \
      "$head" 2>/dev/null; then
      printf '\0'
    fi
  )
  [[ $tree_complete -eq 1 && $count -gt 0 ]] || return 1

  # Hash all literal worktree bytes in one Git process. Besides avoiding clean
  # filters, batching keeps this per-command authority check inexpensive.
  while IFS= read -r actual; do
    if [[ -z $actual ]]; then
      hash_complete=1
      continue
    fi
    if [[ $hash_complete -eq 1 || $hash_count -ge $count ||
      $actual != "${expected_oids[$hash_count]}" ]]; then
      valid=0
    fi
    hash_count=$((hash_count + 1))
  done < <(
    if printf '%s\n' "${paths[@]}" |
      dot_client_git -C "$checkout" hash-object --no-filters \
        --stdin-paths 2>/dev/null; then
      printf '\n'
    fi
  )
  [[ $hash_complete -eq 1 && $valid -eq 1 && $hash_count -eq $count ]]
}

dot_client_checkout_authorized() {
  local checkout=$1 launcher=$2 template
  local checkout_physical top top_physical head

  [[ -d "$checkout" && ! -L "$checkout" && -O "$checkout" ]] || return 1
  checkout_physical=$(cd -P -- "$checkout" 2>/dev/null && pwd -P) || return 1
  dot_client_normalized_absolute "$checkout_physical" || return 1
  template=$checkout/support/client-launcher.sh
  [[ -f "$template" && ! -L "$template" ]] || return 1
  dot_client_select_host_git "$checkout_physical" || return 1
  dot_client_git_metadata_safe "$checkout" || return 1
  dot_client_exact_file "$launcher" "$template" || return 1
  top=$(dot_client_git -C "$checkout" rev-parse --show-toplevel 2>/dev/null) ||
    return 1
  top_physical=$(cd -P -- "$top" 2>/dev/null && pwd -P) || return 1
  [[ $top_physical == "$checkout_physical" ]] || return 1
  head=$(dot_client_git -C "$checkout" rev-parse --verify 'HEAD^{commit}' 2>/dev/null) ||
    return 1
  [[ $head =~ ^[0-9a-f]{40}$ ]] || return 1
  if ! dot_client_git -C "$checkout" merge-base --is-ancestor \
    "$DOT_CLIENT_MINIMUM_REVISION" "$head" >/dev/null 2>&1; then
    return 1
  fi
  # Readiness proves convergence at one point in time. Recheck the complete
  # tracked tree immediately before dispatch so later content or type changes
  # cannot become the standalone execution authority.
  dot_client_tracked_tree_matches "$checkout" "$head"
}

dot_client_standalone_ready() {
  local checkout=$1 runtime=$2 launcher=$3 ready=$4

  [[ -f "$runtime" && ! -L "$runtime" && -x "$runtime" ]] || return 1
  dot_client_checkout_authorized "$checkout" "$launcher" || return 1
  dot_client_config_path || return 1
  dot_client_read_ready "$ready" || return 1
  dot_client_exact_link "$HOME/.local/lib/dot" "$checkout/lib/dot/public"
}

client_validation_mode=0
if [[ ${1:-} == __client ]]; then
  # Temporary fleet migration API. Keep one exact silent command so the
  # preparation writer can reuse checkout authority without loading any bytes
  # from the checkout or exposing a general-purpose internal command surface.
  [[ $# -eq 2 ]] || exit 2
  [[ $2 == validate-checkout ]] || exit 2
  client_validation_mode=1
fi

[[ -n ${HOME:-} ]] || {
  [[ $client_validation_mode -eq 0 ]] || exit 1
  dot_client_error 'HOME is not set'
  exit 1
}

launcher_source=${BASH_SOURCE[0]}
case $launcher_source in
  /* | */*) launcher_directory=${launcher_source%/*} ;;
  *) launcher_directory=. ;;
esac
[[ -n $launcher_directory ]] || launcher_directory=/
launcher_parent=$(cd -P -- "$launcher_directory" 2>/dev/null && pwd -P) || {
  [[ $client_validation_mode -eq 0 ]] || exit 1
  dot_client_error 'cannot resolve the client launcher'
  exit 1
}
launcher=$launcher_parent/${BASH_SOURCE[0]##*/}
[[ -f "$launcher" && ! -L "$launcher" ]] || {
  [[ $client_validation_mode -eq 0 ]] || exit 1
  dot_client_error "client launcher is not a regular file: $launcher"
  exit 1
}

cutover_lock=${DOT_CLIENT_CUTOVER_LOCK:-$HOME/.local/lib/dotfiles/dot-cutover.lock}
if [[ $client_validation_mode -eq 1 ]]; then
  dot_client_normalized_absolute "$cutover_lock" || exit 1
  dot_client_read_lock "$cutover_lock" || exit 1
  dot_client_resolve_checkout || exit 1
  dot_client_checkout_authorized "$DOT_CLIENT_CHECKOUT" "$launcher" || exit 1
  exit 0
fi

handoff_helper=${DOT_CLIENT_HANDOFF_HELPER:-$HOME/.local/lib/dotfiles/dot-library-handoff.sh}
legacy_launcher=${DOT_CLIENT_LEGACY_LAUNCHER:-$HOME/.local/lib/dotfiles/legacy-dot-launcher.sh}
ready_override=${DOT_CLIENT_READY:-}
for client_path in "$cutover_lock" "$handoff_helper" "$legacy_launcher"; do
  dot_client_normalized_absolute "$client_path" || {
    dot_client_error "client migration path must be normalized and absolute: $client_path"
    exit 1
  }
done

if [[ -e "$handoff_helper" || -L "$handoff_helper" ]]; then
  [[ -f "$handoff_helper" && ! -L "$handoff_helper" && -x "$handoff_helper" ]] || {
    dot_client_error "library handoff helper is unsafe: $handoff_helper"
    exit 1
  }
fi

standalone_status=0
standalone_authorized=0
dot_client_read_lock "$cutover_lock" || standalone_status=$?
ready=''
checkout=''
dot_runtime=''
# The tracked phase is the fleet-visible deployment gate. `prepare` may stage a
# complete checkout, but cannot activate it. An embedded updater also re-execs
# this front door with its private --skip-pull flag; keep that exact invocation
# on the retained launcher because standalone Dot deliberately does not own the
# old engine's private flag.
if [[ $standalone_status -eq 0 && $DOT_CLIENT_PHASE == active &&
  ${DOT_REEXEC:-0} != 1 ]]; then
  if [[ -n $ready_override ]]; then
    if dot_client_normalized_absolute "$ready_override"; then
      ready=$ready_override
    else
      standalone_status=1
    fi
  elif dot_client_state_home; then
    # The symbolic generation lets a client invalidate obsolete convergence
    # proofs when its topology changes without inventing a second runtime pin.
    if [[ $DOT_CLIENT_STATE_HOME == / ]]; then
      ready=/dot/client-ready-v4/$DOT_CLIENT_READINESS_GENERATION/$DOT_CLIENT_MINIMUM_REVISION
    else
      ready=$DOT_CLIENT_STATE_HOME/dot/client-ready-v4/$DOT_CLIENT_READINESS_GENERATION/$DOT_CLIENT_MINIMUM_REVISION
    fi
  else
    standalone_status=1
  fi
  if [[ $standalone_status -eq 0 ]] &&
    dot_client_resolve_checkout &&
    dot_client_standalone_ready \
      "$DOT_CLIENT_CHECKOUT" "$DOT_CLIENT_RUNTIME" "$launcher" "$ready"; then
    checkout=$DOT_CLIENT_CHECKOUT
    dot_runtime=$DOT_CLIENT_RUNTIME
    standalone_authorized=1
  fi
fi
if [[ $standalone_authorized -eq 1 ]]; then
  exec "$dot_runtime" "$@"
fi

if [[ -f "$legacy_launcher" && ! -L "$legacy_launcher" &&
  -x "$legacy_launcher" ]]; then
  if [[ -f "$handoff_helper" && ! -L "$handoff_helper" &&
    -x "$handoff_helper" ]]; then
    "$handoff_helper" restore || {
      dot_client_error 'library handoff restoration failed'
      exit 1
    }
  fi
  DOT_CLIENT_FRONT_DOOR=$launcher
  export DOT_CLIENT_FRONT_DOOR
  exec "$legacy_launcher" "$@"
fi
if [[ -e "$legacy_launcher" || -L "$legacy_launcher" ]]; then
  dot_client_error "embedded launcher is unsafe: $legacy_launcher"
  exit 1
fi

if [[ $standalone_status -eq 2 ]]; then
  dot_client_error "malformed cutover lock: $cutover_lock"
  exit 2
fi
if [[ -z $dot_runtime ]]; then
  dot_client_resolve_checkout || {
    dot_client_error 'SHDEPS_INSTALL_DIR must be normalized and absolute'
    exit 1
  }
  dot_runtime=$DOT_CLIENT_RUNTIME
fi

cat >&2 <<EOF
dot: standalone runtime is unavailable: $dot_runtime
reinstall it with:
  curl -fsSL https://raw.githubusercontent.com/cgraf78/dot/main/install.sh | bash
EOF
exit 1
