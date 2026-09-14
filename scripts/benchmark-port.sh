#!/bin/bash

set -euo pipefail
CDPATH=
declare -a PERF_CARGO_TEST_ARGS=()

perf_usage() {
  printf 'usage: %s [--validate-publication PATH]\n' "${0##*/}" >&2
  exit 2
}

perf_release_cargo_test_contract() {
  local candidate_root=$1 argument normalized=cargo

  PERF_CARGO_TEST_ARGS=(test --release --locked --manifest-path \
    "$candidate_root/Cargo.toml" --test perf_update release_performance_gate -- \
    --exact --ignored --nocapture --test-threads=1)
  for argument in "${PERF_CARGO_TEST_ARGS[@]}"; do
    if [[ $argument == "$candidate_root/Cargo.toml" ]]; then
      argument=candidate/Cargo.toml
    fi
    [[ $argument =~ ^[A-Za-z0-9_./:=+-]+$ ]] || return 1
    normalized+=" $argument"
  done
  PERF_CARGO_TEST_INVOCATION=$normalized
}

# Initialize the public, path-normalized contract for validators and test
# helpers that do not run the full tool-resolution preflight. The real driver
# replaces only the manifest path in the same authoritative argument array.
perf_release_cargo_test_contract candidate

perf_canonical_executable() {
  local candidate=$1 directory

  [[ $candidate == /* && -f $candidate && -x $candidate ]] || return 1
  directory=$(cd -P -- "${candidate%/*}" 2>/dev/null && pwd -P) || return 1
  REPLY=$directory/${candidate##*/}
  [[ -f $REPLY && -x $REPLY ]] || return 1
}

perf_first_tool() {
  local candidate

  for candidate in "$@"; do
    if perf_canonical_executable "$candidate"; then
      return 0
    fi
  done
  return 1
}

perf_append_path() {
  local directory=$1

  [[ -d $directory ]] || return 0
  directory=$(cd -P -- "$directory" 2>/dev/null && pwd -P) || return 0
  case :${REPLY_PATH:-}: in
    *:"$directory":*) ;;
    *) REPLY_PATH=${REPLY_PATH:+$REPLY_PATH:}$directory ;;
  esac
}

perf_artifact_directory() {
  local root=$1 configured=$2

  case $configured in
    /*) REPLY=$configured ;;
    *) REPLY=$root/$configured ;;
  esac
}

perf_canonical_artifact_destination() {
  local root=$1 configured=$2 root_canonical artifact_dir

  [[ -n $configured && $configured != *$'\n'* && $configured != *$'\r'* ]] || return 1
  root_canonical=$("$PERF_REALPATH" -e -- "$root") || return 1
  perf_artifact_directory "$root" "$configured"
  artifact_dir=$("$PERF_REALPATH" -m -- "$REPLY") || return 1
  [[ $artifact_dir == /*/* && $artifact_dir != / ]] || return 1
  case "$root_canonical"/ in
    "$artifact_dir"/|"$artifact_dir"/*)
      printf 'error: performance artifacts cannot replace the source tree or its ancestors\n' >&2
      return 1
      ;;
  esac
  REPLY=$artifact_dir
}

perf_lifecycle_lock_root_path() {
  local lock_root=/tmp/.dot-performance-locks-$EUID

  REPLY=$("$PERF_REALPATH" -m -- "$lock_root") || return 1
  [[ $REPLY == /*/* && $REPLY != / ]]
}

perf_lifecycle_lock_root() {
  local lock_root owner mode

  perf_lifecycle_lock_root_path || return 1
  lock_root=$REPLY

  if [[ ! -e $lock_root && ! -L $lock_root ]]; then
    if ! (umask 077 && "$PERF_MKDIR" -- "$lock_root") 2>/dev/null; then
      [[ -e $lock_root || -L $lock_root ]] || return 1
    fi
  fi
  [[ -d $lock_root && ! -L $lock_root && -O $lock_root ]] || return 1
  IFS=: read -r owner mode < <("$PERF_STAT" -Lc '%u:%a' -- "$lock_root") || return 1
  [[ $owner == "$EUID" && $mode =~ ^[0-7]{3,4}$ ]] || return 1
  (( (8#$mode & 077) == 0 )) || return 1
  REPLY=$("$PERF_REALPATH" -e -- "$lock_root") || return 1
}

perf_artifact_lifecycle_lock_path() {
  local artifact_dir=$1 lock_root key

  perf_lifecycle_lock_root || return 1
  lock_root=$REPLY
  case $lock_root/ in
    "$artifact_dir"/*)
      printf 'error: performance artifact directory contains the lifecycle lock root\n' >&2
      return 1
      ;;
  esac
  key=$(printf '%s\0' "$artifact_dir" | "$PERF_SHA256SUM") || return 1
  key=${key%%[[:space:]]*}
  [[ $key =~ ^[0-9a-f]{64}$ ]] || return 1
  REPLY=$lock_root/$key.lock
}

perf_preflight_artifact_destination() {
  local root=$1 configured=$2 root_canonical artifact_dir lock_root relative parent name
  local directory file status run_id
  local -a directories files run_ids

  [[ -n $configured && $configured != *$'\n'* && $configured != *$'\r'* ]] || return 1
  root_canonical=$("$PERF_REALPATH" -e -- "$root") || return 1
  perf_canonical_artifact_destination "$root_canonical" "$configured" || return 1
  artifact_dir=$REPLY
  perf_lifecycle_lock_root_path || return 1
  lock_root=$REPLY
  if [[ $artifact_dir == "$lock_root" || $artifact_dir == "$lock_root"/* \
    || $lock_root == "$artifact_dir"/* ]]; then
    printf 'error: performance artifacts cannot overlap the lifecycle lock root\n' >&2
    return 1
  fi
  case $artifact_dir in
    "$root_canonical"/*) relative=${artifact_dir#"$root_canonical"/} ;;
    /*)
      [[ $configured == /* ]] && return 0
      printf 'error: relative performance artifact path escapes the source repository\n' >&2
      return 1
      ;;
    *)
      printf 'error: relative performance artifact path escapes the source repository\n' >&2
      return 1
      ;;
  esac
  case $relative in
    .git|.git/*)
      printf 'error: performance artifacts cannot be stored in Git metadata\n' >&2
      return 1
      ;;
  esac
  parent=${relative%/*}
  name=${relative##*/}
  [[ $parent != "$relative" ]] || parent=.
  if [[ -n ${3:-} ]]; then
    [[ $3 =~ ^[0-9a-f]{40}$ ]] || return 1
    run_ids=("$3")
  else
    run_ids=(1111111111111111111111111111111111111111 \
      2222222222222222222222222222222222222222)
  fi
  directories=("$relative")
  for run_id in "${run_ids[@]}"; do
    directories+=("$parent/.$name.publication.$run_id" "$relative.run-$run_id")
  done
  files=(driver.tsv metadata.tsv samples.tsv summary.tsv calibration.tsv completion.tsv)
  for directory in "${directories[@]}"; do
    for file in "${files[@]}"; do
      status=0
      "$PERF_ENV" -i HOME=/nonexistent LC_ALL=C PATH=/usr/bin:/bin \
        GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null \
        GIT_TERMINAL_PROMPT=0 GIT_OPTIONAL_LOCKS=0 \
        GIT_CONFIG_COUNT=2 GIT_CONFIG_KEY_0=core.excludesFile \
        GIT_CONFIG_VALUE_0=/dev/null GIT_CONFIG_KEY_1=core.fsmonitor \
        GIT_CONFIG_VALUE_1=false \
        "$PERF_ARTIFACT_GIT" -C "$root_canonical" check-ignore -q -- \
        "$directory/$file" || status=$?
      if [[ $status -eq 1 ]]; then
        printf 'error: in-repository performance artifact paths must be ignored\n' >&2
        return 1
      fi
      [[ $status -eq 0 ]] || {
        printf 'error: cannot verify the performance artifact ignore policy\n' >&2
        return 1
      }
    done
  done
}

perf_acquire_artifact_lifecycle() {
  local root=$1 configured=$2 artifact_dir lock lock_identity
  local lock_fd

  perf_canonical_artifact_destination "$root" "$configured" || {
    printf 'error: invalid performance artifact directory\n' >&2
    return 1
  }
  artifact_dir=$REPLY
  perf_artifact_lifecycle_lock_path "$artifact_dir" || return 1
  lock=$REPLY
  if [[ ! -e $lock && ! -L $lock ]]; then
    if ! (umask 077 && set -o noclobber && : >"$lock") 2>/dev/null; then
      [[ -e $lock || -L $lock ]] || return 1
    fi
  fi
  perf_lifecycle_lock_identity "$lock" || {
    printf 'error: performance artifact lifecycle lock is unsafe\n' >&2
    return 1
  }
  lock_identity=$REPLY
  exec {lock_fd}<>"$lock" || return 1
  if ! perf_lifecycle_lock_identity "/proc/$BASHPID/fd/$lock_fd" descriptor \
    || [[ $REPLY != "$lock_identity" ]] \
    || ! "$PERF_FLOCK" --exclusive --nonblock "$lock_fd"; then
    exec {lock_fd}>&-
    printf 'error: performance artifact lifecycle is already active or needs recovery\n' >&2
    return 1
  fi
  if ! perf_lifecycle_lock_identity "$lock" || [[ $REPLY != "$lock_identity" ]]; then
    "$PERF_FLOCK" --unlock "$lock_fd" || :
    exec {lock_fd}>&-
    printf 'error: performance artifact lifecycle lock changed during acquisition\n' >&2
    return 1
  fi
  PERF_LIFECYCLE_LOCK=$lock
  PERF_LIFECYCLE_LOCK_IDENTITY=$lock_identity
  PERF_LIFECYCLE_FD=$lock_fd
  PERF_LIFECYCLE_ARTIFACT=$artifact_dir
  REPLY=$artifact_dir
}

perf_lifecycle_lock_identity() {
  local lock=$1 kind=${2:-path} identity links owner mode

  [[ -f $lock && -O $lock ]] || return 1
  [[ $kind == descriptor || ! -L $lock ]] || return 1
  identity=$("$PERF_STAT" -Lc '%d:%i:%h:%u:%a' -- "$lock") || return 1
  IFS=: read -r _ _ links owner mode <<<"$identity"
  [[ $links == 1 && $owner == "$EUID" && $mode =~ ^[0-7]{3,4}$ ]] || return 1
  (( (8#$mode & 077) == 0 )) || return 1
  REPLY=$identity
}

perf_lifecycle_lock_is_held() {
  local fd=${PERF_LIFECYCLE_FD:-}

  [[ $fd =~ ^[0-9]+$ && -n ${PERF_LIFECYCLE_LOCK:-} \
    && -e /proc/$BASHPID/fd/$fd ]] || return 1
  perf_lifecycle_lock_identity "$PERF_LIFECYCLE_LOCK" || return 1
  [[ $REPLY == "${PERF_LIFECYCLE_LOCK_IDENTITY:-}" ]] || return 1
  perf_lifecycle_lock_identity "/proc/$BASHPID/fd/$fd" descriptor || return 1
  [[ $REPLY == "${PERF_LIFECYCLE_LOCK_IDENTITY:-}" ]]
}

perf_release_artifact_lifecycle() {
  [[ -n ${PERF_LIFECYCLE_FD:-} ]] || return 0
  "$PERF_FLOCK" --unlock "$PERF_LIFECYCLE_FD" || return 1
  exec {PERF_LIFECYCLE_FD}>&-
  unset PERF_LIFECYCLE_FD PERF_LIFECYCLE_LOCK PERF_LIFECYCLE_LOCK_IDENTITY
  unset PERF_LIFECYCLE_ARTIFACT
}

perf_close_artifact_lifecycle_owner() {
  [[ -n ${PERF_LIFECYCLE_FD:-} ]] || return 0
  # Closing only this process's descriptor preserves the flock on the shared
  # open-file description while an inherited supervisor or descendant still
  # owns cleanup authority.
  exec {PERF_LIFECYCLE_FD}>&-
  unset PERF_LIFECYCLE_FD PERF_LIFECYCLE_LOCK PERF_LIFECYCLE_LOCK_IDENTITY
  unset PERF_LIFECYCLE_ARTIFACT
}

perf_close_lifecycle_lock_for_child() {
  [[ ${PERF_LIFECYCLE_FD:-} =~ ^[0-9]+$ ]] || return 0
  exec {PERF_LIFECYCLE_FD}>&-
  unset PERF_LIFECYCLE_FD PERF_LIFECYCLE_LOCK PERF_LIFECYCLE_LOCK_IDENTITY
  unset PERF_LIFECYCLE_ARTIFACT
}

perf_resolve_artifact_tools() {
  perf_first_tool /usr/bin/env /bin/env || return 1
  PERF_ENV=$REPLY
  perf_first_tool /usr/bin/git /bin/git || return 1
  PERF_ARTIFACT_GIT=$REPLY
  perf_first_tool /usr/bin/mkdir /bin/mkdir || return 1
  PERF_MKDIR=$REPLY
  perf_first_tool /usr/bin/mv /bin/mv || return 1
  PERF_MV=$REPLY
  perf_first_tool /usr/bin/rm /bin/rm || return 1
  PERF_RM=$REPLY
  perf_first_tool /usr/bin/flock /bin/flock || return 1
  PERF_FLOCK=$REPLY
  perf_first_tool /usr/bin/stat /bin/stat || return 1
  PERF_STAT=$REPLY
  perf_first_tool /usr/bin/realpath /bin/realpath || return 1
  PERF_REALPATH=$REPLY
  perf_first_tool /usr/bin/sha256sum /bin/sha256sum || return 1
  PERF_SHA256SUM=$REPLY
}

perf_invalidate_prior_completion() {
  local artifact_dir=$1 completion=$1/completion.tsv

  if [[ ${PERF_LIFECYCLE_ARTIFACT:-} != "$artifact_dir" ]] \
    || ! perf_lifecycle_lock_is_held; then
    return 1
  fi
  if [[ -e $artifact_dir || -L $artifact_dir ]]; then
    [[ -d $artifact_dir && ! -L $artifact_dir ]] || {
      printf 'error: performance artifact path is not a directory\n' >&2
      return 1
    }
    "$PERF_RM" -f -- "$completion" || return 1
    [[ ! -e $completion && ! -L $completion ]] || return 1
  fi
}

perf_remove_stale_artifact_temps() {
  local artifact_dir=$1 path name prefix suffix

  for prefix in driver metadata samples summary calibration completion; do
    for path in "$artifact_dir/.$prefix.tsv."*; do
      [[ -e $path || -L $path ]] || continue
      name=${path##*/}
      suffix=${name##*.}
      [[ $suffix =~ ^[0-9]+$ || $suffix =~ ^[0-9a-f]{40}$ ]] || continue
      if [[ -f $path || -L $path ]]; then
        "$PERF_RM" -f -- "$path" || return 1
      fi
    done
  done
}

perf_remove_stale_publication_dirs() {
  local artifact_dir=$1 parent name path suffix

  parent=${artifact_dir%/*}
  name=${artifact_dir##*/}
  for path in "$parent/.$name.publication."*; do
    [[ -e $path || -L $path ]] || continue
    suffix=${path##*.}
    [[ $suffix =~ ^[0-9a-f]{40}$ ]] || continue
    if [[ -d $path && ! -L $path ]]; then
      "$PERF_RM" -rf -- "$path" || return 1
    elif [[ -f $path || -L $path ]]; then
      "$PERF_RM" -f -- "$path" || return 1
    fi
  done
}

perf_prepare_artifacts() {
  local root=$1 configured=$2 artifact_dir file

  perf_artifact_directory "$root" "$configured"
  artifact_dir=$REPLY
  if [[ -e $artifact_dir || -L $artifact_dir ]]; then
    [[ -d $artifact_dir && ! -L $artifact_dir ]] || {
      printf 'error: performance artifact path is not a directory\n' >&2
      return 1
    }
    # Invalidate a prior success before any other fallible artifact operation.
    "$PERF_RM" -f -- "$artifact_dir/completion.tsv" || return 1
  else
    "$PERF_MKDIR" -p -- "$artifact_dir" || return 1
  fi
  artifact_dir=$(cd -P -- "$artifact_dir" && pwd -P) || return 1
  perf_remove_stale_artifact_temps "$artifact_dir" || return 1
  for file in driver.tsv metadata.tsv samples.tsv summary.tsv calibration.tsv; do
    "$PERF_RM" -f -- "$artifact_dir/$file" || return 1
  done
  REPLY=$artifact_dir
}

perf_require_command_supervisor_platform() {
  local kernel architecture

  kernel=$("$PERF_UNAME" -s) || return 1
  architecture=$("$PERF_UNAME" -m) || return 1
  case $kernel:$architecture in
    Linux:x86_64|Linux:aarch64) ;;
    *)
      printf 'error: the release performance gate requires Linux process supervision on x86_64 or aarch64\n' >&2
      return 1
      ;;
  esac
}

perf_bash_compatible() {
  local executable=$1 major

  # shellcheck disable=SC2016 # The selected Bash expands its own version array.
  major=$("$executable" --noprofile --norc -c \
    'printf "%s\n" "${BASH_VERSINFO[0]:-0}"' 2>/dev/null) || return 1
  [[ $major =~ ^[0-9]+$ && $major -ge 4 ]]
}

perf_parse_public_tool_version() {
  local name=$1 output=$2 line prefix version

  line=${output%%$'\n'*}
  case $name in
    git) prefix='git version ' ;;
    bash) prefix='GNU bash, version ' ;;
    cargo) prefix='cargo ' ;;
    rustc) prefix='rustc ' ;;
    *) return 1 ;;
  esac
  [[ $line == "$prefix"* ]] || return 1
  version=${line#"$prefix"}
  version=${version%%[[:space:]]*}
  [[ -n $version && ${#version} -le 64 ]] || return 1
  perf_public_version_token "$name" "$version" || return 1
  REPLY=$name\ $version
}

perf_public_version_token() {
  local name=$1 version=$2

  [[ -n $version && ${#version} -le 64 ]] || return 1
  case $name in
    git)
      [[ $version =~ ^[0-9]{1,6}(\.[0-9]{1,6}){1,3}$ ]] || return 1
      ;;
    bash)
      [[ $version =~ ^[0-9]{1,6}\.[0-9]{1,6}\.[0-9]{1,6}\([0-9]{1,6}\)-release$ ]] || return 1
      ;;
    cargo|rustc)
      [[ $version =~ ^[0-9]{1,6}\.[0-9]{1,6}\.[0-9]{1,6}(-(alpha|beta|dev|nightly)(\.[0-9]{1,6})?)?$ ]] || return 1
      ;;
  esac
}

perf_validate_public_tool_identity() {
  local name=$1 identity=$2 version

  [[ $identity == "$name "* ]] || return 1
  version=${identity#"$name "}
  [[ $version != *[[:space:]]* ]] || return 1
  perf_public_version_token "$name" "$version"
}

perf_public_tool_version() {
  local name=$1 program=$2 output
  shift 2

  output=$("$PERF_ENV" -i LC_ALL=C PATH="$PERF_BUILD_PATH" \
    "$program" "$@") || return 1
  perf_parse_public_tool_version "$name" "$output"
}

perf_public_runner_image() {
  case ${ImageOS-} in
    ubuntu20|ubuntu22|ubuntu24) REPLY=$ImageOS ;;
    '') REPLY=local ;;
    *) REPLY=other ;;
  esac
}

perf_validate_public_runner_image() {
  case $1 in
    local|other|ubuntu20|ubuntu22|ubuntu24) ;;
    *) return 1 ;;
  esac
}

perf_require_executable_scratch() {
  local scratch=$1 probe

  probe=$scratch/native-execution-probe

  "$PERF_CP" -- "$PERF_UNAME" "$probe" || {
    printf 'error: cannot create native executable scratch probe\n' >&2
    return 1
  }
  if ! "$probe" -s >/dev/null 2>&1; then
    "$PERF_RM" -f -- "$probe"
    printf 'error: performance scratch must permit native executable files\n' >&2
    return 1
  fi
  "$PERF_RM" -f -- "$probe"
}

perf_cleanup() {
  [[ -n ${PERF_RUN_SCRATCH:-} ]] || return 0
  case ${PERF_RUN_SCRATCH:-} in
    "${PERF_SCRATCH_PARENT:-}"/dot-performance.*)
      "$PERF_CHMOD" -R u+w "$PERF_RUN_SCRATCH" 2>/dev/null || :
      "$PERF_RM" -rf -- "$PERF_RUN_SCRATCH"
      ;;
    *)
      printf 'warning: refusing to clean unexpected benchmark path: %s\n' \
        "${PERF_RUN_SCRATCH:-<unset>}" >&2
      ;;
  esac
}

