# shellcheck shell=bash
# Core extension discovery health checks.

_dr_check_merges() {
  local root count=0

  _dr_section 'Extensions'
  if [[ ${DOT_EXTENSION_API:-} != 1 || -z ${DOT_EXTENSIONS_DIR:-} ]]; then
    _dr_skip 'no extension root configured'
    return 0
  fi
  root=$DOT_EXTENSIONS_DIR/merge-hooks.d
  if [[ ! -e $root && ! -L $root ]]; then
    _dr_skip 'merge-hook extensions' 'none configured'
    return 0
  fi
  if [[ ! -d $root || -L $root ]]; then
    _dr_fail 'merge-hook extension directory is unavailable' "$root"
    return 0
  fi
  if ! count=$(_merge_hook_specs | wc -l | tr -d ' '); then
    _dr_fail 'merge-hook extension inventory is invalid' "$root"
    return 0
  fi
  if [[ $count -gt 0 ]]; then
    _dr_ok 'merge-hook extensions' "$count hook(s)"
  else
    _dr_skip 'merge-hook extensions' 'none configured'
  fi
}
