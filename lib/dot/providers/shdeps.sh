# shellcheck shell=bash
# Optional Shdeps provider bootstrap pinned by the active dot release.

_dot_shdeps_lock_value() {
  local key=$1 line
  while IFS= read -r line || [[ -n "$line" ]]; do
    [[ "$line" == "$key="* ]] || continue
    printf '%s\n' "${line#*=}"
    return 0
  done <"$DOT_SOURCE_ROOT/support/shdeps.lock"
  return 1
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
  [[ "${DOT_FORCE:-0}" -eq 1 ]] && export SHDEPS_FORCE=1
  [[ "${DOT_QUIET:-0}" -eq 1 ]] && export SHDEPS_QUIET=1
  # Optional flag propagation must not become the function's result. Provider
  # setup is successful when the roots above resolved, including the ordinary
  # non-force, non-quiet path.
  return 0
}

_dot_shdeps_installer() {
  local installed=${SHDEPS_DIR:-$HOME/.local/share/shdeps}
  local development_revision expected_revision

  if [[ -n ${SHDEPS_LIB:-} && -f ${SHDEPS_LIB%/*}/install.sh ]] &&
    _dot_shdeps_installer_hash_matches "${SHDEPS_LIB%/*}/install.sh"; then
    REPLY=${SHDEPS_LIB%/*}/install.sh
    return 0
  fi
  development_revision=$(git -C "$SHDEPS_GIT_DEV_DIR/shdeps" rev-parse HEAD 2>/dev/null || true)
  expected_revision=$(_dot_shdeps_lock_value revision) || expected_revision=''
  if [[ -f "$SHDEPS_GIT_DEV_DIR/shdeps/install.sh" &&
    -f "$SHDEPS_GIT_DEV_DIR/shdeps/shdeps.sh" &&
    -n "$expected_revision" && "$development_revision" == "$expected_revision" ]] &&
    _dot_shdeps_installer_hash_matches "$SHDEPS_GIT_DEV_DIR/shdeps/install.sh"; then
    SHDEPS_LIB=$SHDEPS_GIT_DEV_DIR/shdeps/shdeps.sh
    export SHDEPS_LIB
    REPLY=$SHDEPS_GIT_DEV_DIR/shdeps/install.sh
    return 0
  fi
  if [[ -f "$installed/install.sh" && -f "$installed/shdeps.sh" ]] &&
    _dot_shdeps_installer_hash_matches "$installed/install.sh"; then
    REPLY=$installed/install.sh
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

_dot_shdeps_download_installer() {
  local revision temporary url

  revision=$(_dot_shdeps_lock_value revision) || return 1
  [[ "$revision" =~ ^[0-9a-f]{40}$ ]] || return 1
  _dot_cleanup_mktemp || return 1
  temporary=$REPLY
  url=https://raw.githubusercontent.com/cgraf78/shdeps/$revision/install.sh
  if ! curl -fsSL "$url" -o "$temporary"; then
    _dot_cleanup_remove_path "$temporary" || true
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

_dot_shdeps_binary_abi() {
  local binary=${_SHDEPSW_BIN:-} expected

  expected=$(_dot_shdeps_lock_value abi) || return 1
  [[ -n "$binary" && -x "$binary" ]] || return 1
  [[ "$(command "$binary" __api version 2>/dev/null)" == "abi:$expected" ]]
}

_ensure_shdeps() {
  local installer temporary=false

  [[ "${DOT_DEPENDENCY_PROVIDER:-none}" == shdeps ]] || return 0
  _dot_shdeps_configure_env || return 1
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

  # shellcheck source=/dev/null
  if ! . "$installer" --bootstrap; then
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
  export DOT_REEXEC_ONCE=1 DOT_REEXEC_EXPECTED_REVISION=$after
  exec "$interpreter" "$DOT_SOURCE_ROOT/lib/dot/main.sh" \
    "${DOT_ORIGINAL_ARGV[@]}"
}
