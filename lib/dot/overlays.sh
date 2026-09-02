# shellcheck shell=bash
# Overlay discovery helpers shared by dot commands.

if ! declare -F _dot_overlay_field_safe >/dev/null 2>&1; then
  # shellcheck source=overlay-context.sh
  . "${BASH_SOURCE[0]%/*}/overlay-context.sh"
fi

# Active overlays, populated by _discover_overlays. Each entry:
# "name|path|url|conf|optional|sync". A missing final field means `git`; `none` means
# the source tree already exists locally and repository synchronization is
# deliberately outside dot.
# shellcheck disable=SC2034  # used by scripts that source this file
OVERLAYS=()
# Profile-aware discovery publishes each lifecycle boundary explicitly. The
# compatibility OVERLAYS array is assigned from exactly one of these sets at a
# call boundary; it is never a mixture of merely configured and active repos.
CONFIGURED_OVERLAY_NAMES=()
ELIGIBLE_OVERLAY_NAMES=()
ELIGIBLE_OVERLAYS=()
ACTIVE_OVERLAY_NAMES=()
ACTIVE_OVERLAYS=()
DOT_OVERLAY_LIFECYCLE=()
PHASE_ONE_SELECTED_OVERLAY_NAMES=()
PHASE_ONE_ELIGIBLE_OVERLAYS=()
PHASE_ONE_ACTIVE_OVERLAYS=()

# Extract overlay name from conf filename: "10-work.conf" → "work"
_overlay_name() {
  local base="${1##*/}"
  local sync="${2:-git}"
  base="${base%.conf}"
  [[ "$sync" == "none" ]] && base="${base%.local}"
  [[ "$base" =~ ^[0-9]+-(.+)$ ]] && base="${BASH_REMATCH[1]}"
  # printf, not echo: a name beginning with a dash would be eaten as an echo flag.
  printf '%s\n' "$base"
}

# Profile definitions need an identity before a selected descriptor is opened.
# `.local.conf` is the canonical profile-aware spelling for a sync=none
# descriptor, so normalize that suffix without depending on descriptor content.
_overlay_profile_name() {
  local base="${1##*/}"
  base="${base%.conf}"
  base="${base%.local}"
  [[ "$base" =~ ^[0-9]+-(.+)$ ]] && base="${BASH_REMATCH[1]}"
  printf '%s\n' "$base"
}

_overlay_descriptor_value_safe() {
  _dot_overlay_field_safe "$1"
}

_overlay_relative_path_safe() {
  local rel="$1"
  _overlay_descriptor_value_safe "$rel" || return 1
  case "$rel" in
    "" | /* | . | .. | ./* | ../* | */./* | */../* | */. | */.. | */ | *//*)
      return 1
      ;;
  esac
}

_overlay_conf_invalid() {
  REPLY="invalid overlay descriptor $1: $2"
  _warn "  warning: $REPLY"
  return 2
}

_overlay_descriptor_file_safe() {
  local path=$1 size
  [[ -f $path && ! -L $path ]] || return 1
  size=$(LC_ALL=C wc -c <"$path" 2>/dev/null | tr -d '[:space:]') || return 1
  [[ $size =~ ^[0-9]+$ && $size -le 65536 ]] || return 1
  LC_ALL=C od -An -t u1 "$path" 2>/dev/null |
    awk '{ for (i = 1; i <= NF; i++) if (($i < 32 && $i != 10) || $i == 127) exit 1 }'
}

# Directory holding overlay descriptor files.
_overlay_conf_dir() {
  dot_xdg_path config dot/overlays.d || return
  printf '%s\n' "$REPLY"
}

