# shellcheck shell=bash
# Optional Shdeps provider bootstrap pinned by the active dot release.

# Provider selection mutates and exports these variables for Shdeps. Preserve
# the process inputs separately so a later Dot re-exec can distinguish genuine
# caller policy from values derived by the configuration being replaced.
_DOT_SHDEPS_CALLER_FORCE_SET=${SHDEPS_FORCE+x}
_DOT_SHDEPS_CALLER_FORCE=${SHDEPS_FORCE-}
_DOT_SHDEPS_CALLER_LIB_SET=${SHDEPS_LIB+x}
_DOT_SHDEPS_CALLER_LIB=${SHDEPS_LIB-}

_dot_shdeps_restore_caller_env() {
  if [[ $_DOT_SHDEPS_CALLER_FORCE_SET == x ]]; then
    SHDEPS_FORCE=$_DOT_SHDEPS_CALLER_FORCE
    export SHDEPS_FORCE
  else
    unset SHDEPS_FORCE
  fi
  if [[ $_DOT_SHDEPS_CALLER_LIB_SET == x ]]; then
    SHDEPS_LIB=$_DOT_SHDEPS_CALLER_LIB
    export SHDEPS_LIB
  else
    unset SHDEPS_LIB
  fi
}

_dot_shdeps_lock_value() {
  local key=$1 line index=0 revision='' install_sha256='' abi=''
  while IFS= read -r line || [[ -n "$line" ]]; do
    index=$((index + 1))
    case $index:$line in
      1:revision=*) revision=${line#*=} ;;
      2:install_sha256=*) install_sha256=${line#*=} ;;
      3:abi=*) abi=${line#*=} ;;
      *) return 1 ;;
    esac
  done <"$DOT_SOURCE_ROOT/support/shdeps.lock"
  [[ $index -eq 3 && $revision =~ ^[0-9a-f]{40}$ &&
    $install_sha256 =~ ^[0-9a-f]{64}$ && $abi =~ ^[1-9][0-9]*$ ]] || return 1
  case $key in
    revision) printf '%s\n' "$revision" ;;
    install_sha256) printf '%s\n' "$install_sha256" ;;
    abi) printf '%s\n' "$abi" ;;
    *) return 1 ;;
  esac
}

_dot_shdeps_configure_env() {
  dot_xdg_path config shdeps || return 1
  SHDEPS_CONF_DIR=$REPLY
  SHDEPS_HOOKS_DIR=$SHDEPS_CONF_DIR/hooks.d
  SHDEPS_INSTALL_DIR=${SHDEPS_INSTALL_DIR:-$HOME/.local/share}
  SHDEPS_BIN_DIR=${SHDEPS_BIN_DIR:-$HOME/.local/bin}
  SHDEPS_GIT_DEV_DIR=${SHDEPS_GIT_DEV_DIR:-$HOME/git}
  export SHDEPS_CONF_DIR SHDEPS_HOOKS_DIR SHDEPS_INSTALL_DIR
  export SHDEPS_BIN_DIR SHDEPS_GIT_DEV_DIR
  if [[ "${DOT_FORCE:-0}" -eq 1 ]]; then
    export SHDEPS_FORCE=1
  fi
  [[ "${DOT_QUIET:-0}" -eq 1 ]] && export SHDEPS_QUIET=1
  # Optional flag propagation must not become the function's result. Provider
  # setup is successful when the roots above resolved, including the ordinary
  # non-force, non-quiet path.
  return 0
}