perf_finish() {
  local status=$?

  set +e
  trap - EXIT
  perf_cleanup
  perf_close_artifact_lifecycle_owner
  exit "$status"
}

perf_resolve_tools() {
  local cargo_home=${CARGO_HOME:-${HOME:?HOME is required}/.cargo}
  local candidate directory path_more path_remaining rustup=

  REPLY_PATH=
  perf_append_path /usr/bin
  perf_append_path /bin
  perf_append_path /usr/sbin
  perf_append_path /sbin
  perf_append_path /usr/local/bin
  perf_append_path /opt/homebrew/bin
  perf_append_path /opt/local/bin
  PERF_CLIENT_PATH=$REPLY_PATH

  if [[ -n ${DOT_PERF_GIT:-} ]]; then
    perf_canonical_executable "$DOT_PERF_GIT" || return 1
  else
    perf_first_tool /usr/bin/git /bin/git /usr/local/bin/git \
      /opt/homebrew/bin/git /opt/local/bin/git || return 1
  fi
  PERF_GIT=$REPLY

  if [[ -n ${DOT_PERF_BASH:-} ]]; then
    perf_canonical_executable "$DOT_PERF_BASH" || return 1
    perf_bash_compatible "$REPLY" || return 1
  else
    REPLY=
    for candidate in /usr/bin/bash /bin/bash /usr/local/bin/bash \
      /opt/homebrew/bin/bash /opt/local/bin/bash; do
      perf_canonical_executable "$candidate" || continue
      if perf_bash_compatible "$REPLY"; then
        break
      fi
      REPLY=
    done
    [[ -n ${REPLY:-} ]] || return 1
  fi
  PERF_BASH=$REPLY

  if perf_first_tool "$cargo_home/bin/rustup" /usr/bin/rustup \
    /usr/local/bin/rustup /opt/homebrew/bin/rustup /opt/local/bin/rustup; then
    rustup=$REPLY
  fi
  if [[ -n ${DOT_PERF_CARGO:-} ]]; then
    perf_canonical_executable "$DOT_PERF_CARGO" || return 1
    PERF_CARGO=$REPLY
  elif [[ -n $rustup ]]; then
    PERF_CARGO=$("$rustup" which cargo) || return 1
    perf_canonical_executable "$PERF_CARGO" || return 1
    PERF_CARGO=$REPLY
  else
    perf_first_tool /usr/bin/cargo /usr/local/bin/cargo \
      /opt/homebrew/bin/cargo /opt/local/bin/cargo || return 1
    PERF_CARGO=$REPLY
  fi
  if [[ -n ${DOT_PERF_RUSTC:-} ]]; then
    perf_canonical_executable "$DOT_PERF_RUSTC" || return 1
    PERF_RUSTC=$REPLY
  elif [[ -n $rustup ]]; then
    PERF_RUSTC=$("$rustup" which rustc) || return 1
    perf_canonical_executable "$PERF_RUSTC" || return 1
    PERF_RUSTC=$REPLY
  else
    perf_first_tool /usr/bin/rustc /usr/local/bin/rustc \
      /opt/homebrew/bin/rustc /opt/local/bin/rustc || return 1
    PERF_RUSTC=$REPLY
  fi

  perf_first_tool /usr/bin/env /bin/env || return 1
  PERF_ENV=$REPLY
  perf_first_tool /usr/bin/cp /bin/cp || return 1
  PERF_CP=$REPLY
  perf_first_tool /usr/bin/ln /bin/ln || return 1
  PERF_LN=$REPLY
  perf_first_tool /usr/bin/chmod /bin/chmod || return 1
  PERF_CHMOD=$REPLY
  perf_first_tool /usr/bin/mkdir /bin/mkdir || return 1
  PERF_MKDIR=$REPLY
  if [[ -n ${DOT_PERF_MKTEMP:-} ]]; then
    perf_canonical_executable "$DOT_PERF_MKTEMP" || return 1
  else
    perf_first_tool /usr/bin/mktemp /bin/mktemp || return 1
  fi
  PERF_MKTEMP=$REPLY
  perf_first_tool /usr/bin/rm /bin/rm || return 1
  PERF_RM=$REPLY
  perf_first_tool /usr/bin/od /bin/od || return 1
  PERF_OD=$REPLY
  if [[ -n ${DOT_PERF_UNAME:-} ]]; then
    perf_canonical_executable "$DOT_PERF_UNAME" || return 1
  else
    perf_first_tool /usr/bin/uname /bin/uname || return 1
  fi
  PERF_UNAME=$REPLY

  REPLY_PATH=
  perf_append_path "${PERF_CARGO%/*}"
  perf_append_path "${PERF_RUSTC%/*}"
  path_remaining=$PERF_CLIENT_PATH
  while :; do
    case $path_remaining in
      *:*)
        directory=${path_remaining%%:*}
        path_remaining=${path_remaining#*:}
        path_more=true
        ;;
      *)
        directory=$path_remaining
        path_more=false
        ;;
    esac
    perf_append_path "$directory"
    [[ $path_more == true ]] || break
  done
  PERF_BUILD_PATH=$REPLY_PATH
  perf_release_cargo_test_contract candidate
}

perf_git() (
  perf_close_lifecycle_lock_for_child
  local -a environment=(
    HOME="$PERF_DRIVER_HOME"
    LC_ALL=C
    PATH="$PERF_CLIENT_PATH"
    TMPDIR="$PERF_SCRATCH_PARENT"
    GIT_CONFIG_NOSYSTEM=1
    GIT_CONFIG_GLOBAL=/dev/null
    GIT_TERMINAL_PROMPT=0
    GIT_OPTIONAL_LOCKS=0
    GIT_CONFIG_COUNT=3
    GIT_CONFIG_KEY_0=core.hooksPath
    GIT_CONFIG_VALUE_0=/dev/null
    GIT_CONFIG_KEY_1=commit.gpgSign
    GIT_CONFIG_VALUE_1=false
    GIT_CONFIG_KEY_2=tag.gpgSign
    GIT_CONFIG_VALUE_2=false
  )
  local name
  for name in HTTPS_PROXY HTTP_PROXY ALL_PROXY NO_PROXY SSL_CERT_FILE SSL_CERT_DIR; do
    if [[ -n ${!name+x} ]]; then
      environment+=("$name=${!name}")
    fi
  done
  "$PERF_ENV" -i "${environment[@]}" "$PERF_GIT" "$@"
)

