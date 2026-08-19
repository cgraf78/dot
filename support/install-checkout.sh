#!/usr/bin/env bash
# Publish the command and public library as links to this one checkout. Link
# creation is monotonic: an already-correct link is kept, an absent path is
# filled through a deterministic sibling stage, and no live path is removed.
# That ordering makes an interrupted two-link install resumable without a
# rollback window that could temporarily remove the command.

set -euo pipefail
CDPATH=

PREFIX=${PREFIX:-$HOME/.local}
BIN_DIR=${BIN_DIR:-$PREFIX/bin}
DOT_PUBLIC_LIB=${DOT_PUBLIC_LIB:-$PREFIX/lib/dot}
ROOT=$(cd -P -- "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
LINK_ROOT=$ROOT
CANONICAL_ROOT=${CGRAF78_CHECKOUT_INSTALL_DIR:-}
if [[ -z "$CANONICAL_ROOT" ]]; then
  install_home=${SHDEPS_INSTALL_DIR:-$HOME/.local/share}
  while [[ "$install_home" != / && "$install_home" == */ ]]; do
    install_home=${install_home%/}
  done
  case $install_home in
    /) CANONICAL_ROOT=/cgraf78/dot ;;
    /*) CANONICAL_ROOT=$install_home/cgraf78/dot ;;
    *) CANONICAL_ROOT= ;;
  esac
fi
case $CANONICAL_ROOT in
  '' | / | */ | *//* | */./* | */. | */../* | */.. | *$'\n'* | *$'\r'*)
    printf 'dot: canonical checkout root must be normalized and absolute\n' >&2
    exit 1
    ;;
  /*) ;;
  *)
    printf 'dot: canonical checkout root must be absolute\n' >&2
    exit 1
    ;;
esac
if [[ -d "$CANONICAL_ROOT" ]] &&
  canonical_physical=$(cd -P -- "$CANONICAL_ROOT" && pwd -P) &&
  [[ "$canonical_physical" == "$ROOT" ]]; then
  # Link through the stable manager-owned root. Its target may switch between
  # this physical development checkout and a managed directory without
  # changing either public entry point.
  LINK_ROOT=$CANONICAL_ROOT
fi
COMMAND_PROVIDER=$ROOT/bin/dot
LIBRARY_PROVIDER=$ROOT/lib/dot/public
COMMAND_SOURCE=$LINK_ROOT/bin/dot
LIBRARY_SOURCE=$LINK_ROOT/lib/dot/public
COMMAND_TARGET=$BIN_DIR/dot
LIBRARY_TARGET=$DOT_PUBLIC_LIB
CLIENT_ADAPTER=$ROOT/support/client-launcher.sh

for source in "$COMMAND_PROVIDER" "$CLIENT_ADAPTER"; do
  [[ -f "$source" && ! -L "$source" && -x "$source" ]] || {
    printf 'dot: required executable is missing: %s\n' "$source" >&2
    exit 1
  }
done
[[ -d "$LIBRARY_PROVIDER" && ! -L "$LIBRARY_PROVIDER" ]] || {
  printf 'dot: public library is missing: %s\n' "$LIBRARY_PROVIDER" >&2
  exit 1
}

dot_install_exact_link() {
  local path=$1 expected=$2 actual

  [[ -L "$path" ]] || return 1
  actual=$(readlink "$path") || return 1
  [[ "$actual" == "$expected" ]]
}

dot_install_exact_file() {
  local expected=$1 actual=$2

  # Git is already required to obtain and run this checkout, while small
  # bootstrap images may omit coreutils `cmp`. Disable both external diff and
  # text conversion so the comparison is over the two files' literal bytes and
  # cannot delegate to client configuration.
  command git --no-pager diff --no-index --quiet --no-ext-diff --no-textconv \
    -- "$expected" "$actual"
}

dot_install_remove_exact_link() {
  local path=$1 expected=$2

  [[ -e "$path" || -L "$path" ]] || return 0
  dot_install_exact_link "$path" "$expected" || {
    printf 'dot: refusing foreign installer stage: %s\n' "$path" >&2
    return 1
  }
  rm -f -- "$path"
}

dot_install_recover_nested_stage() {
  local container=$1 stage=$2 source=$3 nested

  dot_install_exact_link "$container" "$source" && return 0
  [[ -d "$container" && ! -L "$container" ]] || return 0
  nested=$container/${stage##*/}
  [[ -e "$nested" || -L "$nested" ]] || return 0
  dot_install_remove_exact_link "$nested" "$source"
}