# Parse a single overlay conf file.
# Sets REPLY to "name|path|url|conf|optional|sync". Returns 1 when a valid
# descriptor is filtered from this host and 2 when the descriptor is invalid.
_parse_overlay_conf() {
  local file="$1"
  local url="" path="" platforms="" hosts="" optional="" sync="git"
  local seen_url=0 seen_path=0 seen_platforms=0 seen_hosts=0 seen_optional=0 seen_sync=0
  local line strict_error=""
  DOT_OVERLAY_PARSE_STATE=invalid
  local -a unknown_lines=()
  if [[ ${DOT_OVERLAY_STRICT_SELECTED:-0} == 1 ]]; then
    _overlay_descriptor_file_safe "$file" ||
      _overlay_conf_invalid "$file" "unsafe descriptor file" || return
  fi
  while IFS= read -r line || [[ -n "$line" ]]; do
    case "$line" in
      url=*)
        if ((seen_url > 0)) && [[ -z "$strict_error" ]]; then
          strict_error="duplicate url"
        fi
        seen_url=$((seen_url + 1))
        url="${line#url=}"
        ;;
      path=*)
        if ((seen_path > 0)) && [[ -z "$strict_error" ]]; then
          strict_error="duplicate path"
        fi
        seen_path=$((seen_path + 1))
        path="${line#path=}"
        ;;
      platforms=*)
        if ((seen_platforms > 0)) && [[ -z "$strict_error" ]]; then
          strict_error="duplicate platforms"
        fi
        seen_platforms=$((seen_platforms + 1))
        platforms="${line#platforms=}"
        ;;
      hosts=*)
        if ((seen_hosts > 0)) && [[ -z "$strict_error" ]]; then
          strict_error="duplicate hosts"
        fi
        seen_hosts=$((seen_hosts + 1))
        hosts="${line#hosts=}"
        ;;
      optional=*)
        if ((seen_optional > 0)) && [[ -z "$strict_error" ]]; then
          strict_error="duplicate optional"
        fi
        seen_optional=$((seen_optional + 1))
        optional="${line#optional=}"
        ;;
      sync=*)
        if ((seen_sync > 0)) && [[ -z "$strict_error" ]]; then
          strict_error="duplicate sync"
        fi
        seen_sync=$((seen_sync + 1))
        sync="${line#sync=}"
        ;;
      \#* | "") ;;
      *)
        unknown_lines+=("$line")
        if [[ -z "$strict_error" ]]; then
          strict_error="unknown key: ${line%%=*}"
        fi
        ;;
    esac
  done <"$file"

  if ((seen_sync > 1)); then
    _overlay_conf_invalid "$file" "duplicate sync" || return
  fi
  case "$sync" in
    git | none) ;;
    *) _overlay_conf_invalid "$file" "unknown sync value: $sync" || return ;;
  esac

  local name
  name=$(_overlay_name "$file" "$sync")

  # Externally managed sources bypass Git checkout and origin validation before
  # their files drive HOME mutations. Treat ambiguity in that newer descriptor
  # shape as an error rather than guessing which local tree owns a path. Git
  # descriptors predate this strict schema, so retain their historical permissive
  # parsing, including warning-only unknown keys, instead of making existing
  # overlays fail after an update.
  if [[ "$sync" == "none" || "$seen_path" -gt 0 ]]; then
    [[ -z "$strict_error" ]] || _overlay_conf_invalid "$file" "$strict_error" || return
    [[ "$sync" == "none" ]] || _overlay_conf_invalid "$file" "path requires sync=none" || return
    [[ "$seen_path" -eq 1 && -n "$path" ]] || _overlay_conf_invalid "$file" "missing path" || return
    [[ "$seen_url" -eq 0 ]] || _overlay_conf_invalid "$file" "url is not valid with sync=none" || return
    [[ "$seen_optional" -eq 0 ]] || _overlay_conf_invalid "$file" "optional is not valid with sync=none" || return
    if ! _overlay_descriptor_value_safe "$name" ||
      ! _overlay_descriptor_value_safe "$file"; then
      _overlay_conf_invalid "$file" "unrepresentable name or path" || return
    fi
    case "$path" in
      \~/*) path="$HOME/${path#\~/}" ;;
      /*) ;;
      *) _overlay_conf_invalid "$file" "path must be absolute or begin with ~/" || return ;;
    esac
    _overlay_descriptor_value_safe "$path" || _overlay_conf_invalid "$file" "unrepresentable path" || return
    case "$path" in
      / | */ | *//* | */./* | */../* | */. | */..)
        _overlay_conf_invalid "$file" "path must be normalized" || return
        ;;
    esac
    optional=false
  else
    if [[ ${DOT_OVERLAY_STRICT_SELECTED:-0} == 1 && -n $strict_error ]]; then
      _overlay_conf_invalid "$file" "$strict_error" || return
    fi
    for line in "${unknown_lines[@]+"${unknown_lines[@]}"}"; do
      _warn "  warning: unknown key in $file: $line"
    done
    if [[ -z "$url" ]]; then
      if [[ ${DOT_OVERLAY_STRICT_SELECTED:-0} == 1 ]]; then
        _overlay_conf_invalid "$file" "missing url" || return
      fi
      return 1
    fi
    _overlay_descriptor_value_safe "$url" ||
      _overlay_conf_invalid "$file" "unrepresentable url" || return
    case $optional in
      '' | true | false) ;;
      *)
        if [[ ${DOT_OVERLAY_STRICT_SELECTED:-0} == 1 ]]; then
          _overlay_conf_invalid "$file" "unknown optional value: $optional" || return
        fi
        _warn "  warning: unknown optional value in $file: $optional"
        ;;
    esac
  fi

  if [[ -n "$platforms" ]]; then
    if ! dot_platform_match "$platforms"; then
      DOT_OVERLAY_PARSE_STATE=ineligible
      return 1
    fi
  fi
  if [[ -n "$hosts" ]]; then
    if ! dot_host_match "$hosts"; then
      DOT_OVERLAY_PARSE_STATE=ineligible
      return 1
    fi
  fi
  # Optional overlays declare private or context-specific repos in the base
  # config without making every machine prove access to them. Store the flag on
  # the parsed active record so a filtered-out duplicate name cannot change the
  # behavior of the overlay that actually matched this machine.
  if [[ "$sync" == "git" ]]; then
    path="$HOME/.dotfiles-$name"
  fi
  REPLY="$name|$path|$url|$file|${optional:-false}|$sync"
  DOT_OVERLAY_PARSE_STATE=eligible
}

