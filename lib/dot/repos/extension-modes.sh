# shellcheck shell=bash
# Canonicalize the Git-owned portion of the executable extension namespace.
# Git preserves only the executable bit, so a checkout created under umask 0002
# can otherwise publish 0664 files and 0775 directories that the extension
# trust boundary must reject. Limit mutation to tracked paths at or below the
# configured extension root; unrelated HOME paths and sync=none overlays remain
# caller-owned.

_dot_extension_relative_root() {
  local root=${DOT_EXTENSIONS_DIR:-}

  REPLY=
  [[ ${DOT_EXTENSION_API:-} == 1 && -n $root ]] || return 1
  if [[ $HOME == / ]]; then
    case $root in
      /*) REPLY=${root#/} ;;
      *) return 1 ;;
    esac
  else
    case $root in
      "$HOME"/*) REPLY=${root#"$HOME"/} ;;
      *) return 1 ;;
    esac
  fi
  _dot_init_safe_relative_path "$REPLY"
}

_dot_extension_mode_directory() {
  local path=$1 mode

  [[ -d $path && ! -L $path && -O $path ]] || return 1
  chmod 0755 "$path" || return 1
  mode=$(stat -c '%a' "$path" 2>/dev/null || stat -f '%Lp' "$path" 2>/dev/null) ||
    return 1
  [[ $mode == 755 || $mode == 0755 ]]
}

# Normalize ROOT and the existing directory chain leading to RELATIVE. Missing
# live destinations are expected before an overlay is linked, so stop cleanly
# at the first absent component without creating a new path.
_dot_extension_mode_directories() {
  local root=$1 relative=$2 current=$1 component parent
  local -a components=()

  [[ -e $root || -L $root ]] || return 0
  _dot_extension_mode_directory "$root" || return 1
  [[ $relative == */* ]] || return 0
  parent=${relative%/*}
  IFS=/ read -r -a components <<<"$parent"
  for component in "${components[@]}"; do
    current=$current/$component
    [[ -e $current || -L $current ]] || return 0
    _dot_extension_mode_directory "$current" || return 1
  done
}

_dot_extension_mode_repo() {
  local kind=$1 name=$2 path=$3 url=$4 extension_relative=$5
  local prefix source_root inventory record header mode oid stage repo_relative
  local relative source expected_mode status=0

  if [[ $kind == overlay ]]; then
    _overlay_checkout_matches "$path" "$url" || return 1
    prefix=home/$extension_relative
    source_root=$path/$prefix
  else
    prefix=$extension_relative
    source_root=$HOME/$prefix
  fi

  _dot_cleanup_mktemp || return 1
  inventory=$REPLY
  if ! _repo_git "$kind" "$path" ls-files --stage -z -- "$prefix" >"$inventory"; then
    _dot_cleanup_remove_path "$inventory" || true
    return 1
  fi
  if [[ $kind == overlay && -s $inventory ]]; then
    # Extension trust binds the overlay checkout before following a published
    # symlink into its home/ mapping. The per-entry pass below covers home/ and
    # every tracked parent component leading to the extension file.
    if ! _dot_extension_mode_directory "$path"; then
      status=1
    fi
  fi
  while IFS= read -r -d '' record; do
    [[ $status -eq 0 ]] || break
    [[ $record == *$'\t'* ]] || {
      status=1
      break
    }
    header=${record%%$'\t'*}
    repo_relative=${record#*$'\t'}
    read -r mode oid stage <<<"$header"
    [[ $mode =~ ^(100644|100755|120000)$ &&
      $oid =~ ^[0-9a-fA-F]{40,64}$ && $stage == 0 ]] || {
      status=1
      break
    }
    _dot_init_safe_relative_path "$repo_relative" || {
      status=1
      break
    }
    case $repo_relative in
      "$prefix"/*) relative=${repo_relative#"$prefix"/} ;;
      *)
        status=1
        break
        ;;
    esac
    if [[ $kind == overlay ]]; then
      _dot_extension_mode_directories "$path/home" \
        "$extension_relative/$relative" || {
        status=1
        break
      }
    else
      _dot_extension_mode_directories "$source_root" "$relative" || {
        status=1
        break
      }
    fi
    source=$path/$repo_relative
    [[ $kind == base ]] && source=$HOME/$repo_relative
    case $mode in
      100644 | 100755)
        if [[ $kind == base && -L $source ]]; then
          : # An active Git overlay owns and validates this linked generation.
        else
          [[ -f $source && ! -L $source && -O $source ]] || {
            status=1
            break
          }
          expected_mode=0644
          [[ $mode == 100755 ]] && expected_mode=0755
          chmod "$expected_mode" "$source" || {
            status=1
            break
          }
        fi
        ;;
      120000)
        [[ -L $source ]] || {
          status=1
          break
        }
        ;;
    esac

    _dot_extension_mode_directories "$HOME/$extension_relative" "$relative" || {
      status=1
      break
    }
  done <"$inventory"
  _dot_cleanup_remove_path "$inventory" || status=1
  [[ $status -eq 0 ]] || {
    printf 'dot: could not normalize trusted extension modes for %s\n' "$name" >&2
    return 1
  }
}

_dot_extension_modes_normalize() {
  local extension_relative

  [[ ${DOT_EXTENSION_API:-} == 1 && -n ${DOT_EXTENSIONS_DIR:-} ]] || return 0
  # External extension roots are not part of the managed HOME worktree. Their
  # owner retains mode policy, and the read-only trust checks still fail closed.
  _dot_extension_relative_root || return 0
  extension_relative=$REPLY
  _repo_each_existing _dot_extension_mode_repo "$extension_relative"
}
