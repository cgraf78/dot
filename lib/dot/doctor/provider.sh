# shellcheck shell=bash
# Read-only health checks for the optional dependency-provider boundary.

_dr_shdeps_binary() {
  local installer=$1 root candidate

  if [[ -n ${_SHDEPSW_BIN:-} && -x ${_SHDEPSW_BIN:-} ]]; then
    REPLY=$_SHDEPSW_BIN
    return 0
  fi
  root=${installer%/*}
  for candidate in \
    "$root/shdeps" \
    "$root/target/debug/shdeps" \
    "$root/target/release/shdeps"; do
    if [[ -f $candidate && ! -L $candidate && -x $candidate ]]; then
      REPLY=$candidate
      return 0
    fi
  done
  return 1
}

_dr_check_provider() {
  local installer binary expected actual policy development
  local locked_revision development_revision development_invalid=false

  _dr_section 'Dependency provider'
  case ${DOT_DEPENDENCY_PROVIDER:-none} in
    none)
      _dr_skip 'no dependency provider configured'
      return 0
      ;;
    shdeps) ;;
    *)
      _dr_fail 'dependency provider is unsupported' "${DOT_DEPENDENCY_PROVIDER:-<missing>}"
      return 0
      ;;
  esac

  policy=${DOT_SHDEPS_UPDATE_POLICY:-pinned}
  if ! _dot_shdeps_configure_env; then
    _dr_ok 'Shdeps update policy' "$policy"
    _dr_fail 'Shdeps provider is unavailable' \
      'run dot update to bootstrap the reviewed provider release'
    return 0
  fi
  _dr_ok 'Shdeps update policy' "$policy"
  development=$SHDEPS_GIT_DEV_DIR/shdeps
  if [[ $policy == latest && (-e $development || -L $development) ]] &&
    ! _dot_shdeps_development_checkout_valid "$development"; then
    development_invalid=true
  fi
  if ! _dot_shdeps_installer; then
    if [[ $development_invalid == true ]]; then
      _dr_warn 'Shdeps development checkout ignored' \
        "verify its owner, modes, Git root, and cgraf78/shdeps origin: $(_dr_tilde "$development")"
    fi
    _dr_fail 'Shdeps provider is unavailable' \
      'run dot update to bootstrap the reviewed provider release'
    return 0
  fi
  installer=$REPLY
  if [[ $development_invalid == true &&
    ${_DOT_SHDEPS_INSTALLER_SOURCE:-} == managed ]]; then
    _dr_warn 'Shdeps development checkout ignored' \
      "verify its owner, modes, Git root, and cgraf78/shdeps origin: $(_dr_tilde "$development")"
  fi
  case ${_DOT_SHDEPS_INSTALLER_SOURCE:-unavailable} in
    explicit)
      _dr_ok 'Shdeps provider source' \
        "caller-selected reviewed installer: $(_dr_tilde "$installer")"
      _dr_ok 'Shdeps installer is reviewed' "$(_dr_tilde "$installer")"
      ;;
    pinned-dev)
      if [[ $policy == latest ]]; then
        _dr_ok 'Shdeps provider source' \
          "development checkout selected by Dot lock: $(_dr_tilde "$development")"
      fi
      _dr_ok 'Shdeps installer is reviewed' "$(_dr_tilde "$installer")"
      ;;
    latest-dev)
      _dr_ok 'Shdeps provider source' \
        "trusted development checkout: $(_dr_tilde "$development")"
      ;;
    managed)
      if [[ $policy == latest ]]; then
        _dr_ok 'Shdeps provider source' \
          'managed release via reviewed bootstrap'
      fi
      _dr_ok 'Shdeps installer is reviewed' "$(_dr_tilde "$installer")"
      ;;
    *)
      _dr_fail 'Shdeps provider source is unavailable' \
        'run dot update to restore provider selection metadata'
      return 0
      ;;
  esac

  if [[ $policy == latest &&
    (${_DOT_SHDEPS_INSTALLER_SOURCE:-} == pinned-dev ||
    ${_DOT_SHDEPS_INSTALLER_SOURCE:-} == latest-dev) ]]; then
    locked_revision=$(_dot_shdeps_lock_value revision 2>/dev/null || true)
    development_revision=$(_dot_sanitized_git -C "$development" \
      rev-parse HEAD 2>/dev/null || true)
    if [[ -n $locked_revision && $development_revision == "$locked_revision" ]]; then
      _dr_ok 'Shdeps development revision' \
        "matches Dot lock: ${development_revision:0:12}"
    else
      _dr_ok 'Shdeps development revision' \
        "trusted unpinned revision differs from Dot lock; accepted by latest policy: ${development_revision:-<unavailable>}"
    fi
  fi

  if ! _dr_shdeps_binary "$installer"; then
    _dr_fail 'Shdeps provider binary is unavailable' \
      'run dot update to complete provider installation'
    return 0
  fi
  binary=$REPLY
  expected=$(_dot_shdeps_lock_value abi 2>/dev/null || true)
  actual=$(command "$binary" __api version 2>/dev/null || true)
  if [[ -n $expected && $actual == "abi:$expected" ]]; then
    _dr_ok 'Shdeps provider ABI' "$actual"
  else
    _dr_fail 'Shdeps provider ABI mismatch' \
      "expected abi:${expected:-<missing>}, found ${actual:-<unavailable>}"
  fi
}