# Resolve an existing directory physically, then append any still-missing path
# components lexically. Overlay relative paths are normalized before this is
# called, so the suffix cannot escape the resolved ancestor.
_overlay_physical_dir_candidate() {
  local candidate="$1" suffix="" part parent physical
  [[ "$candidate" == /* ]] || return 1
  while [[ ! -d "$candidate" ]]; do
    [[ "$candidate" != "/" ]] || return 1
    part="${candidate##*/}"
    [[ -n "$part" ]] || return 1
    suffix="/$part$suffix"
    parent="${candidate%/*}"
    [[ -n "$parent" ]] || parent="/"
    [[ "$parent" != "$candidate" ]] || return 1
    candidate="$parent"
  done
  physical=$(cd -P -- "$candidate" 2>/dev/null && pwd -P) || return 1
  if [[ "$physical" == "/" ]]; then
    REPLY="/${suffix#/}"
  else
    REPLY="$physical$suffix"
  fi
}

_overlay_local_destination_safe() {
  local path="$1" rel="$2" source_root_real="${3:-}"
  local overlay_home="$path/home"
  local source_prefix rel_parent dst_parent destination_real candidate_prefix

  if ! _overlay_relative_path_safe "$rel"; then
    REPLY="$overlay_home (unrepresentable destination: $rel)"
    return 1
  fi
  if [[ -z "$source_root_real" ]]; then
    source_root_real=$(cd -P -- "$overlay_home" 2>/dev/null && pwd -P) || {
      REPLY="$overlay_home"
      return 1
    }
  fi
  source_prefix="${source_root_real%/}/"
  [[ "$source_root_real" == "/" ]] && source_prefix="/"

  rel_parent="${rel%/*}"
  if [[ "$rel_parent" == "$rel" ]]; then
    dst_parent="$HOME"
  else
    dst_parent="$HOME/$rel_parent"
  fi
  if ! _overlay_physical_dir_candidate "$dst_parent"; then
    REPLY="$overlay_home (cannot resolve destination: $rel)"
    return 1
  fi
  destination_real="$REPLY"
  candidate_prefix="${destination_real%/}/"
  [[ "$destination_real" == "/" ]] && candidate_prefix="/"
  case "$candidate_prefix" in
    "$source_prefix"*)
      REPLY="$overlay_home (destination resolves inside source: $rel)"
      return 1
      ;;
  esac
  REPLY=""
}

# No overlay writer may reach an active filesystem overlay's source through a
# symlinked destination parent. Check every local source, not just the writer's
# own source: overlay order and synchronization mode must not create a path
# that lets a later local or Git overlay mutate an earlier source tree.
_overlay_destination_outside_local_sources() {
  local rel="$1" entry path sync
  for entry in "${OVERLAYS[@]+"${OVERLAYS[@]}"}"; do
    IFS='|' read -r _ path _ _ _ sync <<<"$entry"
    sync="${sync:-git}"
    [[ "$sync" == "none" ]] || continue
    _overlay_local_destination_safe "$path" "$rel" || return 1
  done
  REPLY=""
}

