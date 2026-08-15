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
  local installer binary expected actual

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

  if ! _dot_shdeps_configure_env || ! _dot_shdeps_installer; then
    _dr_fail 'Shdeps provider is unavailable' \
      'run dot update to bootstrap the reviewed provider release'
    return 0
  fi
  installer=$REPLY
  _dr_ok 'Shdeps installer is reviewed' "$(_dr_tilde "$installer")"

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