_dot_shdeps_path_owned() {
  local path=$1 output uid mode

  if output=$(command stat -c '%u %a' "$path" 2>/dev/null); then
    :
  elif output=$(command stat -f '%u %Lp' "$path" 2>/dev/null); then
    :
  else
    return 1
  fi
  read -r uid mode <<<"$output"
  [[ $uid == "$EUID" && $mode != *[!0-7]* ]] || return 1
  (((8#$mode & 022) == 0))
}

_dot_shdeps_origin_allowed() {
  case $1 in
    https://github.com/cgraf78/shdeps | \
      https://github.com/cgraf78/shdeps.git | \
      git@github.com:cgraf78/shdeps | \
      git@github.com:cgraf78/shdeps.git | \
      ssh://git@github.com/cgraf78/shdeps | \
      ssh://git@github.com/cgraf78/shdeps.git)
      return 0
      ;;
    *) return 1 ;;
  esac
}

_dot_shdeps_development_checkout_valid() {
  local checkout=$1 physical root git_dir common_dir
  local -a origins=() effective_origins=()

  # Latest mode is an explicit developer-checkout trust decision. These checks
  # bind that decision to the expected user-owned root, bootstrap entrypoints,
  # Git metadata, and official origin; they do not recursively sandbox source,
  # prebuilt binaries, or Cargo inputs inside the selected checkout.
  [[ -d $checkout && ! -L $checkout ]] || return 1
  _dot_shdeps_path_owned "$checkout" || return 1
  [[ -f $checkout/install.sh && ! -L $checkout/install.sh ]] || return 1
  _dot_shdeps_path_owned "$checkout/install.sh" || return 1
  [[ -f $checkout/shdeps.sh && ! -L $checkout/shdeps.sh ]] || return 1
  _dot_shdeps_path_owned "$checkout/shdeps.sh" || return 1
  [[ (-d $checkout/.git || -f $checkout/.git) && ! -L $checkout/.git ]] || return 1
  _dot_shdeps_path_owned "$checkout/.git" || return 1

  physical=$(cd -P -- "$checkout" 2>/dev/null && pwd -P) || return 1
  root=$(_dot_sanitized_git -C "$physical" \
    rev-parse --show-toplevel 2>/dev/null) || return 1
  root=$(cd -P -- "$root" 2>/dev/null && pwd -P) || return 1
  [[ $root == "$physical" ]] || return 1
  git_dir=$(_dot_sanitized_git -C "$physical" \
    rev-parse --absolute-git-dir 2>/dev/null) || return 1
  git_dir=$(cd -P -- "$git_dir" 2>/dev/null && pwd -P) || return 1
  _dot_shdeps_path_owned "$git_dir" || return 1
  common_dir=$(_dot_sanitized_git -C "$physical" \
    rev-parse --path-format=absolute --git-common-dir 2>/dev/null) || return 1
  common_dir=$(cd -P -- "$common_dir" 2>/dev/null && pwd -P) || return 1
  _dot_shdeps_path_owned "$common_dir" || return 1

  mapfile -t origins < <(_dot_sanitized_git -C "$physical" \
    config --local --get-all remote.origin.url 2>/dev/null)
  [[ ${#origins[@]} -eq 1 ]] || return 1
  _dot_shdeps_origin_allowed "${origins[0]}" || return 1
  mapfile -t effective_origins < <(_dot_sanitized_git -C "$physical" \
    remote get-url --all origin 2>/dev/null)
  [[ ${#effective_origins[@]} -eq 1 ]] || return 1
  _dot_shdeps_origin_allowed "${effective_origins[0]}"
}

_dot_shdeps_installer() {
  local installed=${SHDEPS_DIR:-$HOME/.local/share/shdeps}
  local development=$SHDEPS_GIT_DEV_DIR/shdeps
  local development_revision expected_revision

  _DOT_SHDEPS_INSTALLER_SOURCE=unavailable

  if [[ -n ${SHDEPS_LIB:-} && -f ${SHDEPS_LIB%/*}/install.sh ]] &&
    _dot_shdeps_installer_hash_matches "${SHDEPS_LIB%/*}/install.sh"; then
    REPLY=${SHDEPS_LIB%/*}/install.sh
    _DOT_SHDEPS_INSTALLER_SOURCE=explicit
    return 0
  fi
  development_revision=$(_dot_sanitized_git -C "$development" \
    rev-parse HEAD 2>/dev/null || true)
  expected_revision=$(_dot_shdeps_lock_value revision) || expected_revision=''
  if [[ -f "$development/install.sh" &&
    -f "$development/shdeps.sh" &&
    -n "$expected_revision" && "$development_revision" == "$expected_revision" ]] &&
    _dot_shdeps_installer_hash_matches "$development/install.sh"; then
    SHDEPS_LIB=$development/shdeps.sh
    export SHDEPS_LIB
    REPLY=$development/install.sh
    _DOT_SHDEPS_INSTALLER_SOURCE=pinned-dev
    return 0
  fi
  if [[ "${DOT_SHDEPS_UPDATE_POLICY:-pinned}" == latest ]] &&
    _dot_shdeps_development_checkout_valid "$development"; then
    SHDEPS_LIB=$development/shdeps.sh
    export SHDEPS_LIB
    REPLY=$development/install.sh
    _DOT_SHDEPS_INSTALLER_SOURCE=latest-dev
    return 0
  fi
  if [[ -f "$installed/install.sh" && -f "$installed/shdeps.sh" ]] &&
    _dot_shdeps_installer_hash_matches "$installed/install.sh"; then
    REPLY=$installed/install.sh
    _DOT_SHDEPS_INSTALLER_SOURCE=managed
    return 0
  fi
  return 1
}

_dot_shdeps_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

_dot_shdeps_installer_hash_matches() {
  local expected actual
  expected=$(_dot_shdeps_lock_value install_sha256) || return 1
  [[ $expected =~ ^[0-9a-f]{64}$ ]] || return 1
  actual=$(_dot_shdeps_sha256 "$1" 2>/dev/null) || return 1
  [[ $actual == "$expected" ]]
}

_dot_shdeps_run_bounded() (
  local timeout_seconds=$1 label=$2 stderr_mode=$3
  local child deadline rc=1 tmpdir output_file status_file
  shift 3

  [[ $timeout_seconds =~ ^[1-9][0-9]*$ && -n $label && $# -gt 0 ]] || return 2
  case $stderr_mode in
    inherit-stderr | discard-stderr) ;;
    *) return 2 ;;
  esac

  # This synchronous supervisor must own a distinct group even when called
  # from a larger worker. Its wrapper remains the group leader after the real
  # command exits, so an inherited-output descendant cannot escape the bound.
  unset DOT_CLEANUP_INHERIT_GROUP
  _dot_cleanup_prepare_subshell
  _dot_cleanup_mktemp -d || return 1
  tmpdir=$REPLY
  output_file=$tmpdir/output
  status_file=$tmpdir/status

  _dot_cleanup_begin_job_launch closed-stdin
  if [[ $DOT_CLEANUP_LAUNCH_ISOLATED -ne 1 ]]; then
    _dot_cleanup_end_registration
    _dot_cleanup_remove_path "$tmpdir" || true
    return 1
  fi
  (
    local command_status=0
    _dot_cleanup_prepare_subshell
    if [[ $stderr_mode == discard-stderr ]]; then
      if "$@" >"$output_file" 2>/dev/null; then
        command_status=0
      else
        command_status=$?
      fi
    elif "$@" >"$output_file"; then
      command_status=0
    else
      command_status=$?
    fi
    printf '%s\n' "$command_status" >"$status_file"
    while :; do
      sleep 3600 || true
    done
  ) <&"$DOT_CLEANUP_LAUNCH_STDIN_FD" &
  child=$!
  _dot_cleanup_finish_job_launch "$child"

  deadline=$((SECONDS + timeout_seconds))
  while [[ ! -s $status_file ]]; do
    if ((SECONDS >= deadline)); then
      printf '  warning: Shdeps %s timed out after %ss\n' \
        "$label" "$timeout_seconds" >&2
      _dot_cleanup_all
      return 124
    fi
    _dot_cleanup_job_matches "$child" active || {
      _dot_cleanup_all
      return 1
    }
    sleep 0.05 || true
  done

  rc=$(<"$status_file")
  [[ $rc =~ ^([0-9]|[1-9][0-9]{1,2})$ && $rc -le 255 ]] || {
    _dot_cleanup_all
    return 1
  }
  _dot_cleanup_group_job_active "$child" "$child" || {
    _dot_cleanup_all
    return 1
  }
  kill -KILL -- "-$child" 2>/dev/null || {
    _dot_cleanup_all
    return 1
  }
  wait "$child" 2>/dev/null || true
  _dot_cleanup_unregister_pid "$child"
  command cat "$output_file" || {
    _dot_cleanup_remove_path "$tmpdir" || true
    return 1
  }
  _dot_cleanup_remove_path "$tmpdir" || return 1
  return "$rc"
)

_dot_shdeps_download_installer() {
  local revision temporary url attempt curl_status=1
  local retry_delay=${_DOT_SHDEPS_DOWNLOAD_RETRY_DELAY_SECONDS:-1}

  revision=$(_dot_shdeps_lock_value revision) || return 1
  [[ "$revision" =~ ^[0-9a-f]{40}$ ]] || return 1
  [[ $retry_delay =~ ^[0-9]+$ ]] || retry_delay=1
  _dot_cleanup_mktemp || return 1
  temporary=$REPLY
  url=https://raw.githubusercontent.com/cgraf78/shdeps/$revision/install.sh
  for attempt in 1 2 3; do
    if curl --connect-timeout 10 --max-time 30 \
      --speed-limit 1024 --speed-time 15 -fsSL "$url" -o "$temporary"; then
      curl_status=0
      break
    else
      curl_status=$?
    fi
    [[ $attempt -eq 3 ]] || sleep "$retry_delay"
  done
  if [[ $curl_status -ne 0 ]]; then
    _dot_cleanup_remove_path "$temporary" || true
    _warn '  warning: Shdeps bootstrap download failed'
    return 1
  fi
  if ! _dot_shdeps_installer_hash_matches "$temporary"; then
    _dot_cleanup_remove_path "$temporary" || true
    _warn '  warning: downloaded Shdeps bootstrap did not match the release digest'
    return 1
  fi
  chmod 0700 "$temporary" || return 1
  REPLY=$temporary
}

_dot_shdeps_binary_abi_version() {
  local binary=$1 timeout_seconds=${_DOT_SHDEPS_ABI_TIMEOUT_SECONDS:-10}

  [[ $timeout_seconds =~ ^[1-9][0-9]*$ ]] || timeout_seconds=10
  [[ -n $binary && -x $binary ]] || return 1
  if ! REPLY=$(_dot_shdeps_run_bounded "$timeout_seconds" \
    'provider ABI probe' discard-stderr "$binary" __api version); then
    REPLY=''
    return 1
  fi
}

_dot_shdeps_binary_abi() {
  local binary=${_SHDEPSW_BIN:-} expected

  expected=$(_dot_shdeps_lock_value abi) || return 1
  _dot_shdeps_binary_abi_version "$binary" || return 1
  [[ $REPLY == "abi:$expected" ]]
}

_ensure_shdeps() {
  local installer temporary=false development bootstrap_status=0
  local bootstrap_force_set bootstrap_force

  [[ "${DOT_DEPENDENCY_PROVIDER:-none}" == shdeps ]] || return 0
  _dot_shdeps_configure_env || return 1
  development=$SHDEPS_GIT_DEV_DIR/shdeps
  if _dot_shdeps_installer; then
    installer=$REPLY
  else
    _dot_shdeps_download_installer || {
      _warn '  warning: failed to fetch the reviewed Shdeps bootstrap'
      return 1
    }
    installer=$REPLY
    temporary=true
  fi

  # A pinned or latest-policy validation failure must not be bypassed by the
  # downloaded/installed bootstrap rediscovering the rejected checkout on its
  # own. The selected development installer remains visible; every other
  # source gets an explicitly empty development root for this sourced call.
  if [[ $installer != "$development/install.sh" ]]; then
    local SHDEPS_GIT_DEV_DIR=/dev/null
    export SHDEPS_GIT_DEV_DIR
  fi

  # Latest governs provider freshness, not dependency convergence. Use the
  # installer-only control so the sourced Shdeps API never snapshots a global
  # dependency force request.
  bootstrap_force_set=${SHDEPS_BOOTSTRAP_FORCE+x}
  bootstrap_force=${SHDEPS_BOOTSTRAP_FORCE-}
  if [[ "${DOT_SHDEPS_UPDATE_POLICY:-pinned}" == latest ]]; then
    export SHDEPS_BOOTSTRAP_FORCE=1
  fi

  # shellcheck source=/dev/null
  if . "$installer" --bootstrap; then
    :
  else
    bootstrap_status=$?
  fi
  if [[ $bootstrap_force_set == x ]]; then
    SHDEPS_BOOTSTRAP_FORCE=$bootstrap_force
    export SHDEPS_BOOTSTRAP_FORCE
  else
    unset SHDEPS_BOOTSTRAP_FORCE
  fi
  if [[ $bootstrap_status -ne 0 ]]; then
    [[ "$temporary" == false ]] || _dot_cleanup_remove_path "$installer" || true
    return 1
  fi
  [[ "$temporary" == false ]] || _dot_cleanup_remove_path "$installer" || true
  _dot_shdeps_binary_abi
}

_dot_active_revision() {
  _dot_source_git rev-parse HEAD 2>/dev/null || true
}

_dot_reexec_checkpoint_path() {
  dot_xdg_path state dot/provider-reexec-failed
}

_dot_provider_revision_valid() {
  [[ ${1:-} =~ ^[0-9a-fA-F]{40,64}$ ]]
}

# Parse the durable one-generation guard record without sourcing it. The file
# is recovery authority: accept only the exact schema written below, keep it
# private and singly linked, and leave malformed evidence untouched.
_dot_provider_read_checkpoint() {
  local path=$1 output uid mode links size line count=0
  local before='' after='' seen_before=0 seen_after=0

  [[ -f $path && ! -L $path && -O $path ]] || return 1
  if output=$(stat -c '%u %a %h %s' "$path" 2>/dev/null); then
    :
  elif output=$(stat -f '%u %Lp %l %z' "$path" 2>/dev/null); then
    :
  else
    return 1
  fi
  read -r uid mode links size <<<"$output"
  [[ $uid == "$(id -u)" && $mode != *[!0-7]* && $mode == 600 &&
  $links == 1 && $size =~ ^[0-9]+$ && $size -le 512 ]] || return 1

  while IFS= read -r line || [[ -n $line ]]; do
    count=$((count + 1))
    case $count:$line in
      '1:cgraf78 dot provider reexec checkpoint v1') ;;
      2:before=*)
        [[ $seen_before -eq 0 ]] || return 1
        before=${line#before=}
        seen_before=1
        ;;
      3:after=*)
        [[ $seen_after -eq 0 ]] || return 1
        after=${line#after=}
        seen_after=1
        ;;
      *) return 1 ;;
    esac
  done <"$path"
  [[ $count -eq 3 && $seen_before -eq 1 && $seen_after -eq 1 ]] || return 1
  _dot_provider_revision_valid "$before" || return 1
  _dot_provider_revision_valid "$after" || return 1
  [[ $before != "$after" ]] || return 1
  DOT_PROVIDER_CHECKPOINT_AFTER=${after,,}
}

# Consume a checkpoint only after binding it to the currently executing dot
# checkout. A mismatch means the user must inspect or repair provider state;
# silently deleting it would discard the only explanation for the guarded stop.
_dot_provider_consume_checkpoint() {
  local path identity active

  _dot_reexec_checkpoint_path || return 1
  path=$REPLY
  [[ -e $path || -L $path ]] || return 0
  identity=$(_dot_path_identity "$path" 2>/dev/null) || {
    _warn "  warning: provider re-exec checkpoint is unsafe: $path"
    return 1
  }
  if ! _dot_provider_read_checkpoint "$path"; then
    _warn "  warning: provider re-exec checkpoint is malformed: $path"
    return 1
  fi
  active=$(_dot_active_revision)
  if ! _dot_provider_revision_valid "$active" ||
    [[ ${active,,} != "$DOT_PROVIDER_CHECKPOINT_AFTER" ]]; then
    _warn "  warning: provider re-exec checkpoint does not match the active dot revision: $path"
    return 1
  fi
  [[ $(_dot_path_identity "$path" 2>/dev/null || true) == "$identity" ]] || {
    _warn "  warning: provider re-exec checkpoint changed during validation: $path"
    return 1
  }
  rm "$path"
}

_dot_provider_write_checkpoint() {
  local before=$1 after=$2 path temporary
  _dot_provider_revision_valid "$before" || return 1
  _dot_provider_revision_valid "$after" || return 1
  before=${before,,}
  after=${after,,}
  [[ $before != "$after" ]] || return 1
  _dot_reexec_checkpoint_path || return 1
  path=$REPLY
  mkdir -p "${path%/*}" || return 1
  chmod 0700 "${path%/*}" 2>/dev/null || true
  [[ ! -e $path && ! -L $path ]] || return 1
  _dot_sibling_tmp_for "$path" || return 1
  temporary=$REPLY
  {
    printf 'cgraf78 dot provider reexec checkpoint v1\n'
    printf 'before=%s\n' "$before"
    printf 'after=%s\n' "$after"
  } >"$temporary" || {
    rm -f "$temporary"
    return 1
  }
  chmod 0600 "$temporary" || {
    rm -f "$temporary"
    return 1
  }
  if ! _dot_move_noreplace "$temporary" "$path"; then
    rm -f "$temporary"
    return 1
  fi
}

_dot_provider_maybe_reexec() {
  local before=$1 after interpreter=${BASH:-}
  if ! _dot_provider_revision_valid "$before"; then
    _warn '  warning: active dot revision was invalid before provider update'
    return 1
  fi
  after=$(_dot_active_revision)
  if ! _dot_provider_revision_valid "$after"; then
    _warn '  warning: active dot revision is unavailable after provider update'
    return 1
  fi
  before=${before,,}
  after=${after,,}
  [[ $before != "$after" ]] || return 0

  if [[ ${DOT_REEXEC_ONCE:-0} == 1 ]]; then
    if _dot_provider_write_checkpoint "$before" "$after"; then
      _warn '  warning: dot changed twice during one update; rerun to validate the provider checkpoint'
    else
      _warn '  warning: dot changed twice and its provider checkpoint could not be published'
    fi
    return 1
  fi
  case $interpreter in
    /*) ;;
    *)
      _warn '  warning: cannot re-exec dot with a non-absolute Bash path'
      return 1
      ;;
  esac
  [[ -f $interpreter && -x $interpreter ]] || return 1
  # shellcheck disable=SC2016 # Evaluated by the selected interpreter.
  "$interpreter" --noprofile --norc -c \
    '[[ ${BASH_VERSINFO[0]} -ge 4 ]]' || return 1
  _dot_shdeps_restore_caller_env
  export DOT_REEXEC_ONCE=1 DOT_REEXEC_EXPECTED_REVISION=$after
  exec "$interpreter" "$DOT_SOURCE_ROOT/lib/dot/main.sh" \
    "${DOT_ORIGINAL_ARGV[@]}"
}