# Revalidate one entry from a filesystem overlay inventory. This is shared by
# the initial preflight and the mutation-boundary check, because an external
# source may change while an update is running.
_overlay_local_source_entry_validate() {
  local path="$1" src="$2" rel="$3" source_root_real="$4"
  local overlay_home="$path/home"

  if [[ "$src" != "$overlay_home/$rel" ]] ||
    ! _overlay_relative_path_safe "$rel"; then
    REPLY="$overlay_home (unrepresentable entry)"
    return 1
  fi

  if [[ -L "$src" ]]; then
    if [[ ! -e "$src" ]]; then
      REPLY="$overlay_home (dangling symlink: $rel)"
      return 1
    elif [[ -d "$src" ]]; then
      if [[ ! -r "$src" || ! -x "$src" ]]; then
        REPLY="$overlay_home (unreadable symlink target: $rel)"
        return 1
      fi
    elif [[ ! -f "$src" || ! -r "$src" ]]; then
      REPLY="$overlay_home (unreadable symlink target: $rel)"
      return 1
    fi
  elif [[ ! -f "$src" || ! -r "$src" ]]; then
    REPLY="$overlay_home (unreadable entry: $rel)"
    return 1
  fi

  _overlay_local_destination_safe "$path" "$rel" "$source_root_real" || return 1
  _overlay_destination_outside_local_sources "$rel" || return 1
  REPLY=""
}

# Validate one filesystem overlay and leave a diagnostic in REPLY on failure.
# Regular entries must be readable. Source symlinks are allowed when they
# resolve to a readable regular file, or to a readable/searchable directory;
# dangling links and links to special files are rejected before HOME mutation.
_overlay_local_source_validate() {
  local path="$1"
  local overlay_home="$path/home" inventory="" src rel
  local source_root_real
  local invalid_inventory=0

  REPLY=""
  if [[ ! -d "$overlay_home" || ! -r "$overlay_home" || ! -x "$overlay_home" ]]; then
    REPLY="$overlay_home"
    return 1
  fi
  source_root_real=$(cd -P -- "$overlay_home" 2>/dev/null && pwd -P) || {
    REPLY="$overlay_home"
    return 1
  }
  # Inventory traversal can be slow on externally managed trees. Register the
  # scratch file at allocation time so a top-level signal removes it even when
  # Bash exits before the normal post-find cleanup below.
  if ! _dot_cleanup_mktemp 2>/dev/null; then
    REPLY="could not validate inventory for $overlay_home"
    return 1
  fi
  inventory=$REPLY
  if ! find "$overlay_home" \( -type f -o -type l \) ! -name '*.~[0-9]*~' -print0 \
    >"$inventory"; then
    REPLY="could not read inventory for $overlay_home"
    _dot_cleanup_remove_path "$inventory" || true
    return 1
  fi

  while IFS= read -r -d '' src; do
    rel="${src#"$overlay_home"/}"
    if ! _overlay_local_source_entry_validate \
      "$path" "$src" "$rel" "$source_root_real"; then
      invalid_inventory=1
      break
    fi
  done <"$inventory"
  _dot_cleanup_remove_path "$inventory" || true
  [[ "$invalid_inventory" -eq 0 ]] || return 1
  REPLY=""
}

# Validate every active filesystem overlay before repository synchronization,
# link mutation, or update finalization. The defensive linker call closes
# re-exec and direct-library entry paths as well.
_preflight_local_overlays() {
  local entry name path sync
  for entry in "${OVERLAYS[@]+"${OVERLAYS[@]}"}"; do
    IFS='|' read -r name path _ _ _ sync <<<"$entry"
    sync="${sync:-git}"
    [[ "$sync" == "none" ]] || continue
    if ! _overlay_local_source_validate "$path"; then
      _warn "  warning: $name overlay source is unavailable: ${REPLY:-$path/home}"
      return 1
    fi
  done
}

# Return success when an overlay record has a currently usable source without
# performing network access or mutating either repository or worktree state.
_overlay_record_active_existing() {
  local record=$1 name path url _descriptor optional sync
  IFS='|' read -r name path url _descriptor optional sync <<<"$record"
  case ${sync:-git} in
    git) _overlay_checkout_matches "$path" "$url" ;;
    none) _overlay_local_source_validate "$path" ;;
    *) return 1 ;;
  esac
}