dot_install_recover_stage_creation() {
  local stage=$1 source=$2 nested

  dot_install_exact_link "$stage" "$source" && return 0
  [[ -d "$stage" && ! -L "$stage" ]] || return 0
  nested=$stage/${source##*/}
  [[ -e "$nested" || -L "$nested" ]] || return 0
  dot_install_remove_exact_link "$nested" "$source"
}

dot_install_detect_link_tools() {
  local probe source moved ln_bin mv_bin

  ln_bin=$(type -P ln 2>/dev/null) || return 1
  mv_bin=$(type -P mv 2>/dev/null) || return 1
  probe=$(mktemp -d "${TMPDIR:-/tmp}/dot-link-tools.XXXXXX") || return 1
  source=$probe/source
  moved=$probe/moved

  # Select behavior from the resolved command, not the kernel name. macOS
  # users commonly put GNU coreutils ahead of BSD /bin, while Termux uses GNU
  # semantics on an Android kernel. The isolated probes keep that distinction
  # out of every live destination.
  if "$ln_bin" -sT -- probe-source "$probe/link" 2>/dev/null &&
    [[ -L "$probe/link" && $(readlink "$probe/link") == probe-source ]]; then
    DOT_INSTALL_LN_MODE=T
  else
    rm -f -- "$probe/link"
    if "$ln_bin" -sh probe-source "$probe/link" 2>/dev/null &&
      [[ -L "$probe/link" && $(readlink "$probe/link") == probe-source ]]; then
      DOT_INSTALL_LN_MODE=h
    else
      rm -f -- "$probe/link"
      rmdir "$probe" 2>/dev/null || true
      printf 'dot: ln lacks exact-destination link publication\n' >&2
      return 1
    fi
  fi
  rm -f -- "$probe/link"

  printf 'probe\n' >"$source"
  if "$mv_bin" -nT -- "$source" "$moved" 2>/dev/null &&
    [[ -f "$moved" && ! -e "$source" ]]; then
    DOT_INSTALL_MV_MODE=T
  else
    rm -f -- "$source" "$moved"
    printf 'probe\n' >"$source"
    if "$mv_bin" -nh "$source" "$moved" 2>/dev/null &&
      [[ -f "$moved" && ! -e "$source" ]]; then
      DOT_INSTALL_MV_MODE=h
    else
      rm -f -- "$source" "$moved"
      rmdir "$probe" 2>/dev/null || true
      printf 'dot: mv lacks exact-destination no-clobber publication\n' >&2
      return 1
    fi
  fi
  rm -f -- "$source" "$moved"
  rmdir "$probe" || return 1
  DOT_INSTALL_LN_BIN=$ln_bin
  DOT_INSTALL_MV_BIN=$mv_bin
}

dot_install_create_stage() {
  local source=$1 stage=$2

  if [[ $DOT_INSTALL_LN_MODE == T ]]; then
    "$DOT_INSTALL_LN_BIN" -sT -- "$source" "$stage"
  else
    "$DOT_INSTALL_LN_BIN" -sh "$source" "$stage"
  fi
}

dot_install_move_stage() {
  local stage=$1 target=$2

  if [[ $DOT_INSTALL_MV_MODE == T ]]; then
    "$DOT_INSTALL_MV_BIN" -nT -- "$stage" "$target"
  else
    # BSD `-h` prevents following a symlink-to-directory. A real directory can
    # still act as a container, so the exact nested-stage recovery remains.
    "$DOT_INSTALL_MV_BIN" -nh "$stage" "$target"
  fi
}

dot_install_recover_link_state() {
  local source=$1 target=$2 parent stage

  parent=${target%/*}
  stage=$parent/.${target##*/}.dot-install-stage-v1
  dot_install_recover_nested_stage "$target" "$stage" "$source" || return
  dot_install_recover_stage_creation "$stage" "$source" || return
  if [[ (-e "$target" || -L "$target") ]] &&
    ! dot_install_exact_link "$target" "$source"; then
    dot_install_remove_exact_link "$stage" "$source"
  fi
}

dot_install_publish_link() {
  local source=$1 target=$2 parent stage status=0

  parent=${target%/*}
  stage=$parent/.${target##*/}.dot-install-stage-v1

  # Portable `ln` and `mv` treat an unexpected directory destination as a
  # container. Recover only the exact deterministic link they can nest there;
  # every foreign entry stays untouched and makes the install fail closed.
  dot_install_recover_nested_stage "$target" "$stage" "$source" || return
  dot_install_recover_stage_creation "$stage" "$source" || return

  if dot_install_exact_link "$target" "$source"; then
    dot_install_remove_exact_link "$stage" "$source"
    return
  fi
  if [[ -e "$target" || -L "$target" ]]; then
    printf 'dot: refusing to replace existing path: %s\n' "$target" >&2
    return 1
  fi

  if [[ -z ${DOT_INSTALL_LN_BIN:-} || -z ${DOT_INSTALL_MV_BIN:-} ]]; then
    dot_install_detect_link_tools || return
  fi

  if [[ -e "$stage" || -L "$stage" ]]; then
    dot_install_exact_link "$stage" "$source" || {
      printf 'dot: refusing foreign installer stage: %s\n' "$stage" >&2
      return 1
    }
  else
    dot_install_create_stage "$source" "$stage" 2>/dev/null || status=$?
    if ! dot_install_exact_link "$stage" "$source"; then
      dot_install_recover_stage_creation "$stage" "$source" || return
      printf 'dot: could not prepare link stage: %s\n' "$stage" >&2
      [[ "$status" -ne 0 ]] || status=1
      return "$status"
    fi
  fi

  status=0
  dot_install_move_stage "$stage" "$target" 2>/dev/null || status=$?
  if dot_install_exact_link "$target" "$source"; then
    dot_install_remove_exact_link "$stage" "$source"
    return 0
  fi

  # A directory winner may contain the stage after portable `mv -n`. Remove
  # only that exact symlink, then preserve the winner and report the collision.
  dot_install_recover_nested_stage "$target" "$stage" "$source" || return
  dot_install_remove_exact_link "$stage" "$source" || return
  printf 'dot: destination appeared before link publication: %s\n' "$target" >&2
  [[ "$status" -ne 0 ]] && return "$status"
  return 1
}

command_adapter=false
dot_install_recover_link_state "$COMMAND_SOURCE" "$COMMAND_TARGET"
dot_install_recover_link_state "$LIBRARY_SOURCE" "$LIBRARY_TARGET"
if [[ -e "$COMMAND_TARGET" || -L "$COMMAND_TARGET" ]]; then
  if dot_install_exact_link "$COMMAND_TARGET" "$COMMAND_SOURCE"; then
    :
  elif [[ -f "$COMMAND_TARGET" && ! -L "$COMMAND_TARGET" &&
    -x "$COMMAND_TARGET" ]] &&
    dot_install_exact_file "$CLIENT_ADAPTER" "$COMMAND_TARGET"; then
    command_adapter=true
  else
    printf 'dot: refusing to replace existing command: %s\n' \
      "$COMMAND_TARGET" >&2
    exit 1
  fi
fi
if [[ -e "$LIBRARY_TARGET" || -L "$LIBRARY_TARGET" ]] &&
  ! dot_install_exact_link "$LIBRARY_TARGET" "$LIBRARY_SOURCE"; then
  printf 'dot: refusing to replace existing library path: %s\n' \
    "$LIBRARY_TARGET" >&2
  exit 1
fi

mkdir -p "$BIN_DIR" "${LIBRARY_TARGET%/*}"
if [[ "$command_adapter" == false ]]; then
  dot_install_publish_link "$COMMAND_SOURCE" "$COMMAND_TARGET"
fi
dot_install_publish_link "$LIBRARY_SOURCE" "$LIBRARY_TARGET"

# Revalidate the final public surface after both monotonic publications. A
# client-owned regular adapter is the one documented exception to link
# ownership and must remain byte-identical.
if [[ "$command_adapter" == true ]]; then
  if [[ ! -f "$COMMAND_TARGET" || -L "$COMMAND_TARGET" ||
    ! -x "$COMMAND_TARGET" ]] ||
    ! dot_install_exact_file "$CLIENT_ADAPTER" "$COMMAND_TARGET"; then
    printf 'dot: client adapter changed during installation: %s\n' \
      "$COMMAND_TARGET" >&2
    exit 1
  fi
else
  dot_install_exact_link "$COMMAND_TARGET" "$COMMAND_SOURCE" || {
    printf 'dot: command link changed during installation: %s\n' \
      "$COMMAND_TARGET" >&2
    exit 1
  }
fi
dot_install_exact_link "$LIBRARY_TARGET" "$LIBRARY_SOURCE" || {
  printf 'dot: library link changed during installation: %s\n' \
    "$LIBRARY_TARGET" >&2
  exit 1
}

if [[ "$command_adapter" == true ]]; then
  printf 'preserved client-owned dot adapter at %s\n' "$COMMAND_TARGET"
else
  printf 'installed dot command at %s\n' "$COMMAND_TARGET"
fi
printf 'installed dot public library at %s\n' "$LIBRARY_TARGET"
