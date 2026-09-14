# shellcheck shell=bash
# Client preparation extensions that must complete before repository network or
# checkout mutation. This lifecycle is intentionally generic; clients own the
# concrete transport, credential, or environment policy they prepare.

_dot_pre_sync_specs() {
  local root=${DOT_EXTENSIONS_DIR:-}/pre-sync.d script key identity
  local nullglob_was_set=0
  local -A seen=()

  [[ -n ${DOT_EXTENSIONS_DIR:-} ]] || return 0
  if [[ ! -e $root && ! -L $root ]]; then
    return 0
  fi
  _dot_extension_directory_validate "$root" || return 1

  shopt -q nullglob && nullglob_was_set=1
  shopt -s nullglob
  for script in "$root"/*.sh; do
    _dot_extension_file_validate "$script" || return 1
    key=${script##*/}
    key=${key%.sh}
    key=${key%.serial}
    identity=$key
    [[ $identity =~ ^([0-9]+[-_])?([a-z][a-z0-9-]*)$ ]] || {
      printf 'dot: invalid pre-sync extension identity: %s\n' "${script##*/}" >&2
      return 1
    }
    identity=${BASH_REMATCH[2]}
    [[ -z ${seen[$identity]+x} ]] || {
      printf 'dot: duplicate pre-sync extension identity: %s\n' "$identity" >&2
      return 1
    }
    seen[$identity]=1
    printf '%s\t%s\n' "$key" "$script"
  done
  [[ $nullglob_was_set -eq 1 ]] || shopt -u nullglob
}

_run_pre_sync_extensions() {
  local stage=${1:-} specs key script temporary result status=0 context token
  shift || true
  local -a records=("$@")

  case $stage in prepare | reconcile) ;; *) return 2 ;; esac

  specs=$(_dot_pre_sync_specs) || return 1
  [[ -n $specs ]] || return 0
  while IFS=$'\t' read -r key script; do
    [[ -n $key && -n $script ]] || return 1
    _dot_cleanup_mktemp -d || return 1
    temporary=$REPLY
    result=$temporary/result
    : >"$result" || return 1
    chmod 0600 "$result" || return 1
    _dot_overlay_context_create "$temporary" pre-sync eligible "$stage" \
      "${records[@]+"${records[@]}"}" || return 1
    context=$REPLY_PATH
    token=$REPLY_TOKEN
    if ! _dot_extension_worker_run pre-sync "$script" "$temporary" "$result" \
      "$context" "$token"; then
      _warn "  warning: pre-sync extension failed: ${script##*/}"
      status=1
      _dot_cleanup_remove_path "$temporary" || true
      break
    fi
    _dot_cleanup_remove_path "$temporary" || return 1
  done <<<"$specs"
  return "$status"
}