_dot_overlay_use_set() {
  case ${1:-} in
    eligible) OVERLAYS=("${ELIGIBLE_OVERLAYS[@]+"${ELIGIBLE_OVERLAYS[@]}"}") ;;
    active) OVERLAYS=("${ACTIVE_OVERLAYS[@]+"${ACTIVE_OVERLAYS[@]}"}") ;;
    *) return 2 ;;
  esac
}

# Preserve pre-profile discovery semantics exactly when profiles.d is absent.
# In particular, Git descriptors retain a literal `.local` suffix and legacy
# names are not constrained to the stricter profile identifier grammar.
_discover_legacy_overlays() {
  local file name record parse_rc state seen_names=""
  for file in "$@"; do
    [[ -f $file || -L $file ]] || continue
    parse_rc=0
    DOT_OVERLAY_STRICT_SELECTED=0
    if _parse_overlay_conf "$file"; then
      record=$REPLY
      name=${record%%|*}
      if [[ " $seen_names " == *" $name "* ]]; then
        _warn "  warning: duplicate overlay name '$name' in $file — skipping"
        continue
      fi
      seen_names="$seen_names $name"
      CONFIGURED_OVERLAY_NAMES+=("$name")
      SELECTED_OVERLAY_NAMES+=("$name")
      ELIGIBLE_OVERLAY_NAMES+=("$name")
      ELIGIBLE_OVERLAYS+=("$record")
      if _overlay_record_active_existing "$record"; then
        ACTIVE_OVERLAY_NAMES+=("$name")
        ACTIVE_OVERLAYS+=("$record")
        DOT_OVERLAY_LIFECYCLE+=("$name|active|$file")
      else
        IFS='|' read -r _ _ _ _ selected _ <<<"$record"
        if [[ $selected == true ]]; then
          state='selected-optional-unavailable'
        else
          state='selected-unavailable'
        fi
        DOT_OVERLAY_LIFECYCLE+=("$name|$state|$file")
      fi
    else
      parse_rc=$?
      [[ $parse_rc -eq 1 ]] && continue
      DOT_OVERLAY_DISCOVERY_ERROR=${REPLY:-"invalid overlay descriptor: $file"}
      ELIGIBLE_OVERLAYS=()
      ACTIVE_OVERLAYS=()
      unset DOT_OVERLAY_STRICT_SELECTED
      return "${parse_rc:-2}"
    fi
  done
  unset DOT_OVERLAY_STRICT_SELECTED
  _dot_overlay_use_set eligible
}