perf_require_config_free_cwd() {
  local directory=$1 candidate

  [[ $directory == /* ]] || return 1
  while :; do
    for candidate in "$directory/.cargo/config" "$directory/.cargo/config.toml"; do
      if [[ -e $candidate || -L $candidate ]]; then
        printf 'error: Cargo configuration in benchmark cwd ancestry is not allowed\n' >&2
        return 1
      fi
    done
    [[ $directory == / ]] && break
    directory=${directory%/*}
    [[ -n $directory ]] || directory=/
  done
}

perf_process_start_tick() {
  local pid=$1 line rest

  [[ $pid =~ ^[0-9]+$ ]] || return 1
  if [[ -r /proc/$pid/stat ]]; then
    IFS= read -r line <"/proc/$pid/stat" || return 1
    rest=${line##*) }
    [[ $rest != "$line" ]] || return 1
    # shellcheck disable=SC2086  # Kernel stat fields are intentionally tokenized.
    set -- $rest
    [[ $# -ge 20 && ${20} =~ ^[0-9]+$ ]] || return 1
    printf '%s\n' "${20}"
    return 0
  fi
  # No procfs (macOS): fall back to the `ps` start timestamp. `ps`
  # prints one `lstart` line per live PID; an unknown or reaped PID
  # prints nothing, which fails closed like the procfs path.
  command -v ps >/dev/null 2>&1 || return 1
  line=$(ps -o lstart= -p "$pid" 2>/dev/null) || return 1
  line=${line#"${line%%[![:space:]]*}"}
  [[ -n $line ]] || return 1
  printf '%s\n' "$line"
}

perf_process_identity_matches() {
  local pid=$1 expected=$2 actual

  [[ -n $expected ]] || return 1
  actual=$(perf_process_start_tick "$pid") || return 1
  [[ $actual == "$expected" ]]
}

perf_cargo() {
  local -a environment=(
    HOME="$PERF_BUILD_HOME"
    CARGO_HOME="$PERF_CARGO_HOME"
    CARGO_TARGET_DIR="$PERF_CARGO_TARGET_DIR"
    LC_ALL=C
    PATH="$PERF_BUILD_PATH"
    TMPDIR="$PERF_SCRATCH_PARENT"
    RUSTC="$PERF_RUSTC"
    GIT_CONFIG_NOSYSTEM=1
    GIT_CONFIG_GLOBAL=/dev/null
    GIT_TERMINAL_PROMPT=0
    GIT_OPTIONAL_LOCKS=0
    GIT_CONFIG_COUNT=3
    GIT_CONFIG_KEY_0=core.hooksPath
    GIT_CONFIG_VALUE_0=/dev/null
    GIT_CONFIG_KEY_1=commit.gpgSign
    GIT_CONFIG_VALUE_1=false
    GIT_CONFIG_KEY_2=tag.gpgSign
    GIT_CONFIG_VALUE_2=false
    DOT_BUILD_COMMIT="$DOT_BUILD_COMMIT"
    DOT_PERF_ARTIFACT_DIR="$DOT_PERF_ARTIFACT_DIR"
    DOT_PERF_BASH="$PERF_BASH"
    DOT_PERF_CARGO="$PERF_CARGO"
    DOT_PERF_CLIENT_PATH="$PERF_CLIENT_PATH"
    DOT_PERF_CURRENT_DIRTY="$DOT_PERF_CURRENT_DIRTY"
    DOT_PERF_CURRENT_SHA="$DOT_PERF_CURRENT_SHA"
    DOT_PERF_GIT="$PERF_GIT"
    DOT_PERF_RUSTC="$PERF_RUSTC"
    DOT_PERF_RUNNER_IMAGE="$DOT_PERF_RUNNER_IMAGE"
    DOT_PERF_RUN_ID="$DOT_PERF_RUN_ID"
    DOT_PERF_SHDEPS_BINARY="$DOT_PERF_SHDEPS_BINARY"
    DOT_PERF_SHDEPS_ROOT="$DOT_PERF_SHDEPS_ROOT"
    DOT_PERF_SHELL_ROOT="$DOT_PERF_SHELL_ROOT"
    DOT_PERF_UNAME="$PERF_UNAME"
    SHDEPS_BUILD_COMMIT="$SHDEPS_BUILD_COMMIT"
  )
  local name supervisor_pid='' supervisor_start='' requested_signal=0 status=0 driver_pid=$BASHPID
  local old_hup old_int old_quit old_term
  for name in HTTPS_PROXY HTTP_PROXY ALL_PROXY NO_PROXY SSL_CERT_FILE SSL_CERT_DIR RUST_BACKTRACE; do
    if [[ -n ${!name+x} ]]; then
      environment+=("$name=${!name}")
    fi
  done
  local cargo_cwd=$PERF_BUILD_HOME/cargo-cwd
  "$PERF_MKDIR" -p -- "$cargo_cwd" || return 1
  cargo_cwd=$(cd -P -- "$cargo_cwd" && pwd -P) || return 1
  [[ $PERF_CARGO_TARGET_DIR == /* ]] || {
    printf 'error: Cargo target directory must be absolute and run-private\n' >&2
    return 1
  }
  "$PERF_MKDIR" -p -- "$PERF_CARGO_TARGET_DIR" || return 1
  perf_require_config_free_cwd "$cargo_cwd" || return 1
  [[ -x ${PERF_COMMAND_SUPERVISOR:-} && -f ${PERF_COMMAND_SUPERVISOR:-} \
    && ! -L ${PERF_COMMAND_SUPERVISOR:-} ]] || {
    printf 'error: Cargo process supervisor is unavailable\n' >&2
    return 1
  }
  old_hup=$(trap -p HUP) || :
  old_int=$(trap -p INT) || :
  old_quit=$(trap -p QUIT) || :
  old_term=$(trap -p TERM) || :
  trap '[[ $requested_signal -ne 0 ]] || requested_signal=1; \
    [[ -z $supervisor_pid ]] || kill -HUP "$supervisor_pid" 2>/dev/null || :' HUP
  trap '[[ $requested_signal -ne 0 ]] || requested_signal=2; \
    [[ -z $supervisor_pid ]] || kill -INT "$supervisor_pid" 2>/dev/null || :' INT
  trap '[[ $requested_signal -ne 0 ]] || requested_signal=3; \
    [[ -z $supervisor_pid ]] || kill -QUIT "$supervisor_pid" 2>/dev/null || :' QUIT
  trap '[[ $requested_signal -ne 0 ]] || requested_signal=15; \
    [[ -z $supervisor_pid ]] || kill -TERM "$supervisor_pid" 2>/dev/null || :' TERM
  "$PERF_ENV" -i "${environment[@]}" "$PERF_COMMAND_SUPERVISOR" \
    --parent-pid "$driver_pid" --cwd "$cargo_cwd" -- "$PERF_CARGO" "$@" &
  supervisor_pid=$!
  supervisor_start=$(perf_process_start_tick "$supervisor_pid" 2>/dev/null) ||
    supervisor_start=''
  if ((requested_signal != 0)); then
    kill -"$requested_signal" "$supervisor_pid" 2>/dev/null || :
  fi
  while :; do
    if wait "$supervisor_pid"; then
      status=0
      supervisor_pid=''
      break
    else
      status=$?
    fi
    # A reaped supervisor frees its numeric PID for immediate reuse, so a bare
    # `kill -0` after wait can observe an unrelated process and either spin on
    # a stale identity or, through the still-installed exit-path signal traps,
    # signal one. The exact /proc start tick pins the original supervisor: a
    # missing record or a changed tick proves the original was reaped even when
    # its PID was recycled. Clearing supervisor_pid on that definitive reap
    # also withdraws the traps' signal authority before they are restored. An
    # unreadable start tick fails toward the reaped state, which can only
    # return the already-observed wait status instead of misdirecting a signal.
    if ! perf_process_identity_matches "$supervisor_pid" "$supervisor_start"; then
      supervisor_pid=''
      break
    fi
  done
  # shellcheck disable=SC2294 # Restoring exact caller-owned trap commands.
  [[ -z $old_hup ]] || eval "$old_hup"
  # shellcheck disable=SC2294 # Restoring exact caller-owned trap commands.
  [[ -z $old_int ]] || eval "$old_int"
  # shellcheck disable=SC2294 # Restoring exact caller-owned trap commands.
  [[ -z $old_quit ]] || eval "$old_quit"
  # shellcheck disable=SC2294 # Restoring exact caller-owned trap commands.
  [[ -z $old_term ]] || eval "$old_term"
  [[ -n $old_hup ]] || trap - HUP
  [[ -n $old_int ]] || trap - INT
  [[ -n $old_quit ]] || trap - QUIT
  [[ -n $old_term ]] || trap - TERM
  if ((requested_signal != 0 && status != 70)); then
    return $((128 + requested_signal))
  fi
  return "$status"
}

perf_provider_build_release() {
  local manifest=$1 status=0

  if perf_cargo build --release --locked --manifest-path "$manifest"; then
    return 0
  else
    status=$?
  fi
  return "$status"
}

perf_build_command_supervisor() {
  local source=$1 output=$2

  perf_require_command_supervisor_platform || return 1
  [[ -f $source && ! -L $source && $output == /* \
    && ! -e $output && ! -L $output ]] || return 1
  (
    perf_close_lifecycle_lock_for_child
    "$PERF_ENV" -i HOME="$PERF_BUILD_HOME" LC_ALL=C PATH="$PERF_BUILD_PATH" \
      TMPDIR="$PERF_SCRATCH_PARENT" "$PERF_RUSTC" --edition=2021 \
      -C opt-level=2 -C debuginfo=0 -o "$output" "$source"
  ) || return 1
  [[ -f $output && -x $output && ! -L $output ]]
}

perf_source_state() {
  local root=$1 commit tree status_blob

  commit=$(perf_git -C "$root" rev-parse HEAD) || {
    printf 'error: cannot resolve current benchmark commit\n' >&2
    return 1
  }
  [[ $commit =~ ^[0-9a-f]{40}$ ]] || {
    printf 'error: cannot resolve current benchmark commit\n' >&2
    return 1
  }
  tree=$(perf_git -C "$root" rev-parse 'HEAD^{tree}') || {
    printf 'error: cannot resolve benchmark source tree\n' >&2
    return 1
  }
  [[ $tree =~ ^[0-9a-f]{40}$ ]] || {
    printf 'error: cannot resolve benchmark source tree\n' >&2
    return 1
  }
  status_blob=$(perf_git -C "$root" status --porcelain=v1 -z --untracked-files=all |
    perf_git hash-object --stdin) || {
    printf 'error: cannot inspect current benchmark worktree\n' >&2
    return 1
  }
  [[ $status_blob =~ ^[0-9a-f]{40}$ ]] || {
    printf 'error: cannot fingerprint current benchmark worktree\n' >&2
    return 1
  }
  REPLY_SHA=$commit
  REPLY_TREE=$tree
  REPLY_STATUS_BLOB=$status_blob
  if [[ $status_blob != e69de29bb2d1d6434b8b29ae775ad8c2e48c5391 ]]; then
    REPLY_DIRTY=true
  else
    REPLY_DIRTY=false
  fi
}

perf_seal_source_snapshot() {
  local root=$1 expected_commit=$2 label=$3 tree status_blob

  perf_require_dissociated_repository "$root" || {
    printf 'error: %s source snapshot shares or delegates Git object storage\n' \
      "$label" >&2
    return 1
  }
  perf_source_state "$root" || return 1
  [[ $REPLY_SHA == "$expected_commit" && $REPLY_DIRTY == false ]] || {
    printf 'error: %s source snapshot is not the requested clean commit\n' "$label" >&2
    return 1
  }
  tree=$REPLY_TREE
  status_blob=$REPLY_STATUS_BLOB
  # No `--`: BSD chmod parses the mode first, so a `--` after it is treated
  # as a filename and fails the seal on macOS. Sealed roots are absolute
  # scratch paths, never dash-leading, so the guard is unnecessary.
  "$PERF_CHMOD" -R a-w "$root" || return 1
  perf_source_state "$root" || return 1
  [[ $REPLY_SHA == "$expected_commit" && $REPLY_TREE == "$tree" \
    && $REPLY_STATUS_BLOB == "$status_blob" && $REPLY_DIRTY == false ]] || {
    printf 'error: %s source snapshot changed while being sealed\n' "$label" >&2
    return 1
  }
  REPLY_TREE=$tree
  REPLY_STATUS_BLOB=$status_blob
}

perf_clone_source_snapshot() {
  local source=$1 commit=$2 destination=$3 label=$4

  perf_git clone --no-local --quiet "$source" "$destination" || return 1
  perf_git -C "$destination" checkout --quiet --detach "$commit" || return 1
  perf_seal_source_snapshot "$destination" "$commit" "$label"
}

perf_prepare_provider_run_root() {
  # Shell samples bootstrap install.sh against SHDEPS_LIB's checkout, which
  # rebuilds the Rust CLI from source when the checkout has no matching
  # binary. The sealed provider root deliberately has none (the release
  # binary is built into a separate target dir), so samples would trigger a
  # from-source rebuild with network fetches and exceed the sample timeout.
  # Give samples a verified copy of the sealed root with the release binary
  # linked where bootstrap expects it. The staged entries must be symlinks
  # to the harness-built binary, not copies: feature-payload validation
  # requires the installed provider to canonicalize to the pinned
  # executable path. Identity evidence still comes from the sealed root
  # plus the hashed provider binary blob.
  local sealed=$1 binary=$2 run_root=$3 commit=$4 head short version
  perf_canonical_executable "$binary" || {
    printf 'error: Shdeps provider binary is not usable: %s\n' "$binary" >&2
    return 1
  }
  binary=$REPLY
  "$PERF_MKDIR" -p -- "$run_root" || return 1
  "$PERF_CP" -r -- "$sealed/." "$run_root/" || return 1
  "$PERF_CHMOD" -R u+w "$run_root" || return 1
  "$PERF_MKDIR" -p -- "$run_root/target/release" || return 1
  "$PERF_LN" -s -- "$binary" "$run_root/target/release/shdeps" || return 1
  "$PERF_LN" -s -- "$binary" "$run_root/shdeps" || return 1
  [[ -x $run_root/target/release/shdeps && -x $run_root/shdeps ]] || {
    printf 'error: staged Shdeps binaries lost the executable bit\n' >&2
    return 1
  }
  head=$("$PERF_GIT" -C "$run_root" rev-parse HEAD) || return 1
  [[ $head == "$commit" ]] || {
    printf 'error: Shdeps run-root identity mismatch\n' >&2
    return 1
  }
  short=$("$PERF_GIT" -C "$run_root" rev-parse --short=8 HEAD) || return 1
  version=$("$run_root/target/release/shdeps" version 2>/dev/null) || {
    printf 'error: staged Shdeps binary does not report a version\n' >&2
    return 1
  }
  [[ $version == *"$short"* ]] || {
    printf 'error: staged Shdeps binary does not match run-root HEAD\n' >&2
    return 1
  }
}

perf_require_dissociated_repository() {
  local root=$1 git_dir objects alternates

  [[ -d $root && ! -L $root && -O $root ]] || return 1
  git_dir=$(perf_git -C "$root" rev-parse --absolute-git-dir) || return 1
  [[ $git_dir == /* && -d $git_dir && ! -L $git_dir && -O $git_dir ]] || return 1
  objects=$git_dir/objects
  alternates=$objects/info/alternates
  [[ -d $objects && ! -L $objects && -O $objects \
    && ! -e $alternates && ! -L $alternates ]]
}

perf_require_source_identity() {
  local root=$1 commit=$2 tree=$3 status_blob=$4 phase=$5

  perf_source_state "$root" || return 1
  [[ $REPLY_SHA == "$commit" && $REPLY_TREE == "$tree" \
    && $REPLY_STATUS_BLOB == "$status_blob" ]] || {
    printf 'error: benchmark source identity changed during %s\n' "$phase" >&2
    return 1
  }
}

perf_require_all_source_identities() {
  local phase=$1

  perf_require_source_identity "$PERF_CURRENT_ROOT" "$PERF_CURRENT_COMMIT" \
    "$PERF_CURRENT_TREE" "$PERF_CURRENT_STATUS_BLOB" "$phase (candidate)" || return 1
  perf_require_source_identity "$PERF_SHELL_SOURCE_ROOT" "$PERF_SHELL_COMMIT" \
    "$PERF_SHELL_TREE" "$PERF_CLEAN_STATUS_BLOB" "$phase (shell baseline)" || return 1
  perf_require_source_identity "$PERF_SHDEPS_SOURCE_ROOT" "$PERF_SHDEPS_COMMIT" \
    "$PERF_SHDEPS_TREE" "$PERF_CLEAN_STATUS_BLOB" "$phase (provider)"
}

perf_lock_identity() {
  local root=$1 key value extra revision='' abi=''

  while IFS='=' read -r key value extra; do
    case $key in
      revision)
        [[ -z $revision && -z ${extra:-} ]] || return 1
        revision=$value
        ;;
      abi)
        [[ -z $abi && -z ${extra:-} ]] || return 1
        abi=$value
        ;;
    esac
  done <"$root/support/shdeps.lock" || return 1
  [[ $revision =~ ^[0-9a-f]{40}$ && $abi =~ ^[0-9]+$ ]] || return 1
  REPLY_REVISION=$revision
  REPLY_ABI=$abi
}

perf_write_final_driver() {
  local artifact_dir=$1 run_id=$2 shell_commit=$3 shell_tree=$4
  local current_commit=$5 current_tree=$6 current_dirty=$7 shdeps_commit=$8 shdeps_tree=$9

  {
    printf 'key\tvalue\n'
    printf 'format\tperformance-driver-v2\n'
    printf 'status\tpassed\n'
    printf 'artifact_location\tconfigured-output\n'
    printf 'git_location\tsystem-tool\n'
    printf 'bash_location\tsystem-tool\n'
    printf 'cargo_location\tselected-build-tool\n'
    printf 'rustc_location\tselected-build-tool\n'
    printf 'client_path_policy\tcontrolled\n'
    printf 'run_id\t%s\n' "$run_id"
    printf 'shell_commit\t%s\n' "$shell_commit"
    printf 'shell_tree\t%s\n' "$shell_tree"
    printf 'rust_commit\t%s\n' "$current_commit"
    printf 'rust_tree\t%s\n' "$current_tree"
    printf 'rust_worktree_dirty\t%s\n' "$current_dirty"
    printf 'shdeps_commit\t%s\n' "$shdeps_commit"
    printf 'shdeps_tree\t%s\n' "$shdeps_tree"
    printf 'cargo_invocation\t%s\n' "$PERF_CARGO_TEST_INVOCATION"
    printf 'cargo_test_name\trelease_performance_gate\n'
    printf 'cargo_exit_code\t0\n'
  } >"$artifact_dir/driver.tsv"
}

perf_new_run_id() {
  local root=$1 artifact_config=$2 digest

  # The run ID is an opaque uniqueness nonce, not a source identity. Generate
  # it with shell-owned process entropy so the exact publication paths can be
  # checked before creating a lifecycle lock, scratch, or artifact entry.
  digest=$(printf '%s\0%s\0%s\0%s\0%s\0%s\0%s\0' \
    "$root" "$artifact_config" "$$" "$BASHPID" "$SECONDS" \
    "$RANDOM" "$RANDOM$RANDOM" | "$PERF_SHA256SUM") || return 1
  digest=${digest%%[[:space:]]*}
  [[ $digest =~ ^[0-9a-f]{64}$ ]] || return 1
  REPLY=${digest:0:40}
}

declare -A PERF_TSV_VALUES=()
declare -A PERF_SAMPLE_VALUES=()
declare -A PERF_SAMPLE_MEDIANS=()
declare -A PERF_SAMPLE_P95S=()
declare -A PERF_SUMMARY_VALUES=()

perf_evidence_error() {
  printf 'error: invalid performance evidence: %s\n' "$1" >&2
  return 1
}

perf_split_tsv_line() {
  local remaining=$1 expected=$2

  REPLY_FIELDS=()
  while [[ $remaining == *$'\t'* ]]; do
    REPLY_FIELDS+=("${remaining%%$'\t'*}")
    remaining=${remaining#*$'\t'}
  done
  REPLY_FIELDS+=("$remaining")
  [[ ${#REPLY_FIELDS[@]} -eq $expected ]]
}

perf_load_key_value_tsv() {
  local file=$1 line='' rows=0 key value

  PERF_TSV_VALUES=()
  while :; do
    line=
    if IFS= read -r line; then
      :
    else
      [[ -z $line ]] || {
        perf_evidence_error 'unterminated key/value record'
        return 1
      }
      break
    fi
    rows=$((rows + 1))
    if ((rows == 1)); then
      [[ $line == $'key\tvalue' ]] || {
        perf_evidence_error 'invalid key/value header'
        return 1
      }
      continue
    fi
    perf_split_tsv_line "$line" 2 || {
      perf_evidence_error 'invalid key/value record'
      return 1
    }
    key=${REPLY_FIELDS[0]}
    value=${REPLY_FIELDS[1]}
    [[ $key =~ ^[a-z][a-z0-9_]*$ \
      && -z ${PERF_TSV_VALUES[$key]+present} ]] || {
      perf_evidence_error 'duplicate or empty key/value identity'
      return 1
    }
    PERF_TSV_VALUES[$key]=$value
  done <"$file"
  [[ $rows -ge 1 ]] || {
    perf_evidence_error 'empty key/value evidence'
    return 1
  }
  PERF_TSV_ROW_COUNT=$((rows - 1))
}

perf_require_loaded_value() {
  local key=$1 expected=$2

  [[ -n ${PERF_TSV_VALUES[$key]+present} && ${PERF_TSV_VALUES[$key]} == "$expected" ]] || {
    perf_evidence_error "mismatched $key"
    return 1
  }
}

perf_file_blob() {
  local file=$1

  [[ -f $file && ! -L $file ]] || {
    perf_evidence_error 'identity target is not a regular file'
    return 1
  }
  REPLY=$(perf_git hash-object --no-filters -- "$file") || return 1
  [[ $REPLY =~ ^[0-9a-f]{40}$ ]] || {
    perf_evidence_error 'invalid file identity'
    return 1
  }
}

perf_validate_artifact_set() {
  local artifact_dir=$1 mode=$2 path name count=0 expected

  [[ $mode == provisional || $mode == complete ]] || {
    perf_evidence_error 'invalid artifact-set mode'
    return 1
  }

  [[ -d $artifact_dir && ! -L $artifact_dir ]] || {
    perf_evidence_error 'artifact directory is not a regular directory'
    return 1
  }
  for path in "$artifact_dir"/* "$artifact_dir"/.[!.]* "$artifact_dir"/..?*; do
    [[ -e $path || -L $path ]] || continue
    name=${path##*/}
    case $mode:$name in
      provisional:driver.tsv|provisional:metadata.tsv|provisional:samples.tsv|provisional:summary.tsv|provisional:calibration.tsv) ;;
      complete:driver.tsv|complete:metadata.tsv|complete:samples.tsv|complete:summary.tsv|complete:calibration.tsv|complete:completion.tsv) ;;
      *)
        perf_evidence_error 'unexpected artifact entry'
        return 1
        ;;
    esac
    [[ -f $path && ! -L $path ]] || {
      perf_evidence_error 'artifact entry is not a regular file'
      return 1
    }
    count=$((count + 1))
  done
  if [[ $mode == provisional ]]; then expected=5; else expected=6; fi
  [[ $count -eq $expected ]] || {
    perf_evidence_error 'incomplete artifact set'
    return 1
  }
}

