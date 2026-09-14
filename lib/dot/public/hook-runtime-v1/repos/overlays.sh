# shellcheck shell=bash
# Read-only installed-link authority helpers exposed to hook API workers.

_overlay_link_target() {
  local rel=$1 name=$2 rest prefix=
  rest=$rel
  while [[ $rest == */* ]]; do
    rest=${rest#*/}
    prefix=../$prefix
  done
  REPLY=${prefix}.dotfiles-$name/home/$rel
}

_overlay_private_regular_file() {
  local path=$1 mode links
  [[ -f $path && ! -L $path && -O $path ]] || return 1
  mode=$(stat -c '%a' "$path" 2>/dev/null || stat -f '%Lp' "$path" 2>/dev/null) || return 1
  links=$(stat -c '%h' "$path" 2>/dev/null || stat -f '%l' "$path" 2>/dev/null) || return 1
  [[ $mode != *[!0-7]* && $links == 1 ]] || return 1
  (((8#$mode & 077) == 0))
}

_overlay_parse_manifest_record() {
  local line=$1 rel owner target remainder
  [[ $line == *$'\t'* ]] || return 1
  rel=${line%%$'\t'*}
  remainder=${line#*$'\t'}
  if [[ $remainder == *$'\t'* ]]; then
    owner=${remainder%%$'\t'*}
    target=${remainder#*$'\t'}
    [[ $target != *$'\t'* && -n $target ]] || return 1
  else
    owner=$remainder
    _overlay_link_target "$rel" "$owner"
    target=$REPLY
  fi
  case $rel in
    '' | /* | . | .. | ./* | ../* | */./* | */../* | */. | */.. | */ | *//*)
      return 1
      ;;
  esac
  case $owner in
    '' | . | .. | */*) return 1 ;;
  esac
  [[ $target != *$'\r'* && $target != *$'\n'* ]] || return 1
  REPLY_REL=$rel
  REPLY_OWNER=$owner
  REPLY_TARGET=$target
}

_overlay_manifest_safe() {
  local path=$1 line exact_targets=0 links
  # shellcheck disable=SC2034 # Parser publishes these globals for validation.
  local REPLY_REL REPLY_OWNER REPLY_TARGET
  [[ -f $path && ! -L $path && -O $path ]] || return 1
  links=$(stat -c '%h' "$path" 2>/dev/null || stat -f '%l' "$path" 2>/dev/null) || return 1
  [[ $links == 1 ]] || return 1
  while IFS= read -r line || [[ -n $line ]]; do
    _overlay_parse_manifest_record "$line" || return 1
    [[ ${line#*$'\t'} != *$'\t'* ]] || exact_targets=1
  done <"$path"
  [[ $exact_targets -eq 0 ]] || _overlay_private_regular_file "$path"
}