# Discover selected overlay descriptors. Filename-derived identities are
# validated globally before any descriptor content is opened. In legacy mode,
# where profiles.d is absent, every descriptor retains its historical implicit
# selection.
_discover_overlays() {
  OVERLAYS=()
  CONFIGURED_OVERLAY_NAMES=()
  ELIGIBLE_OVERLAY_NAMES=()
  ELIGIBLE_OVERLAYS=()
  ACTIVE_OVERLAY_NAMES=()
  ACTIVE_OVERLAYS=()
  DOT_OVERLAY_LIFECYCLE=()
  unset DOT_OVERLAY_DISCOVERY_ERROR
  local conf_dir file name selected record parse_rc state
  local nullglob_was_set=0
  local -a descriptor_files=()
  local -A descriptors=() selected_names=()
  conf_dir="$(_overlay_conf_dir)"
  [[ -d "$conf_dir" ]] || return 0

  shopt -q nullglob && nullglob_was_set=1
  shopt -s nullglob
  descriptor_files=("$conf_dir"/*.conf)
  [[ $nullglob_was_set -eq 1 ]] || shopt -u nullglob

  if [[ ${DOT_PROFILES_PRESENT:-0} -ne 1 ]]; then
    _discover_legacy_overlays "${descriptor_files[@]+"${descriptor_files[@]}"}"
    return
  fi

  for file in "${descriptor_files[@]+"${descriptor_files[@]}"}"; do
    [[ -f $file || -L $file ]] || continue
    name=$(_overlay_profile_name "$file")
    if ! _dot_profile_identifier_valid "$name"; then
      DOT_OVERLAY_DISCOVERY_ERROR="invalid overlay descriptor filename: ${file##*/}"
      [[ ${DOT_OVERLAY_DISCOVERY_SILENT:-0} == 1 ]] ||
        printf 'dot: overlay: %s\n' "$DOT_OVERLAY_DISCOVERY_ERROR" >&2
      return 2
    fi
    if [[ -n ${descriptors[$name]+x} ]]; then
      DOT_OVERLAY_DISCOVERY_ERROR="duplicate overlay name '$name' in ${descriptors[$name]} and $file"
      [[ ${DOT_OVERLAY_DISCOVERY_SILENT:-0} == 1 ]] ||
        printf 'dot: overlay: %s\n' "$DOT_OVERLAY_DISCOVERY_ERROR" >&2
      return 2
    fi
    descriptors["$name"]=$file
    CONFIGURED_OVERLAY_NAMES+=("$name")
  done

  for name in "${SELECTED_OVERLAY_NAMES[@]+"${SELECTED_OVERLAY_NAMES[@]}"}"; do
    selected_names["$name"]=1
    if [[ -z ${descriptors[$name]+x} ]]; then
      DOT_OVERLAY_DISCOVERY_ERROR="selected overlay has no descriptor: $name"
      [[ ${DOT_OVERLAY_DISCOVERY_SILENT:-0} == 1 ]] ||
        printf 'dot: overlay: %s\n' "$DOT_OVERLAY_DISCOVERY_ERROR" >&2
      return 2
    fi
  done

  for name in "${CONFIGURED_OVERLAY_NAMES[@]+"${CONFIGURED_OVERLAY_NAMES[@]}"}"; do
    file=${descriptors[$name]}
    if [[ -z ${selected_names[$name]+x} ]]; then
      DOT_OVERLAY_LIFECYCLE+=("$name|not-selected|$file")
      continue
    fi
    parse_rc=0
    DOT_OVERLAY_STRICT_SELECTED=1
    if _parse_overlay_conf "$file"; then
      record=$REPLY
      ELIGIBLE_OVERLAY_NAMES+=("$name")
      ELIGIBLE_OVERLAYS+=("$record")
      if _overlay_record_active_existing "$record"; then
        ACTIVE_OVERLAY_NAMES+=("$name")
        ACTIVE_OVERLAYS+=("$record")
        DOT_OVERLAY_LIFECYCLE+=("$name|active|$file")
      else
        IFS='|' read -r _ _ _ _ selected _ <<<"$record"
        if [[ $selected == true ]]; then
          state='selected-optional-unavailable'
        else
          state='selected-unavailable'
        fi
        DOT_OVERLAY_LIFECYCLE+=("$name|$state|$file")
      fi
    else
      parse_rc=$?
      if [[ $parse_rc -eq 1 && ${DOT_OVERLAY_PARSE_STATE:-} == ineligible ]]; then
        DOT_OVERLAY_LIFECYCLE+=("$name|selected-ineligible|$file")
        continue
      fi
      DOT_OVERLAY_DISCOVERY_ERROR=${REPLY:-"invalid selected overlay descriptor: $file"}
      ELIGIBLE_OVERLAYS=()
      ACTIVE_OVERLAYS=()
      return "${parse_rc:-2}"
    fi
  done
  unset DOT_OVERLAY_STRICT_SELECTED
  _dot_overlay_use_set eligible
}

_dot_resolve_overlays() {
  local mode=${1:-inspect}
  case $mode in
    converge | inspect | fetch) ;;
    *) return 2 ;;
  esac

  _dot_profiles_load_default || return
  if [[ $DOT_PROFILES_PRESENT -eq 0 ]]; then
    _discover_overlays || return
    [[ $mode == converge ]] || _dot_overlay_use_set active
    return 0
  fi

  _dot_profile_select_base || return
  _discover_overlays || return
  # shellcheck disable=SC2034 # Published for lifecycle doctor reporting.
  PHASE_ONE_SELECTED_OVERLAY_NAMES=(
    "${SELECTED_OVERLAY_NAMES[@]+"${SELECTED_OVERLAY_NAMES[@]}"}"
  )
  # shellcheck disable=SC2034 # Published for lifecycle doctor reporting.
  PHASE_ONE_ELIGIBLE_OVERLAYS=(
    "${ELIGIBLE_OVERLAYS[@]+"${ELIGIBLE_OVERLAYS[@]}"}"
  )
  # shellcheck disable=SC2034 # Published for lifecycle doctor reporting.
  PHASE_ONE_ACTIVE_OVERLAYS=(
    "${ACTIVE_OVERLAYS[@]+"${ACTIVE_OVERLAYS[@]}"}"
  )
  _dot_overlay_use_set active
  _dot_profile_resolve_default || return
  _discover_overlays || return
  if [[ $mode == converge ]]; then
    _dot_overlay_use_set eligible
  else
    _dot_overlay_use_set active
  fi
}