perf_require_public_text() {
  local file=$1 dump byte

  dump=$("$PERF_OD" -An -v -tu1 -- "$file") || return 1
  for byte in $dump; do
    if ! ((byte == 9 || byte == 10 || (byte >= 32 && byte <= 126))); then
      perf_evidence_error 'artifact contains a non-public control byte'
      return 1
    fi
  done
}

perf_validate_positive_ns() {
  local value=$1

  [[ $value =~ ^[1-9][0-9]*$ ]] || return 1
  ((${#value} < 11 || (${#value} == 11 && 10#$value <= 30000000000)))
}

perf_compute_sample_stats() {
  local key=$1 expected=$2 value index middle rank
  local -a sorted=()

  # The values have already been constrained to positive decimal integers.
  for value in ${PERF_SAMPLE_VALUES[$key]-}; do
    index=${#sorted[@]}
    while ((index > 0 && sorted[index - 1] > value)); do
      sorted[index]=${sorted[index - 1]}
      index=$((index - 1))
    done
    sorted[index]=$value
  done
  [[ ${#sorted[@]} -eq $expected ]] || {
    perf_evidence_error 'sample count does not match policy'
    return 1
  }
  middle=$((expected / 2))
  if ((expected % 2 == 0)); then
    PERF_SAMPLE_MEDIANS[$key]=$((sorted[middle - 1] + \
      (sorted[middle] - sorted[middle - 1]) / 2))
  else
    PERF_SAMPLE_MEDIANS[$key]=${sorted[middle]}
  fi
  rank=$(((expected * 95 + 99) / 100))
  PERF_SAMPLE_P95S[$key]=${sorted[rank - 1]}
}

perf_validate_samples() {
  local file=$1 expected_run_id=$2 line='' rows=0
  local run_id engine workload iteration order position elapsed exit_code validated
  local expected_order expected_position expected_exit key expected_engine
  local data_index workload_index expected_workload expected_iteration
  local -a workloads=(help version base-clean disjoint-clean disjoint-dirty \
    profile-provider-hooks-collision pre-sync-failure)
  local -A seen=()

  PERF_SAMPLE_VALUES=()
  PERF_SAMPLE_MEDIANS=()
  PERF_SAMPLE_P95S=()
  while :; do
    line=
    if IFS= read -r line; then
      :
    else
      [[ -z $line ]] || {
        perf_evidence_error 'unterminated sample record'
        return 1
      }
      break
    fi
    rows=$((rows + 1))
    if ((rows == 1)); then
      [[ $line == $'run_id\tengine\tworkload\titeration\torder\tposition\telapsed_ns\texit_code\tvalidated' ]] || {
        perf_evidence_error 'invalid sample header'
        return 1
      }
      continue
    fi
    perf_split_tsv_line "$line" 9 || {
      perf_evidence_error 'invalid sample record'
      return 1
    }
    run_id=${REPLY_FIELDS[0]}
    engine=${REPLY_FIELDS[1]}
    workload=${REPLY_FIELDS[2]}
    iteration=${REPLY_FIELDS[3]}
    order=${REPLY_FIELDS[4]}
    position=${REPLY_FIELDS[5]}
    elapsed=${REPLY_FIELDS[6]}
    exit_code=${REPLY_FIELDS[7]}
    validated=${REPLY_FIELDS[8]}
    [[ $run_id == "$expected_run_id" && $validated == true ]] || {
      perf_evidence_error 'sample identity or validation marker mismatch'
      return 1
    }
    perf_validate_positive_ns "$elapsed" || {
      perf_evidence_error 'invalid sample duration'
      return 1
    }
    if [[ $workload == first-spawn ]]; then
      [[ $rows -eq 2 && $engine == rust && $iteration == 0 && $order == rust-only \
        && $position == 1 && $exit_code == 0 ]] || {
        perf_evidence_error 'invalid first-spawn sample'
        return 1
      }
      key=first-spawn:0:rust
      [[ -z ${seen[$key]+present} ]] || {
        perf_evidence_error 'duplicate sample'
        return 1
      }
      seen[$key]=true
      PERF_SAMPLE_VALUES[first-spawn:rust]=" $elapsed"
      continue
    fi
    data_index=$((rows - 3))
    if ((data_index < 0 || data_index >= ${#workloads[@]} * 30 * 2)); then
      perf_evidence_error 'unexpected sample row'
      return 1
    fi
    workload_index=$((data_index / 60))
    expected_workload=${workloads[$workload_index]}
    expected_iteration=$(((data_index % 60) / 2 + 1))
    expected_position=$((data_index % 2 + 1))
    [[ $workload == "$expected_workload" && $iteration == "$expected_iteration" ]] || {
      perf_evidence_error 'sample workload or iteration order mismatch'
      return 1
    }
    case $workload in
      help|version|base-clean|disjoint-clean|disjoint-dirty|profile-provider-hooks-collision|pre-sync-failure) ;;
      *)
        perf_evidence_error 'unknown sample workload'
        return 1
        ;;
    esac
    [[ $engine == shell || $engine == rust ]] || {
      perf_evidence_error 'unknown sample engine'
      return 1
    }
    [[ $iteration =~ ^([1-9]|[12][0-9]|30)$ ]] || {
      perf_evidence_error 'invalid sample iteration'
      return 1
    }
    if ((10#$iteration % 2 == 1)); then
      expected_order=shell-rust
      if ((expected_position == 1)); then expected_engine=shell; else expected_engine=rust; fi
    else
      expected_order=rust-shell
      if ((expected_position == 1)); then expected_engine=rust; else expected_engine=shell; fi
    fi
    if [[ $workload == pre-sync-failure ]]; then expected_exit=1; else expected_exit=0; fi
    [[ $engine == "$expected_engine" && $order == "$expected_order" \
      && $position == "$expected_position" \
      && $exit_code == "$expected_exit" ]] || {
      perf_evidence_error 'sample order or result mismatch'
      return 1
    }
    key=$workload:$iteration:$engine
    [[ -z ${seen[$key]+present} ]] || {
      perf_evidence_error 'duplicate sample'
      return 1
    }
    seen[$key]=true
    PERF_SAMPLE_VALUES[$workload:$engine]="${PERF_SAMPLE_VALUES[$workload:$engine]-} $elapsed"
  done <"$file"
  [[ $rows -eq 422 && -n ${seen[first-spawn:0:rust]+present} ]] || {
    perf_evidence_error 'sample line count does not match the release contract'
    return 1
  }
  perf_compute_sample_stats first-spawn:rust 1 || return 1
  for workload in "${workloads[@]}"; do
    for iteration in {1..30}; do
      for expected_engine in shell rust; do
        [[ -n ${seen[$workload:$iteration:$expected_engine]+present} ]] || {
          perf_evidence_error 'sample coverage does not match the release contract'
          return 1
        }
      done
    done
    perf_compute_sample_stats "$workload:shell" 30 || return 1
    perf_compute_sample_stats "$workload:rust" 30 || return 1
  done
}

perf_validate_summary() {
  local file=$1 expected_run_id=$2 line='' rows=0 index workload samples
  local shell_median shell_p95 rust_median rust_p95 max_percent budget result
  local expected_workload expected_samples expected_percent expected_budget key
  local -a workloads=(first-spawn help version base-clean disjoint-clean \
    disjoint-dirty profile-provider-hooks-collision pre-sync-failure)
  local -a sample_counts=(1 30 30 30 30 30 30 30)
  local -a percentages=('' 75 75 95 75 75 75 75)
  local -a budgets=(100000000 24075000 23850000 4000000000 1602500000 \
    2010000000 12000000000 4000000000)

  PERF_SUMMARY_VALUES=()
  while :; do
    line=
    if IFS= read -r line; then
      :
    else
      [[ -z $line ]] || {
        perf_evidence_error 'unterminated summary record'
        return 1
      }
      break
    fi
    rows=$((rows + 1))
    if ((rows == 1)); then
      [[ $line == $'run_id\tworkload\tsamples\tshell_median_ns\tshell_p95_ns\trust_median_ns\trust_p95_ns\tmax_rust_percent\trust_p95_budget_ns\tresult' ]] || {
        perf_evidence_error 'invalid summary header'
        return 1
      }
      continue
    fi
    index=$((rows - 2))
    [[ $index -lt ${#workloads[@]} ]] || {
      perf_evidence_error 'unexpected summary row'
      return 1
    }
    perf_split_tsv_line "$line" 10 || {
      perf_evidence_error 'invalid summary record'
      return 1
    }
    expected_workload=${workloads[$index]}
    expected_samples=${sample_counts[$index]}
    expected_percent=${percentages[$index]}
    expected_budget=${budgets[$index]}
    [[ ${REPLY_FIELDS[0]} == "$expected_run_id" ]] || {
      perf_evidence_error 'summary run identity mismatch'
      return 1
    }
    workload=${REPLY_FIELDS[1]}
    samples=${REPLY_FIELDS[2]}
    shell_median=${REPLY_FIELDS[3]}
    shell_p95=${REPLY_FIELDS[4]}
    rust_median=${REPLY_FIELDS[5]}
    rust_p95=${REPLY_FIELDS[6]}
    max_percent=${REPLY_FIELDS[7]}
    budget=${REPLY_FIELDS[8]}
    result=${REPLY_FIELDS[9]}
    [[ $workload == "$expected_workload" && $samples == "$expected_samples" \
      && $max_percent == "$expected_percent" && $budget == "$expected_budget" \
      && $result == pass ]] || {
      perf_evidence_error 'summary policy or result mismatch'
      return 1
    }
    if ! perf_validate_positive_ns "$rust_median" \
      || ! perf_validate_positive_ns "$rust_p95"; then
      perf_evidence_error 'invalid native summary statistic'
      return 1
    fi
    key=$workload:rust
    [[ $rust_median == "${PERF_SAMPLE_MEDIANS[$key]-}" \
      && $rust_p95 == "${PERF_SAMPLE_P95S[$key]-}" ]] || {
      perf_evidence_error 'native summary statistic does not match samples'
      return 1
    }
    if [[ $workload == first-spawn ]]; then
      [[ -z $shell_median && -z $shell_p95 ]] || {
        perf_evidence_error 'first-spawn unexpectedly has shell statistics'
        return 1
      }
    else
      if ! perf_validate_positive_ns "$shell_median" \
        || ! perf_validate_positive_ns "$shell_p95"; then
        perf_evidence_error 'invalid shell summary statistic'
        return 1
      fi
      key=$workload:shell
      [[ $shell_median == "${PERF_SAMPLE_MEDIANS[$key]-}" \
        && $shell_p95 == "${PERF_SAMPLE_P95S[$key]-}" ]] || {
        perf_evidence_error 'shell summary statistic does not match samples'
        return 1
      }
    fi
    ((10#$rust_p95 <= 10#$budget)) || {
      perf_evidence_error 'native p95 exceeds its summary budget'
      return 1
    }
    if [[ -n $max_percent ]]; then
      ((10#$rust_median * 100 <= 10#$shell_median * 10#$max_percent)) || {
        perf_evidence_error 'native median exceeds its relative summary budget'
        return 1
      }
    fi
    for key in samples shell_median shell_p95 rust_median rust_p95 max_percent budget result; do
      case $key in
        samples) PERF_SUMMARY_VALUES[$workload:$key]=$samples ;;
        shell_median) PERF_SUMMARY_VALUES[$workload:$key]=$shell_median ;;
        shell_p95) PERF_SUMMARY_VALUES[$workload:$key]=$shell_p95 ;;
        rust_median) PERF_SUMMARY_VALUES[$workload:$key]=$rust_median ;;
        rust_p95) PERF_SUMMARY_VALUES[$workload:$key]=$rust_p95 ;;
        max_percent) PERF_SUMMARY_VALUES[$workload:$key]=$max_percent ;;
        budget) PERF_SUMMARY_VALUES[$workload:$key]=$budget ;;
        result) PERF_SUMMARY_VALUES[$workload:$key]=$result ;;
      esac
    done
  done <"$file"
  [[ $rows -eq 9 ]] || {
    perf_evidence_error 'summary row count does not match the release contract'
    return 1
  }
}

perf_validate_calibration() {
  local file=$1 expected_run_id=$2 shell_commit=$3 shell_tree=$4 rust_commit=$5
  local rust_tree=$6 shdeps_commit=$7 shdeps_tree=$8 git_blob=$9
  shift 9
  local bash_blob=$1 cargo_blob=$2 rustc_blob=$3 shell_executable_blob=$4
  local rust_executable_blob=$5 shdeps_executable_blob=$6 metadata_blob=$7
  local samples_blob=$8 summary_blob=$9
  local line='' rows=0 index workload samples shell_median rust_median rust_p95 budget
  local observed_percent headroom expected_observed expected_headroom
  local -a workloads=(first-spawn help version base-clean disjoint-clean \
    disjoint-dirty profile-provider-hooks-collision pre-sync-failure)

  while :; do
    line=
    if IFS= read -r line; then
      :
    else
      [[ -z $line ]] || {
        perf_evidence_error 'unterminated calibration record'
        return 1
      }
      break
    fi
    rows=$((rows + 1))
    if ((rows == 1)); then
      [[ $line == $'run_id\tshell_commit\tshell_tree\trust_commit\trust_tree\tshdeps_commit\tshdeps_tree\tgit_blob\tbash_blob\tcargo_blob\trustc_blob\tshell_executable_blob\trust_executable_blob\tshdeps_executable_blob\tmetadata_blob\tsamples_blob\tsummary_blob\tworkload\tsamples\tshell_median_ns\trust_median_ns\trust_p95_ns\trust_p95_budget_ns\tobserved_rust_percent\tbudget_headroom_percent' ]] || {
        perf_evidence_error 'invalid calibration header'
        return 1
      }
      continue
    fi
    index=$((rows - 2))
    [[ $index -lt ${#workloads[@]} ]] || {
      perf_evidence_error 'unexpected calibration row'
      return 1
    }
    perf_split_tsv_line "$line" 25 || {
      perf_evidence_error 'invalid calibration record'
      return 1
    }
    [[ ${REPLY_FIELDS[0]} == "$expected_run_id" \
      && ${REPLY_FIELDS[1]} == "$shell_commit" \
      && ${REPLY_FIELDS[2]} == "$shell_tree" \
      && ${REPLY_FIELDS[3]} == "$rust_commit" \
      && ${REPLY_FIELDS[4]} == "$rust_tree" \
      && ${REPLY_FIELDS[5]} == "$shdeps_commit" \
      && ${REPLY_FIELDS[6]} == "$shdeps_tree" \
      && ${REPLY_FIELDS[7]} == "$git_blob" \
      && ${REPLY_FIELDS[8]} == "$bash_blob" \
      && ${REPLY_FIELDS[9]} == "$cargo_blob" \
      && ${REPLY_FIELDS[10]} == "$rustc_blob" \
      && ${REPLY_FIELDS[11]} == "$shell_executable_blob" \
      && ${REPLY_FIELDS[12]} == "$rust_executable_blob" \
      && ${REPLY_FIELDS[13]} == "$shdeps_executable_blob" \
      && ${REPLY_FIELDS[14]} == "$metadata_blob" \
      && ${REPLY_FIELDS[15]} == "$samples_blob" \
      && ${REPLY_FIELDS[16]} == "$summary_blob" ]] || {
      perf_evidence_error 'calibration identity mismatch'
      return 1
    }
    workload=${REPLY_FIELDS[17]}
    [[ $workload == "${workloads[$index]}" ]] || {
      perf_evidence_error 'calibration workload order mismatch'
      return 1
    }
    samples=${REPLY_FIELDS[18]}
    shell_median=${REPLY_FIELDS[19]}
    rust_median=${REPLY_FIELDS[20]}
    rust_p95=${REPLY_FIELDS[21]}
    budget=${REPLY_FIELDS[22]}
    observed_percent=${REPLY_FIELDS[23]}
    headroom=${REPLY_FIELDS[24]}
    [[ $samples == "${PERF_SUMMARY_VALUES[$workload:samples]-}" \
      && $shell_median == "${PERF_SUMMARY_VALUES[$workload:shell_median]-}" \
      && $rust_median == "${PERF_SUMMARY_VALUES[$workload:rust_median]-}" \
      && $rust_p95 == "${PERF_SUMMARY_VALUES[$workload:rust_p95]-}" \
      && $budget == "${PERF_SUMMARY_VALUES[$workload:budget]-}" ]] || {
      perf_evidence_error 'calibration metrics do not match summary'
      return 1
    }
    if [[ -n $shell_median ]]; then
      expected_observed=$(((10#$rust_median * 100 + 10#$shell_median - 1) / \
        10#$shell_median))
    else
      expected_observed=
    fi
    expected_headroom=$(((10#$budget - 10#$rust_p95) * 100 / 10#$budget))
    [[ $observed_percent == "$expected_observed" \
      && $headroom == "$expected_headroom" ]] || {
      perf_evidence_error 'calibration ratio or headroom mismatch'
      return 1
    }
  done <"$file"
  [[ $rows -eq 9 ]] || {
    perf_evidence_error 'calibration row count does not match the release contract'
    return 1
  }
}

perf_set_evidence_expectations() {
  PERF_EXPECTED_RUN_ID=$1
  PERF_EXPECTED_SHELL_COMMIT=$2
  PERF_EXPECTED_SHELL_TREE=$3
  PERF_EXPECTED_RUST_COMMIT=$4
  PERF_EXPECTED_RUST_TREE=$5
  PERF_EXPECTED_DIRTY=$6
  PERF_EXPECTED_SHDEPS_COMMIT=$7
  PERF_EXPECTED_SHDEPS_TREE=$8
  PERF_EXPECTED_SHELL_SHDEPS_COMMIT=$9
  PERF_EXPECTED_SHDEPS_ABI=${10}
  PERF_EXPECTED_SHELL_EXECUTABLE=${11}
  PERF_EXPECTED_RUST_EXECUTABLE=${12}
  PERF_EXPECTED_SHDEPS_EXECUTABLE=${13}

  [[ $PERF_EXPECTED_RUN_ID =~ ^[0-9a-f]{40}$ \
    && $PERF_EXPECTED_SHELL_COMMIT =~ ^[0-9a-f]{40}$ \
    && $PERF_EXPECTED_SHELL_TREE =~ ^[0-9a-f]{40}$ \
    && $PERF_EXPECTED_RUST_COMMIT =~ ^[0-9a-f]{40}$ \
    && $PERF_EXPECTED_RUST_TREE =~ ^[0-9a-f]{40}$ \
    && $PERF_EXPECTED_SHDEPS_COMMIT =~ ^[0-9a-f]{40}$ \
    && $PERF_EXPECTED_SHDEPS_TREE =~ ^[0-9a-f]{40}$ \
    && $PERF_EXPECTED_SHELL_SHDEPS_COMMIT =~ ^[0-9a-f]{40}$ \
    && $PERF_EXPECTED_SHDEPS_ABI =~ ^[0-9]+$ \
    && $PERF_EXPECTED_DIRTY == false ]] || {
    perf_evidence_error 'invalid expected identity'
    return 1
  }
}

perf_validate_filesystem_description() {
  local value=$1

  [[ $value =~ ^type=(btrfs|ext2|ext3|ext4|f2fs|overlay|tmpfs|xfs|zfs|other)\;options=(async|dirsync|lazytime|noatime|nodev|nodiratime|noexec|nosuid|relatime|ro|rw)(,(async|dirsync|lazytime|noatime|nodev|nodiratime|noexec|nosuid|relatime|ro|rw))*$ ]]
}

perf_validate_driver() {
  local file=$1

  perf_load_key_value_tsv "$file" || return 1
  [[ $PERF_TSV_ROW_COUNT -eq 19 ]] || {
    perf_evidence_error 'driver field count mismatch'
    return 1
  }
  perf_require_loaded_value format performance-driver-v2 || return 1
  perf_require_loaded_value status passed || return 1
  perf_require_loaded_value artifact_location configured-output || return 1
  perf_require_loaded_value git_location system-tool || return 1
  perf_require_loaded_value bash_location system-tool || return 1
  perf_require_loaded_value cargo_location selected-build-tool || return 1
  perf_require_loaded_value rustc_location selected-build-tool || return 1
  perf_require_loaded_value client_path_policy controlled || return 1
  perf_require_loaded_value run_id "$PERF_EXPECTED_RUN_ID" || return 1
  perf_require_loaded_value shell_commit "$PERF_EXPECTED_SHELL_COMMIT" || return 1
  perf_require_loaded_value shell_tree "$PERF_EXPECTED_SHELL_TREE" || return 1
  perf_require_loaded_value rust_commit "$PERF_EXPECTED_RUST_COMMIT" || return 1
  perf_require_loaded_value rust_tree "$PERF_EXPECTED_RUST_TREE" || return 1
  perf_require_loaded_value rust_worktree_dirty "$PERF_EXPECTED_DIRTY" || return 1
  perf_require_loaded_value shdeps_commit "$PERF_EXPECTED_SHDEPS_COMMIT" || return 1
  perf_require_loaded_value shdeps_tree "$PERF_EXPECTED_SHDEPS_TREE" || return 1
  perf_require_loaded_value cargo_invocation "$PERF_CARGO_TEST_INVOCATION" || return 1
  perf_require_loaded_value cargo_test_name release_performance_gate || return 1
  perf_require_loaded_value cargo_exit_code 0
}

perf_validate_metadata() {
  local file=$1 cpu_count cpu_class runner_image arch filesystem key expected_version

  perf_load_key_value_tsv "$file" || return 1
  [[ $PERF_TSV_ROW_COUNT -eq 50 ]] || {
    perf_evidence_error 'metadata field count mismatch'
    return 1
  }
  perf_require_loaded_value format performance-metadata-v2 || return 1
  perf_require_loaded_value run_id "$PERF_EXPECTED_RUN_ID" || return 1
  perf_require_loaded_value shell_commit "$PERF_EXPECTED_SHELL_COMMIT" || return 1
  perf_require_loaded_value shell_tree "$PERF_EXPECTED_SHELL_TREE" || return 1
  perf_require_loaded_value rust_commit "$PERF_EXPECTED_RUST_COMMIT" || return 1
  perf_require_loaded_value rust_tree "$PERF_EXPECTED_RUST_TREE" || return 1
  perf_require_loaded_value rust_worktree_dirty "$PERF_EXPECTED_DIRTY" || return 1
  perf_require_loaded_value profile release || return 1
  perf_require_loaded_value samples 30 || return 1
  perf_require_loaded_value startup_warmups 10 || return 1
  perf_require_loaded_value update_warmups 2 || return 1
  perf_require_loaded_value disjoint_overlays 3 || return 1
  perf_require_loaded_value feature_selected_overlays 3 || return 1
  perf_require_loaded_value feature_declared_overlays 4 || return 1
  perf_require_loaded_value files_per_overlay 20 || return 1
  perf_require_loaded_value os linux || return 1
  arch=${PERF_TSV_VALUES[arch]-}
  [[ $arch == x86_64 || $arch == aarch64 ]] || {
    perf_evidence_error 'invalid public architecture'
    return 1
  }
  cpu_count=${PERF_TSV_VALUES[cpu_count]-}
  [[ $cpu_count =~ ^[1-9][0-9]{0,3}$ ]] || {
    perf_evidence_error 'invalid public CPU count'
    return 1
  }
  case $cpu_count in
    1) cpu_class=single ;;
    2|3|4) cpu_class=small ;;
    *) if ((10#$cpu_count <= 16)); then cpu_class=medium; else cpu_class=large; fi ;;
  esac
  perf_require_loaded_value cpu_class "$cpu_class" || return 1
  runner_image=$PERF_RUNNER_IMAGE
  perf_validate_public_runner_image "$runner_image" || return 1
  perf_require_loaded_value runner_image "$runner_image" || return 1
  perf_require_loaded_value command_timeout_ns 30000000000 || return 1
  perf_require_loaded_value artifact_location configured-output || return 1
  perf_require_loaded_value client_path_policy controlled || return 1

  for key in git bash cargo rustc; do
    case $key in
      git)
        perf_require_loaded_value git_location system-tool || return 1
        expected_version=$PERF_GIT_VERSION
        ;;
      bash)
        perf_require_loaded_value bash_location system-tool || return 1
        expected_version=$PERF_BASH_VERSION
        ;;
      cargo)
        perf_require_loaded_value cargo_location selected-build-tool || return 1
        expected_version=$PERF_CARGO_VERSION
        ;;
      rustc)
        perf_require_loaded_value rustc_location selected-build-tool || return 1
        expected_version=$PERF_RUSTC_VERSION
        ;;
    esac
    perf_validate_public_tool_identity "$key" "$expected_version" || return 1
    perf_require_loaded_value "${key}_version" "$expected_version" || return 1
  done
  perf_require_loaded_value git_blob "$PERF_GIT_BLOB" || return 1
  perf_require_loaded_value bash_blob "$PERF_BASH_BLOB" || return 1
  perf_require_loaded_value cargo_blob "$PERF_CARGO_BLOB" || return 1
  perf_require_loaded_value rustc_blob "$PERF_RUSTC_BLOB" || return 1
  perf_require_loaded_value shell_executable_location historical-checkout/bin/dot || return 1
  perf_require_loaded_value shell_executable_blob "$PERF_SHELL_EXECUTABLE_BLOB" || return 1
  perf_require_loaded_value rust_executable_location run-private-target/dot/release/dot || return 1
  perf_require_loaded_value rust_executable_blob "$PERF_RUST_EXECUTABLE_BLOB" || return 1
  perf_require_loaded_value shdeps_location provider-checkout || return 1
  perf_require_loaded_value current_shdeps_lock_revision "$PERF_EXPECTED_SHDEPS_COMMIT" || return 1
  perf_require_loaded_value shell_shdeps_lock_revision "$PERF_EXPECTED_SHELL_SHDEPS_COMMIT" || return 1
  perf_require_loaded_value shdeps_abi "$PERF_EXPECTED_SHDEPS_ABI" || return 1
  perf_require_loaded_value shdeps_commit "$PERF_EXPECTED_SHDEPS_COMMIT" || return 1
  perf_require_loaded_value shdeps_tree "$PERF_EXPECTED_SHDEPS_TREE" || return 1
  perf_require_loaded_value shdeps_executable_location run-private-target/shdeps/release/shdeps || return 1
  perf_require_loaded_value shdeps_executable_blob "$PERF_SHDEPS_EXECUTABLE_BLOB" || return 1
  for key in fixture_filesystem source_filesystem binary_filesystem; do
    filesystem=${PERF_TSV_VALUES[$key]-}
    perf_validate_filesystem_description "$filesystem" || {
      perf_evidence_error "invalid $key"
      return 1
    }
  done
}

perf_validate_evidence_payload() {
  local artifact_dir=$1 mode=${2:-live} file

  for file in driver.tsv metadata.tsv samples.tsv summary.tsv calibration.tsv; do
    perf_require_public_text "$artifact_dir/$file" || return 1
  done
  case $mode in
    live)
      perf_public_tool_version git "$PERF_GIT" --version || return 1
      PERF_GIT_VERSION=$REPLY
      perf_public_tool_version bash "$PERF_BASH" --version || return 1
      PERF_BASH_VERSION=$REPLY
      perf_public_tool_version cargo "$PERF_CARGO" --version || return 1
      PERF_CARGO_VERSION=$REPLY
      perf_public_tool_version rustc "$PERF_RUSTC" --version || return 1
      PERF_RUSTC_VERSION=$REPLY
      perf_file_blob "$PERF_GIT" || return 1; PERF_GIT_BLOB=$REPLY
      perf_file_blob "$PERF_BASH" || return 1; PERF_BASH_BLOB=$REPLY
      perf_file_blob "$PERF_CARGO" || return 1; PERF_CARGO_BLOB=$REPLY
      perf_file_blob "$PERF_RUSTC" || return 1; PERF_RUSTC_BLOB=$REPLY
      perf_file_blob "$PERF_EXPECTED_SHELL_EXECUTABLE" || return 1
      PERF_SHELL_EXECUTABLE_BLOB=$REPLY
      perf_file_blob "$PERF_EXPECTED_RUST_EXECUTABLE" || return 1
      PERF_RUST_EXECUTABLE_BLOB=$REPLY
      perf_file_blob "$PERF_EXPECTED_SHDEPS_EXECUTABLE" || return 1
      PERF_SHDEPS_EXECUTABLE_BLOB=$REPLY
      ;;
    sealed)
      for file in PERF_GIT_BLOB PERF_BASH_BLOB PERF_CARGO_BLOB PERF_RUSTC_BLOB \
        PERF_SHELL_EXECUTABLE_BLOB PERF_RUST_EXECUTABLE_BLOB \
        PERF_SHDEPS_EXECUTABLE_BLOB; do
        [[ ${!file:-} =~ ^[0-9a-f]{40}$ ]] || return 1
      done
      ;;
    *) return 1 ;;
  esac
  perf_file_blob "$artifact_dir/driver.tsv" || return 1; PERF_DRIVER_BLOB=$REPLY
  perf_file_blob "$artifact_dir/metadata.tsv" || return 1; PERF_METADATA_BLOB=$REPLY
  perf_file_blob "$artifact_dir/samples.tsv" || return 1; PERF_SAMPLES_BLOB=$REPLY
  perf_file_blob "$artifact_dir/summary.tsv" || return 1; PERF_SUMMARY_BLOB=$REPLY
  perf_file_blob "$artifact_dir/calibration.tsv" || return 1; PERF_CALIBRATION_BLOB=$REPLY

  perf_validate_driver "$artifact_dir/driver.tsv" || return 1
  perf_validate_metadata "$artifact_dir/metadata.tsv" || return 1
  perf_validate_samples "$artifact_dir/samples.tsv" "$PERF_EXPECTED_RUN_ID" || return 1
  perf_validate_summary "$artifact_dir/summary.tsv" "$PERF_EXPECTED_RUN_ID" || return 1
  perf_validate_calibration "$artifact_dir/calibration.tsv" "$PERF_EXPECTED_RUN_ID" \
    "$PERF_EXPECTED_SHELL_COMMIT" "$PERF_EXPECTED_SHELL_TREE" \
    "$PERF_EXPECTED_RUST_COMMIT" "$PERF_EXPECTED_RUST_TREE" \
    "$PERF_EXPECTED_SHDEPS_COMMIT" "$PERF_EXPECTED_SHDEPS_TREE" \
    "$PERF_GIT_BLOB" "$PERF_BASH_BLOB" "$PERF_CARGO_BLOB" "$PERF_RUSTC_BLOB" \
    "$PERF_SHELL_EXECUTABLE_BLOB" "$PERF_RUST_EXECUTABLE_BLOB" \
    "$PERF_SHDEPS_EXECUTABLE_BLOB" "$PERF_METADATA_BLOB" "$PERF_SAMPLES_BLOB" \
    "$PERF_SUMMARY_BLOB" || return 1
  PERF_EVIDENCE_VALIDATED=true
}

perf_validate_provisional_run() {
  local artifact_dir=$1
  shift

  PERF_EVIDENCE_VALIDATED=false
  perf_set_evidence_expectations "$@" || return 1
  perf_validate_artifact_set "$artifact_dir" provisional || return 1
  perf_validate_evidence_payload "$artifact_dir"
}

perf_write_completion_manifest() {
  local output=$1 temporary=$1.$$

  [[ ${PERF_EVIDENCE_VALIDATED:-false} == true ]] || {
    perf_evidence_error 'completion requires validated provisional evidence'
    return 1
  }
  {
    printf 'key\tvalue\n'
    printf 'format\tperformance-completion-v2\n'
    printf 'status\tpassed\n'
    printf 'run_id\t%s\n' "$PERF_EXPECTED_RUN_ID"
    printf 'shell_commit\t%s\n' "$PERF_EXPECTED_SHELL_COMMIT"
    printf 'shell_tree\t%s\n' "$PERF_EXPECTED_SHELL_TREE"
    printf 'rust_commit\t%s\n' "$PERF_EXPECTED_RUST_COMMIT"
    printf 'rust_tree\t%s\n' "$PERF_EXPECTED_RUST_TREE"
    printf 'shdeps_commit\t%s\n' "$PERF_EXPECTED_SHDEPS_COMMIT"
    printf 'shdeps_tree\t%s\n' "$PERF_EXPECTED_SHDEPS_TREE"
    printf 'current_shdeps_lock_revision\t%s\n' "$PERF_EXPECTED_SHDEPS_COMMIT"
    printf 'shell_shdeps_lock_revision\t%s\n' "$PERF_EXPECTED_SHELL_SHDEPS_COMMIT"
    printf 'shdeps_abi\t%s\n' "$PERF_EXPECTED_SHDEPS_ABI"
    printf 'git_blob\t%s\n' "$PERF_GIT_BLOB"
    printf 'bash_blob\t%s\n' "$PERF_BASH_BLOB"
    printf 'cargo_blob\t%s\n' "$PERF_CARGO_BLOB"
    printf 'rustc_blob\t%s\n' "$PERF_RUSTC_BLOB"
    printf 'shell_executable_blob\t%s\n' "$PERF_SHELL_EXECUTABLE_BLOB"
    printf 'rust_executable_blob\t%s\n' "$PERF_RUST_EXECUTABLE_BLOB"
    printf 'shdeps_executable_blob\t%s\n' "$PERF_SHDEPS_EXECUTABLE_BLOB"
    printf 'driver_blob\t%s\n' "$PERF_DRIVER_BLOB"
    printf 'metadata_blob\t%s\n' "$PERF_METADATA_BLOB"
    printf 'samples_blob\t%s\n' "$PERF_SAMPLES_BLOB"
    printf 'summary_blob\t%s\n' "$PERF_SUMMARY_BLOB"
    printf 'calibration_blob\t%s\n' "$PERF_CALIBRATION_BLOB"
    printf 'sample_lines\t422\n'
    printf 'summary_data_rows\t8\n'
    printf 'calibration_data_rows\t8\n'
    printf 'cargo_invocation\t%s\n' "$PERF_CARGO_TEST_INVOCATION"
    printf 'cargo_test_name\trelease_performance_gate\n'
    printf 'cargo_exit_code\t0\n'
  } >"$temporary" || return 1
  "$PERF_MV" -f --no-target-directory -- "$temporary" "$output" || return 1
}

perf_validate_completion_manifest() {
  local file=$1

  perf_require_public_text "$file" || return 1
  perf_load_key_value_tsv "$file" || return 1
  [[ $PERF_TSV_ROW_COUNT -eq 30 ]] || {
    perf_evidence_error 'completion manifest field count mismatch'
    return 1
  }
  perf_require_loaded_value format performance-completion-v2 || return 1
  perf_require_loaded_value status passed || return 1
  perf_require_loaded_value run_id "$PERF_EXPECTED_RUN_ID" || return 1
  perf_require_loaded_value shell_commit "$PERF_EXPECTED_SHELL_COMMIT" || return 1
  perf_require_loaded_value shell_tree "$PERF_EXPECTED_SHELL_TREE" || return 1
  perf_require_loaded_value rust_commit "$PERF_EXPECTED_RUST_COMMIT" || return 1
  perf_require_loaded_value rust_tree "$PERF_EXPECTED_RUST_TREE" || return 1
  perf_require_loaded_value shdeps_commit "$PERF_EXPECTED_SHDEPS_COMMIT" || return 1
  perf_require_loaded_value shdeps_tree "$PERF_EXPECTED_SHDEPS_TREE" || return 1
  perf_require_loaded_value current_shdeps_lock_revision "$PERF_EXPECTED_SHDEPS_COMMIT" || return 1
  perf_require_loaded_value shell_shdeps_lock_revision "$PERF_EXPECTED_SHELL_SHDEPS_COMMIT" || return 1
  perf_require_loaded_value shdeps_abi "$PERF_EXPECTED_SHDEPS_ABI" || return 1
  perf_require_loaded_value git_blob "$PERF_GIT_BLOB" || return 1
  perf_require_loaded_value bash_blob "$PERF_BASH_BLOB" || return 1
  perf_require_loaded_value cargo_blob "$PERF_CARGO_BLOB" || return 1
  perf_require_loaded_value rustc_blob "$PERF_RUSTC_BLOB" || return 1
  perf_require_loaded_value shell_executable_blob "$PERF_SHELL_EXECUTABLE_BLOB" || return 1
  perf_require_loaded_value rust_executable_blob "$PERF_RUST_EXECUTABLE_BLOB" || return 1
  perf_require_loaded_value shdeps_executable_blob "$PERF_SHDEPS_EXECUTABLE_BLOB" || return 1
  perf_require_loaded_value driver_blob "$PERF_DRIVER_BLOB" || return 1
  perf_require_loaded_value metadata_blob "$PERF_METADATA_BLOB" || return 1
  perf_require_loaded_value samples_blob "$PERF_SAMPLES_BLOB" || return 1
  perf_require_loaded_value summary_blob "$PERF_SUMMARY_BLOB" || return 1
  perf_require_loaded_value calibration_blob "$PERF_CALIBRATION_BLOB" || return 1
  perf_require_loaded_value sample_lines 422 || return 1
  perf_require_loaded_value summary_data_rows 8 || return 1
  perf_require_loaded_value calibration_data_rows 8 || return 1
  perf_require_loaded_value cargo_invocation "$PERF_CARGO_TEST_INVOCATION" || return 1
  perf_require_loaded_value cargo_test_name release_performance_gate || return 1
  perf_require_loaded_value cargo_exit_code 0
}

perf_load_publication_expectations() {
  local artifact_dir=$1 completion=$1/completion.tsv key value variable

  perf_require_public_text "$completion" || return 1
  perf_load_key_value_tsv "$completion" || return 1
  [[ $PERF_TSV_ROW_COUNT -eq 30 ]] || {
    perf_evidence_error 'completion manifest field count mismatch'
    return 1
  }
  perf_require_loaded_value format performance-completion-v2 || return 1
  perf_require_loaded_value status passed || return 1
  for key in run_id shell_commit shell_tree rust_commit rust_tree shdeps_commit \
    shdeps_tree current_shdeps_lock_revision shell_shdeps_lock_revision git_blob \
    bash_blob cargo_blob rustc_blob shell_executable_blob rust_executable_blob \
    shdeps_executable_blob driver_blob metadata_blob samples_blob summary_blob \
    calibration_blob; do
    value=${PERF_TSV_VALUES[$key]-}
    [[ $value =~ ^[0-9a-f]{40}$ ]] || {
      perf_evidence_error "invalid $key"
      return 1
    }
  done
  [[ ${PERF_TSV_VALUES[current_shdeps_lock_revision]} == \
    "${PERF_TSV_VALUES[shdeps_commit]}" ]] || return 1
  [[ ${PERF_TSV_VALUES[shdeps_abi]-} =~ ^[0-9]+$ ]] || return 1
  perf_require_loaded_value sample_lines 422 || return 1
  perf_require_loaded_value summary_data_rows 8 || return 1
  perf_require_loaded_value calibration_data_rows 8 || return 1
  perf_require_loaded_value cargo_invocation "$PERF_CARGO_TEST_INVOCATION" || return 1
  perf_require_loaded_value cargo_test_name release_performance_gate || return 1
  perf_require_loaded_value cargo_exit_code 0 || return 1

  perf_set_evidence_expectations \
    "${PERF_TSV_VALUES[run_id]}" \
    "${PERF_TSV_VALUES[shell_commit]}" \
    "${PERF_TSV_VALUES[shell_tree]}" \
    "${PERF_TSV_VALUES[rust_commit]}" \
    "${PERF_TSV_VALUES[rust_tree]}" false \
    "${PERF_TSV_VALUES[shdeps_commit]}" \
    "${PERF_TSV_VALUES[shdeps_tree]}" \
    "${PERF_TSV_VALUES[shell_shdeps_lock_revision]}" \
    "${PERF_TSV_VALUES[shdeps_abi]}" /dev/null /dev/null /dev/null || return 1
  PERF_GIT_BLOB=${PERF_TSV_VALUES[git_blob]}
  PERF_BASH_BLOB=${PERF_TSV_VALUES[bash_blob]}
  PERF_CARGO_BLOB=${PERF_TSV_VALUES[cargo_blob]}
  PERF_RUSTC_BLOB=${PERF_TSV_VALUES[rustc_blob]}
  PERF_SHELL_EXECUTABLE_BLOB=${PERF_TSV_VALUES[shell_executable_blob]}
  PERF_RUST_EXECUTABLE_BLOB=${PERF_TSV_VALUES[rust_executable_blob]}
  PERF_SHDEPS_EXECUTABLE_BLOB=${PERF_TSV_VALUES[shdeps_executable_blob]}

  perf_require_public_text "$artifact_dir/metadata.tsv" || return 1
  perf_load_key_value_tsv "$artifact_dir/metadata.tsv" || return 1
  [[ $PERF_TSV_ROW_COUNT -eq 50 ]] || return 1
  PERF_RUNNER_IMAGE=${PERF_TSV_VALUES[runner_image]-}
  perf_validate_public_runner_image "$PERF_RUNNER_IMAGE" || return 1
  for key in git bash cargo rustc; do
    value=${PERF_TSV_VALUES[${key}_version]-}
    perf_validate_public_tool_identity "$key" "$value" || return 1
    case $key in
      git) variable=PERF_GIT_VERSION ;;
      bash) variable=PERF_BASH_VERSION ;;
      cargo) variable=PERF_CARGO_VERSION ;;
      rustc) variable=PERF_RUSTC_VERSION ;;
    esac
    printf -v "$variable" '%s' "$value"
  done
}

perf_validate_publication_path() {
  local publication=$1 parent name mode

  [[ $publication == /* && $publication != *$'\n'* && $publication != *$'\r'* \
    && -d $publication && ! -L $publication && -O $publication ]] || {
    perf_evidence_error 'publication root is not a safe regular directory'
    return 1
  }
  parent=${publication%/*}
  name=${publication##*/}
  [[ -d $parent && ! -L $parent ]] || return 1
  [[ $(cd -P -- "$parent" && pwd -P)/$name == "$publication" ]] || return 1
  [[ $name =~ \.run-([0-9a-f]{40})$ ]] || {
    perf_evidence_error 'publication path does not contain a unique run identity'
    return 1
  }
  PERF_PUBLICATION_PATH_RUN_ID=${BASH_REMATCH[1]}
  mode=$("$PERF_STAT" -Lc '%a' -- "$publication") || return 1
  [[ $mode =~ ^[0-7]{3,4}$ ]] || return 1
  (( (8#$mode & 077) == 0 )) || {
    perf_evidence_error 'publication directory is not private'
    return 1
  }
}

perf_validate_publication_for_upload() {
  local publication=$1

  PERF_EVIDENCE_VALIDATED=false
  perf_validate_publication_path "$publication" || return 1
  perf_validate_artifact_set "$publication" complete || return 1
  perf_load_publication_expectations "$publication" || return 1
  [[ $PERF_EXPECTED_RUN_ID == "$PERF_PUBLICATION_PATH_RUN_ID" ]] || {
    perf_evidence_error 'publication path and evidence run identities differ'
    return 1
  }
  perf_validate_evidence_payload "$publication" sealed || return 1
  if ! perf_validate_completion_manifest "$publication/completion.tsv"; then
    PERF_EVIDENCE_VALIDATED=false
    return 1
  fi
}

perf_resolve_publication_validation_tools() {
  PERF_SCRATCH_PARENT=${RUNNER_TEMP:-${TMPDIR:-/tmp}}
  PERF_DRIVER_HOME=${HOME:?HOME is required}
  REPLY_PATH=
  perf_append_path /usr/bin
  perf_append_path /bin
  perf_append_path /usr/local/bin
  PERF_CLIENT_PATH=$REPLY_PATH
  perf_first_tool /usr/bin/git /bin/git /usr/local/bin/git || return 1
  PERF_GIT=$REPLY
  perf_first_tool /usr/bin/env /bin/env || return 1
  PERF_ENV=$REPLY
  perf_first_tool /usr/bin/od /bin/od || return 1
  PERF_OD=$REPLY
}

perf_expose_publication_path() {
  local publication=$1 output=${GITHUB_OUTPUT:-}

  [[ ${PERF_EVIDENCE_VALIDATED:-false} == true ]] || return 1
  perf_validate_publication_path "$publication" || return 1
  [[ -z $output ]] && return 0
  [[ $output == /* && -f $output && ! -L $output ]] || return 1
  printf 'publication_dir=%s\n' "$publication" >>"$output"
}

perf_validate_completed_run() {
  local artifact_dir=$1
  shift

  PERF_EVIDENCE_VALIDATED=false
  perf_set_evidence_expectations "$@" || return 1
  perf_validate_expected_completed_run "$artifact_dir"
}

perf_validate_expected_completed_run() {
  local artifact_dir=$1

  PERF_EVIDENCE_VALIDATED=false
  perf_validate_artifact_set "$artifact_dir" complete || return 1
  perf_validate_evidence_payload "$artifact_dir" || return 1
  if ! perf_validate_completion_manifest "$artifact_dir/completion.tsv"; then
    PERF_EVIDENCE_VALIDATED=false
    return 1
  fi
}

perf_require_empty_artifact_directory() {
  local artifact_dir=$1 path

  for path in "$artifact_dir"/* "$artifact_dir"/.[!.]* "$artifact_dir"/..?*; do
    [[ -e $path || -L $path ]] || continue
    perf_evidence_error 'publication directory is not empty'
    return 1
  done
}

perf_copy_regular_file() {
  local source=$1 destination=$2

  [[ -f $source && ! -L $source ]] || {
    perf_evidence_error 'publication source is not a regular file'
    return 1
  }
  [[ ! -e $destination && ! -L $destination ]] || {
    perf_evidence_error 'publication target already exists'
    return 1
  }
  "$PERF_CP" --no-dereference --no-target-directory -- "$source" "$destination" || return 1
  [[ -f $destination && ! -L $destination ]] || {
    perf_evidence_error 'publication copy is not a regular file'
    return 1
  }
}

perf_build_sealed_publication() {
  local staging=$1 completion=$2 sealed=$3 file
  local -a files=(driver.tsv metadata.tsv samples.tsv summary.tsv calibration.tsv)

  [[ ! -e $sealed && ! -L $sealed ]] || {
    perf_evidence_error 'run-specific publication staging already exists'
    return 1
  }
  (umask 077 && "$PERF_MKDIR" -- "$sealed") || return 1
  for file in "${files[@]}"; do
    perf_copy_regular_file "$staging/$file" "$sealed/$file" || return 1
  done
  # Validate the exact copies before adding the success marker.
  PERF_EVIDENCE_VALIDATED=false
  perf_validate_artifact_set "$sealed" provisional || return 1
  perf_validate_evidence_payload "$sealed" || return 1
  perf_copy_regular_file "$completion" "$sealed/completion.tsv" || return 1
  perf_validate_expected_completed_run "$sealed"
}

perf_publish_sealed_directory() {
  local sealed=$1 artifact_dir=$2

  # A caller cannot turn a validated copy into a published result: validate
  # the complete sealed set again immediately before its atomic rename.
  perf_validate_expected_completed_run "$sealed" || return 1
  [[ ! -e $artifact_dir && ! -L $artifact_dir ]] || {
    perf_evidence_error 'unique publication target already exists'
    return 1
  }
  "$PERF_MV" --no-target-directory -- "$sealed" "$artifact_dir" || return 1
  if ! perf_validate_expected_completed_run "$artifact_dir"; then
    # Remove the failed publication from the workflow-visible path. Moving the
    # exact directory back is safe and preserves it for diagnosis/cleanup.
    if [[ ! -e $sealed && ! -L $sealed ]]; then
      "$PERF_MV" --no-target-directory -- "$artifact_dir" "$sealed" ||
        "$PERF_RM" -f -- "$artifact_dir/completion.tsv" || :
    else
      "$PERF_RM" -f -- "$artifact_dir/completion.tsv" || :
    fi
    return 1
  fi
}

perf_publish_completed_run() {
  local staging=$1 completion=$2 artifact_root=$3 run_id=$4 parent name sealed
  local publication

  PERF_EVIDENCE_VALIDATED=false
  if [[ ${PERF_LIFECYCLE_ARTIFACT:-} != "$artifact_root" ]] \
    || ! perf_lifecycle_lock_is_held; then
    perf_evidence_error 'publication requires the exclusive lifecycle lock'
    return 1
  fi
  [[ $run_id =~ ^[0-9a-f]{40}$ ]] || return 1
  parent=${artifact_root%/*}
  name=${artifact_root##*/}
  sealed=$parent/.$name.publication.$run_id
  publication=$artifact_root.run-$run_id
  [[ ! -e $publication && ! -L $publication ]] || {
    perf_evidence_error 'unique publication target already exists'
    return 1
  }
  if ! perf_build_sealed_publication "$staging" "$completion" "$sealed"; then
    [[ -d $sealed && ! -L $sealed ]] && "$PERF_RM" -rf -- "$sealed"
    return 1
  fi
  if ! perf_publish_sealed_directory "$sealed" "$publication"; then
    [[ -d $sealed && ! -L $sealed ]] && "$PERF_RM" -rf -- "$sealed"
    return 1
  fi
  REPLY=$publication
}

perf_main() {
  [[ $# -eq 0 ]] || perf_usage

  local script=${BASH_SOURCE[0]} script_dir root baseline_manifest
  local key value extra shell_commit='' shdeps_commit='' shell_shdeps_commit=''
  local shdeps_abi='' current_commit='' current_dirty=false
  local run_id current_tree current_status_blob shell_tree shdeps_tree
  local artifact_config artifact_dir scratch staging completion_path candidate_root shell_root
  local publication
  local cargo_target_root shdeps_target dot_target command_supervisor
  local shdeps_source shdeps_origin shdeps_parent shdeps_root shdeps_run_root
  local cargo_status

  case $script in
    */*) script_dir=${script%/*} ;;
    *) script_dir=. ;;
  esac
  root=$(cd -P -- "$script_dir/.." && pwd -P)
  perf_resolve_artifact_tools || {
    printf 'error: required artifact tools were not found in system locations\n' >&2
    exit 1
  }
  trap perf_finish EXIT
  artifact_config=${DOT_PERF_ARTIFACT_DIR:-target/performance}
  perf_new_run_id "$root" "$artifact_config" || {
    printf 'error: cannot create performance run identity\n' >&2
    exit 1
  }
  run_id=$REPLY
  perf_preflight_artifact_destination "$root" "$artifact_config" || exit 1
  perf_preflight_artifact_destination "$root" "$artifact_config" "$run_id" || exit 1
  perf_acquire_artifact_lifecycle "$root" "$artifact_config" || exit 1
  artifact_dir=$REPLY
  perf_invalidate_prior_completion "$artifact_dir" || exit 1
  perf_remove_stale_publication_dirs "$artifact_dir" || exit 1
  perf_prepare_artifacts "$root" "$artifact_dir" || exit 1
  artifact_dir=$REPLY
  perf_resolve_tools || {
    printf 'error: required performance tools were not found in system locations\n' >&2
    exit 1
  }
  perf_require_command_supervisor_platform || exit 1

  PERF_SCRATCH_PARENT=${RUNNER_TEMP:-${TMPDIR:-/tmp}}
  [[ $PERF_SCRATCH_PARENT == /* && -d $PERF_SCRATCH_PARENT \
    && -w $PERF_SCRATCH_PARENT && -x $PERF_SCRATCH_PARENT ]] || {
    printf 'error: scratch parent must be an absolute writable directory\n' >&2
    exit 1
  }
  scratch=$("$PERF_MKTEMP" -d "$PERF_SCRATCH_PARENT/dot-performance.XXXXXX") || {
    printf 'error: cannot create performance scratch directory\n' >&2
    exit 1
  }
  [[ -n $scratch && -d $scratch ]] || {
    printf 'error: cannot create performance scratch directory\n' >&2
    exit 1
  }
  PERF_DRIVER_HOME=$scratch/driver-home
  PERF_BUILD_HOME=$PERF_DRIVER_HOME/build-home
  PERF_CARGO_HOME=$PERF_DRIVER_HOME/cargo-home
  PERF_RUN_SCRATCH=$scratch
  staging=$scratch/evidence
  completion_path=$scratch/completion.tsv
  "$PERF_MKDIR" -p -- "$PERF_DRIVER_HOME" "$PERF_BUILD_HOME" "$PERF_CARGO_HOME" \
    "$staging"
  perf_require_executable_scratch "$scratch" || exit 1
  trap perf_finish EXIT

  perf_source_state "$root" || exit 1
  current_commit=$REPLY_SHA
  current_tree=$REPLY_TREE
  current_status_blob=$REPLY_STATUS_BLOB
  current_dirty=$REPLY_DIRTY

  if [[ $current_dirty == true ]]; then
    printf 'error: performance evidence requires a clean committed worktree\n' >&2
    exit 1
  fi
  candidate_root=$scratch/candidate
  perf_clone_source_snapshot "$root" "$current_commit" "$candidate_root" candidate || exit 1
  [[ $REPLY_TREE == "$current_tree" ]] || {
    printf 'error: candidate source snapshot tree mismatch\n' >&2
    exit 1
  }
  current_status_blob=$REPLY_STATUS_BLOB

  baseline_manifest=$candidate_root/support/performance-baseline-v1.tsv
  while IFS=$'\t' read -r key value extra; do
    [[ $key == shell_commit ]] || continue
    [[ -z $shell_commit && -z ${extra:-} ]] || shell_commit=invalid
    [[ $shell_commit == invalid ]] || shell_commit=$value
  done <"$baseline_manifest"
  [[ $shell_commit =~ ^[0-9a-f]{40}$ ]] || {
    printf 'error: invalid shell baseline in %s\n' "$baseline_manifest" >&2
    exit 1
  }
  perf_lock_identity "$candidate_root" || {
    printf 'error: invalid Shdeps revision in support/shdeps.lock\n' >&2
    exit 1
  }
  shdeps_commit=$REPLY_REVISION
  shdeps_abi=$REPLY_ABI

  perf_git -C "$candidate_root" cat-file -e "$shell_commit^{commit}"
  perf_git -C "$candidate_root" merge-base --is-ancestor "$shell_commit" "$current_commit" || {
    printf 'error: shell baseline %s is not an ancestor of %s\n' \
      "$shell_commit" "$current_commit" >&2
    exit 1
  }
  shell_root=$scratch/shell-baseline
  perf_clone_source_snapshot "$candidate_root" "$shell_commit" "$shell_root" \
    'shell baseline' || exit 1
  shell_tree=$REPLY_TREE
  [[ -x $shell_root/bin/dot && -f $shell_root/lib/dot/main.sh ]] || {
    printf 'error: pinned baseline is not a complete Bash-engine checkout\n' >&2
    exit 1
  }
  [[ ! -e $candidate_root/lib/dot/main.sh ]] || {
    printf 'error: current checkout still contains the private Bash engine\n' >&2
    exit 1
  }
  perf_lock_identity "$shell_root" || {
    printf 'error: invalid historical Shdeps lock\n' >&2
    exit 1
  }
  shell_shdeps_commit=$REPLY_REVISION
  [[ $REPLY_ABI == "$shdeps_abi" ]] || {
    printf 'error: current and historical Shdeps ABIs are incompatible\n' >&2
    exit 1
  }

  shdeps_source=${DOT_PERF_SHDEPS_SOURCE:-https://github.com/cgraf78/shdeps.git}
  shdeps_origin=$scratch/shdeps-origin.git
  shdeps_parent=$scratch/provider
  shdeps_root=$shdeps_parent/shdeps
  "$PERF_MKDIR" -p -- "$shdeps_parent"
  perf_git clone --mirror --no-local --quiet "$shdeps_source" "$shdeps_origin"
  perf_require_dissociated_repository "$shdeps_origin" || exit 1
  perf_git -C "$shdeps_origin" cat-file -e "$shdeps_commit^{commit}"
  perf_git clone --no-local --quiet "$shdeps_origin" "$shdeps_root"
  perf_git -C "$shdeps_root" checkout --quiet --detach "$shdeps_commit"
  [[ $(perf_git -C "$shdeps_root" rev-parse HEAD) == "$shdeps_commit" ]] || {
    printf 'error: Shdeps checkout identity mismatch\n' >&2
    exit 1
  }
  perf_seal_source_snapshot "$shdeps_root" "$shdeps_commit" provider || exit 1
  shdeps_tree=$REPLY_TREE

  PERF_CURRENT_ROOT=$candidate_root
  PERF_CURRENT_COMMIT=$current_commit
  PERF_CURRENT_TREE=$current_tree
  PERF_CURRENT_STATUS_BLOB=$current_status_blob
  PERF_SHELL_SOURCE_ROOT=$shell_root
  PERF_SHELL_COMMIT=$shell_commit
  PERF_SHELL_TREE=$shell_tree
  PERF_SHDEPS_SOURCE_ROOT=$shdeps_root
  PERF_SHDEPS_COMMIT=$shdeps_commit
  PERF_SHDEPS_TREE=$shdeps_tree
  PERF_CLEAN_STATUS_BLOB=e69de29bb2d1d6434b8b29ae775ad8c2e48c5391
  perf_require_all_source_identities pre-build || exit 1

  command_supervisor=$scratch/performance-command-supervisor
  perf_build_command_supervisor \
    "$candidate_root/support/performance-command-supervisor.rs" \
    "$command_supervisor" || exit 1
  PERF_COMMAND_SUPERVISOR=$command_supervisor
  perf_require_all_source_identities post-supervisor-build || exit 1

  DOT_BUILD_COMMIT=$current_commit
  DOT_PERF_ARTIFACT_DIR=$staging
  DOT_PERF_CURRENT_DIRTY=$current_dirty
  DOT_PERF_CURRENT_SHA=$current_commit
  DOT_PERF_RUN_ID=$run_id
  perf_public_runner_image
  PERF_RUNNER_IMAGE=$REPLY
  DOT_PERF_RUNNER_IMAGE=$PERF_RUNNER_IMAGE
  cargo_target_root=$scratch/cargo-target
  shdeps_target=$cargo_target_root/shdeps
  dot_target=$cargo_target_root/dot
  DOT_PERF_SHDEPS_ROOT=$shdeps_root
  DOT_PERF_SHDEPS_BINARY=$shdeps_target/release/shdeps
  DOT_PERF_SHELL_ROOT=$shell_root
  SHDEPS_BUILD_COMMIT=$shdeps_commit
  export DOT_BUILD_COMMIT DOT_PERF_ARTIFACT_DIR DOT_PERF_CURRENT_DIRTY
  export DOT_PERF_CURRENT_SHA DOT_PERF_RUN_ID DOT_PERF_SHDEPS_BINARY
  export DOT_PERF_RUNNER_IMAGE
  export DOT_PERF_SHDEPS_ROOT
  export DOT_PERF_SHELL_ROOT
  export SHDEPS_BUILD_COMMIT
  PERF_CARGO_TARGET_DIR=$shdeps_target
  cargo_status=0
  if perf_provider_build_release "$shdeps_root/Cargo.toml"; then
    :
  else
    cargo_status=$?
  fi
  [[ $cargo_status -eq 0 ]] || exit "$cargo_status"
  perf_require_all_source_identities post-provider-build || exit 1
  shdeps_run_root=$scratch/provider-run/shdeps
  perf_prepare_provider_run_root "$shdeps_root" "$shdeps_target/release/shdeps" \
    "$shdeps_run_root" "$shdeps_commit" || exit 1
  DOT_PERF_SHDEPS_ROOT=$shdeps_run_root
  PERF_CARGO_TARGET_DIR=$dot_target
  perf_require_all_source_identities pre-measurement || exit 1
  perf_release_cargo_test_contract "$candidate_root" || exit 1
  cargo_status=0
  if perf_cargo "${PERF_CARGO_TEST_ARGS[@]}"; then
    :
  else
    cargo_status=$?
  fi
  [[ $cargo_status -eq 0 ]] || exit "$cargo_status"
  perf_require_all_source_identities pre-finalization || exit 1
  perf_write_final_driver "$staging" "$run_id" "$shell_commit" "$shell_tree" \
    "$current_commit" "$current_tree" "$current_dirty" "$shdeps_commit" "$shdeps_tree" || exit 1
  perf_validate_provisional_run "$staging" "$run_id" "$shell_commit" "$shell_tree" \
    "$current_commit" "$current_tree" "$current_dirty" "$shdeps_commit" "$shdeps_tree" \
    "$shell_shdeps_commit" "$shdeps_abi" "$shell_root/bin/dot" \
    "$dot_target/release/dot" "$shdeps_target/release/shdeps" || exit 1
  perf_require_all_source_identities pre-final-seal || exit 1
  perf_write_completion_manifest "$completion_path" || exit 1
  # Reload the manifest from disk before any passed artifact can be published.
  perf_validate_completion_manifest "$completion_path" || exit 1
  perf_publish_completed_run "$staging" "$completion_path" "$artifact_dir" "$run_id" || exit 1
  publication=$REPLY
  perf_expose_publication_path "$publication" || exit 1
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
  if [[ $# -eq 2 && $1 == --validate-publication ]]; then
    perf_resolve_artifact_tools || exit 1
    perf_resolve_publication_validation_tools || exit 1
    perf_release_cargo_test_contract candidate || exit 1
    perf_validate_publication_for_upload "$2"
  else
    perf_main "$@"
  fi
fi
