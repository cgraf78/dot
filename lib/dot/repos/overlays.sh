# shellcheck shell=bash
# Overlay link and skip-worktree management.
#
# Overlays intentionally shadow selected base-dotfiles paths with symlinks from
# their configured `home/` source. The base repo must mark those tracked paths
# skip-worktree while the overlay owns them, then restore the tracked version
# before pulling so Git never tries to merge through a symlink.

_overlay_link_target() {
  local rel="$1" name="$2" rest prefix=""
  rest="$rel"
  while [[ "$rest" == */* ]]; do
    rest="${rest#*/}"
    prefix="../$prefix"
  done
  REPLY="${prefix}.dotfiles-$name/home/$rel"
}

_overlay_record_link_target() {
  local rel="$1" name="$2" path="$3" sync="${4:-git}"
  case "$sync" in
    git) _overlay_link_target "$rel" "$name" ;;
    none) REPLY="$path/home/$rel" ;;
    *) return 1 ;;
  esac
}

_overlay_link_matches() {
  local rel="$1" name="$2" target
  [[ -n "$name" ]] || return 1
  if (($# >= 3)); then
    target="$3"
  else
    _overlay_link_target "$rel" "$name"
    target="$REPLY"
  fi
  [[ -n "$target" ]] || return 1
  [[ -L "$HOME/$rel" && "$(readlink "$HOME/$rel")" == "$target" ]]
}

# Check active providers independently of the generated manifest. This lets a
# missing manifest recover a live link without treating an arbitrary path as
# overlay-owned.
_overlay_active_provides() {
  local rel="$1" entry name path sync
  for entry in "${OVERLAYS[@]+"${OVERLAYS[@]}"}"; do
    IFS='|' read -r name path _ _ _ sync <<<"$entry"
    sync="${sync:-git}"
    if [[ -f "$path/home/$rel" || -L "$path/home/$rel" ]]; then
      return 0
    fi
  done
  return 1
}

_overlay_active_link_matches() {
  local rel="$1" entry name path sync target
  for entry in "${OVERLAYS[@]+"${OVERLAYS[@]}"}"; do
    IFS='|' read -r name path _ _ _ sync <<<"$entry"
    sync="${sync:-git}"
    _overlay_record_link_target "$rel" "$name" "$path" "$sync" || continue
    target="$REPLY"
    if [[ (-f "$path/home/$rel" || -L "$path/home/$rel") ]] &&
      _overlay_link_matches "$rel" "$name" "$target"; then
      return 0
    fi
  done
  return 1
}

_overlay_skip_worktree() {
  local entry
  entry=$(_base_git ls-files -v -- "$1" 2>/dev/null) || true
  [[ "${entry:0:2}" == "S " ]]
}

# A tracked regular file is safe to shadow only when it is visible to Git and
# unchanged from the index. A remaining skip-worktree bit is owned by the user
# unless unstash just proved and restored the managed symlink.
_overlay_tracked_path_clean() {
  local rel="$1"
  _overlay_skip_worktree "$rel" && return 1
  _base_git diff --quiet -- "$rel" 2>/dev/null
}

# The deterministic pending path is discoverable after an unclean exit. Random
# build temps are never authority because a later process cannot find them.
_overlay_pending_manifest_path() {
  REPLY="${DOT_OVERLAY_MANIFEST}.pending"
}

_overlay_private_regular_file() {
  local path="$1" mode links
  [[ -f "$path" && ! -L "$path" && -O "$path" ]] || return 1
  mode=$(stat -c '%a' "$path" 2>/dev/null || stat -f '%Lp' "$path" 2>/dev/null) || return 1
  links=$(stat -c '%h' "$path" 2>/dev/null || stat -f '%l' "$path" 2>/dev/null) || return 1
  [[ "$mode" != *[!0-7]* && "$links" == "1" ]] || return 1
  (((8#$mode & 077) == 0))
}

_overlay_file_identity() {
  local path="$1"
  REPLY=$(stat -c '%d:%i' "$path" 2>/dev/null || stat -f '%d:%i' "$path" 2>/dev/null) ||
    return 1
  [[ -n "$REPLY" ]]
}

_overlay_parse_manifest_record() {
  local line="$1" rel owner target remainder
  [[ "$line" == *$'\t'* ]] || return 1
  rel="${line%%$'\t'*}"
  remainder="${line#*$'\t'}"
  if [[ "$remainder" == *$'\t'* ]]; then
    owner="${remainder%%$'\t'*}"
    target="${remainder#*$'\t'}"
    [[ "$target" != *$'\t'* && -n "$target" ]] || return 1
  else
    owner="$remainder"
    _overlay_link_target "$rel" "$owner"
    target="$REPLY"
  fi
  case "$rel" in
    "" | /* | . | .. | ./* | ../* | */./* | */../* | */. | */.. | */ | *//*)
      return 1
      ;;
  esac
  case "$owner" in
    "" | . | .. | */*) return 1 ;;
  esac
  [[ "$target" != *$'\r'* && "$target" != *$'\n'* ]] || return 1
  REPLY_REL="$rel"
  REPLY_OWNER="$owner"
  REPLY_TARGET="$target"
}

_overlay_manifest_safe() {
  local path="$1" line exact_targets=0 links
  local REPLY_REL REPLY_OWNER REPLY_TARGET
  [[ -f "$path" && ! -L "$path" && -O "$path" ]] || return 1
  links=$(stat -c '%h' "$path" 2>/dev/null || stat -f '%l' "$path" 2>/dev/null) || return 1
  [[ "$links" == "1" ]] || return 1
  while IFS= read -r line || [[ -n "$line" ]]; do
    _overlay_parse_manifest_record "$line" || return 1
    [[ "${line#*$'\t'}" != *$'\t'* ]] || exact_targets=1
  done <"$path"
  # Legacy two-column files never exposed source paths and may predate the
  # private-mode invariant. Exact-target manifests can contain absolute local
  # paths, so only accept them when their mode is private.
  [[ "$exact_targets" -eq 0 ]] || _overlay_private_regular_file "$path"
}

_overlay_pending_manifest_safe() {
  _overlay_private_regular_file "$1" && _overlay_manifest_safe "$1"
}

# Selected, legacy, and write-ahead manifests may each describe a live link
# after a crash. Callers must accept their union and any recorded owner, while
# still validating the exact generated symlink before cleanup.
_overlay_authority_files() {
  local pending candidate existing duplicate
  _overlay_pending_manifest_path
  pending="$REPLY"
  if [[ -e "$DOT_OVERLAY_MANIFEST" || -L "$DOT_OVERLAY_MANIFEST" ]]; then
    if ! _overlay_manifest_safe "$DOT_OVERLAY_MANIFEST"; then
      REPLY="$DOT_OVERLAY_MANIFEST"
      return 1
    fi
  fi
  if [[ -e "$pending" || -L "$pending" ]]; then
    if ! _overlay_pending_manifest_safe "$pending"; then
      REPLY="$pending"
      return 1
    fi
  fi

  OVERLAY_AUTHORITY_MANIFESTS=()
  for candidate in "$DOT_OVERLAY_MANIFEST" "$DOT_OVERLAY_LEGACY_MANIFEST" "$pending"; do
    [[ -f "$candidate" && ! -L "$candidate" ]] || continue
    if [[ "$candidate" == "$pending" ]]; then
      _overlay_pending_manifest_safe "$candidate" || {
        REPLY="$candidate"
        return 1
      }
    elif ! _overlay_manifest_safe "$candidate"; then
      REPLY="$candidate"
      return 1
    fi
    duplicate=0
    for existing in "${OVERLAY_AUTHORITY_MANIFESTS[@]+"${OVERLAY_AUTHORITY_MANIFESTS[@]}"}"; do
      if [[ "$existing" == "$candidate" ]]; then
        duplicate=1
        break
      fi
    done
    [[ "$duplicate" -eq 1 ]] || OVERLAY_AUTHORITY_MANIFESTS+=("$candidate")
  done
  REPLY="$pending"
}

# Populates the caller's dynamically scoped associative authority maps.
_overlay_load_authority() {
  local manifest line rel target REPLY_REL REPLY_OWNER REPLY_TARGET
  _overlay_authority_files || return 1
  for manifest in "${OVERLAY_AUTHORITY_MANIFESTS[@]+"${OVERLAY_AUTHORITY_MANIFESTS[@]}"}"; do
    while IFS= read -r line || [[ -n "$line" ]]; do
      _overlay_parse_manifest_record "$line" || return 1
      rel="$REPLY_REL"
      target="$REPLY_TARGET"
      _overlay_path_is_authority "$rel" && continue
      _overlay_authority_paths["$rel"]=1
      _overlay_authority_targets["$rel"$'\t'"$target"]=1
    done <"$manifest"
  done
}

_overlay_authority_link_matches() {
  local rel="$1" dst="$HOME/$1" target
  [[ -L "$dst" ]] || return 1
  target=$(readlink "$dst") || return 1
  [[ -n "${_overlay_authority_targets["$rel"$'\t'"$target"]+x}" ]]
}

_overlay_path_is_authority() {
  local rel="$1" pending
  _overlay_pending_manifest_path
  pending="$REPLY"
  [[ "$HOME/$rel" == "$DOT_OVERLAY_MANIFEST" ||
    "$HOME/$rel" == "$DOT_OVERLAY_LEGACY_MANIFEST" ||
    "$HOME/$rel" == "$pending" ]] ||
    dot_candidate_path_is_reserved "$HOME/$rel"
}

_overlay_append_manifest_records() {
  local source="$1" destination="$2" line REPLY_REL REPLY_OWNER REPLY_TARGET
  while IFS= read -r line || [[ -n "$line" ]]; do
    _overlay_parse_manifest_record "$line" || return 1
    _overlay_path_is_authority "$REPLY_REL" && continue
    printf '%s\t%s\t%s\n' "$REPLY_REL" "$REPLY_OWNER" "$REPLY_TARGET" \
      >>"$destination" || return 1
  done <"$source"
}

_overlay_append_candidates() {
  local destination="$1" name="$2" path="$3" inventory="$4" sync="${5:-git}"
  local overlay_home="$path/home" src rel target rc=0
  local REPLY_REL REPLY_OWNER REPLY_TARGET
  [[ -f "$inventory" && ! -L "$inventory" ]] || return 1
  while IFS= read -r -d '' src; do
    rel="${src#"$overlay_home"/}"
    _overlay_path_is_authority "$rel" && return 1
    _overlay_record_link_target "$rel" "$name" "$path" "$sync" || return 1
    target="$REPLY"
    if ! _overlay_parse_manifest_record "$rel"$'\t'"$name"$'\t'"$target" ||
      ! printf '%s\t%s\t%s\n' "$rel" "$name" "$target" >>"$destination"; then
      rc=1
      break
    fi
  done <"$inventory"
  return "$rc"
}

# Freeze each provider's candidate set before publishing pending recovery
# authority or mutating HOME and the Git index. The same NUL-delimited inventory
# drives both authority publication and linking, so every link attempted by this
# run was covered by its recovery record first. Externally managed sources can
# change independently, so retain their physical root and identity for the
# revalidation performed at each later acceptance and mutation boundary.
_overlay_prepare_inventories() {
  local root="$1" entry name path url sync inventory index=0
  local source_root_real source_root_identity
  for entry in "${OVERLAYS[@]+"${OVERLAYS[@]}"}"; do
    IFS='|' read -r name path url _ _ sync <<<"$entry"
    sync="${sync:-git}"
    [[ -d "$path/home" ]] || continue
    if [[ "$sync" == "git" ]]; then
      _overlay_is_worktree "$path" || continue
      _overlay_checkout_matches "$path" "$url" || continue
    else
      source_root_real=$(cd -P -- "$path/home" 2>/dev/null && pwd -P) || return 1
      _overlay_file_identity "$source_root_real" || return 1
      source_root_identity="$REPLY"
      _overlay_inventory_source_roots["$name"]="$source_root_real"
      _overlay_inventory_source_identities["$name"]="$source_root_identity"
    fi
    index=$((index + 1))
    inventory="$root/$index"
    : >"$inventory" || return 1
    chmod 600 "$inventory" || return 1
    find "$path/home" \( -type f -o -type l \) ! -name '*.~[0-9]*~' -print0 \
      >"$inventory" || return 1
    _overlay_inventory_files["$name"]="$inventory"
  done
}

_overlay_local_source_snapshot_matches() {
  local path="$1" expected_root="$2" expected_identity="$3"
  local current_root
  REPLY=""
  if [[ -z "$expected_root" || -z "$expected_identity" ]]; then
    REPLY="$path/home (missing inventory identity)"
    return 1
  fi
  current_root=$(cd -P -- "$path/home" 2>/dev/null && pwd -P) || {
    REPLY="$path/home"
    return 1
  }
  if [[ "$current_root" != "$expected_root" ]]; then
    REPLY="$path/home (source root changed)"
    return 1
  fi
  if ! _overlay_file_identity "$current_root" ||
    [[ "$REPLY" != "$expected_identity" ]]; then
    REPLY="$path/home (source root replaced)"
    return 1
  fi
  REPLY=""
}

_overlay_local_inventory_entry_current() {
  local path="$1" src="$2" rel="$3" expected_root="$4" expected_identity="$5"
  _overlay_local_source_snapshot_matches \
    "$path" "$expected_root" "$expected_identity" || return 1
  _overlay_local_source_entry_validate "$path" "$src" "$rel" "$expected_root"
}

# Publish old authority plus every exact link target this run may create before
# the first HOME or Git index mutation. The over-approximation is safe because
# cleanup still requires a live symlink to match a recorded generated target.
_overlay_publish_pending() {
  local pending build manifest entry name path sync inventory pending_exists=0
  if ! _overlay_authority_files; then
    _warn "  warning: unsafe overlay recovery manifest; refusing to link: $REPLY"
    return 1
  fi
  pending="$REPLY"
  [[ -e $pending || -L $pending ]] && pending_exists=1
  build=$(mktemp "${pending}.tmp.XXXXXX" 2>/dev/null) || {
    _warn "  warning: could not create overlay recovery manifest temp file: ${pending%/*}"
    return 1
  }
  if ! chmod 600 "$build"; then
    _warn "  warning: could not secure overlay recovery manifest temp file: $build"
    rm -f -- "$build"
    return 1
  fi

  for manifest in "${OVERLAY_AUTHORITY_MANIFESTS[@]+"${OVERLAY_AUTHORITY_MANIFESTS[@]}"}"; do
    if ! _overlay_append_manifest_records "$manifest" "$build"; then
      _warn "  warning: could not preserve overlay recovery authority: $manifest"
      rm -f -- "$build"
      return 1
    fi
  done
  for entry in "${OVERLAYS[@]+"${OVERLAYS[@]}"}"; do
    IFS='|' read -r name path _ _ _ sync <<<"$entry"
    sync="${sync:-git}"
    inventory="${_overlay_inventory_files[$name]-}"
    [[ -n "$inventory" ]] || continue
    if ! _overlay_append_candidates "$build" "$name" "$path" "$inventory" "$sync"; then
      _warn "  warning: could not inventory $name overlay recovery authority"
      rm -f -- "$build"
      return 1
    fi
  done

  if { [[ $pending_exists -eq 0 ]] && ! _dot_move_noreplace "$build" "$pending"; } ||
    { [[ $pending_exists -eq 1 ]] && ! _dot_move_replace_nodir "$build" "$pending"; }; then
    _warn "  warning: could not publish overlay recovery manifest: $pending"
    rm -f -- "$build"
    return 1
  fi
  if ! _overlay_pending_manifest_safe "$pending"; then
    _warn "  warning: published overlay recovery manifest is unsafe: $pending"
    return 1
  fi
  REPLY="$pending"
}

_overlay_destination_context() {
  local destination=$1
  if dot_candidate_path_is_reserved "$destination"; then
    return 1
  fi
  _dot_physical_leaf_candidate "$destination" || return 1
  OVERLAY_PHYSICAL_DESTINATION=$REPLY
  OVERLAY_PHYSICAL_PARENT=$REPLY_PHYSICAL_PARENT
  OVERLAY_PARENT_IDENTITY=$REPLY_PARENT_IDENTITY
}

_overlay_replacement_record_path() {
  local destination=$1 hash
  hash=$(printf '%s' "$destination" |
    _overlay_replacement_hash_object --stdin) || return 1
  REPLY=${DOT_OVERLAY_MANIFEST}.replace.$hash
}

_overlay_write_private_line() {
  local destination=$1 line=$2 temporary
  [[ ! -e $destination && ! -L $destination ]] || return 1
  temporary=$(mktemp "${destination}.tmp.XXXXXX" 2>/dev/null) || return 1
  if ! chmod 0600 "$temporary" ||
    ! printf '%s\n' "$line" >"$temporary" ||
    ! _dot_move_noreplace "$temporary" "$destination"; then
    rm -f -- "$temporary"
    return 1
  fi
  _overlay_private_regular_file "$destination"
}

_overlay_private_directory() {
  local directory=$1 mode
  [[ -d $directory && ! -L $directory && -O $directory ]] || return 1
  mode=$(stat -c '%a' "$directory" 2>/dev/null ||
    stat -f '%Lp' "$directory" 2>/dev/null) || return 1
  [[ $mode != *[!0-7]* ]] || return 1
  (((8#$mode & 077) == 0))
}

# Durable replacement identities and record names must not inherit the caller's
# selected Git repository or configuration. Keep every replacement hash command
# behind one narrow sanitized Git boundary.
_overlay_replacement_git() (
  unset GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR GIT_OBJECT_DIRECTORY
  unset GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_INDEX_FILE GIT_CONFIG
  unset GIT_CONFIG_GLOBAL GIT_CONFIG_SYSTEM GIT_CONFIG_COUNT
  unset GIT_CONFIG_PARAMETERS GIT_CONFIG_NOSYSTEM GIT_DEFAULT_HASH
  export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
  command git "$@"
)

_overlay_replacement_hash_object() {
  _overlay_replacement_git -C "$DOT_SOURCE_ROOT" hash-object "$@"
}

# A legacy record name may have been created while the caller exported a Git
# repository with the other supported object format. Recreate that exact Git
# hash in a private throwaway repository; do not add another digest utility or
# let legacy compatibility affect the format used by new publications.
_overlay_replacement_hash_object_format() (
  local object_format=$1 value=$2 temporary hash status=0

  case $object_format in
    sha1 | sha256) ;;
    *) return 1 ;;
  esac
  temporary=$(umask 077 &&
    mktemp -d "${TMPDIR:-/tmp}/dot-overlay-record-hash.XXXXXX") || return 1
  if ! _overlay_replacement_git init -q --bare --template= \
    --object-format="$object_format" "$temporary"; then
    status=1
  elif ! hash=$(printf '%s' "$value" |
    _overlay_replacement_git --git-dir="$temporary" hash-object --stdin); then
    status=1
  fi
  rm -rf -- "$temporary" || status=1
  if [[ $status -eq 0 ]]; then
    printf '%s\n' "$hash" || status=1
  fi
  return "$status"
)

_overlay_replacement_legacy_record_path_matches() {
  local record=$1 destination=$2 current_hash=$3
  local prefix suffix object_format alternate_hash

  prefix=${DOT_OVERLAY_MANIFEST}.replace.
  [[ $record == "$prefix"* ]] || return 1
  suffix=${record#"$prefix"}
  [[ ${#suffix} -ne ${#current_hash} ]] || return 1
  if [[ $suffix =~ ^[0-9a-f]{40}$ ]]; then
    object_format=sha1
  elif [[ $suffix =~ ^[0-9a-f]{64}$ ]]; then
    object_format=sha256
  else
    return 1
  fi
  alternate_hash=$(
    _overlay_replacement_hash_object_format "$object_format" "$destination"
  ) ||
    return 1
  [[ $suffix == "$alternate_hash" ]]
}

# Device and inode alone are not a generation identity: a filesystem may reuse
# an inode immediately after a late writer unlinks the validated destination.
# Bind replacement authority to the no-follow mode, byte size, and exact
# symlink-target/file bytes as well. The before/after metadata check rejects a
# generation that changes while its fingerprint is being computed.
_overlay_replacement_identity() {
  local path=$1 identity metadata digest target
  local identity_after metadata_after

  identity=$(_dot_path_identity "$path") || return 1
  metadata=$(stat -c '%f:%s' "$path" 2>/dev/null ||
    stat -f '%p:%z' "$path" 2>/dev/null) || return 1
  if [[ -L $path ]]; then
    target=$(readlink "$path") || return 1
    digest=$(printf '%s' "$target" |
      _overlay_replacement_hash_object --stdin) || return 1
  elif [[ -f $path ]]; then
    digest=$(_overlay_replacement_hash_object --no-filters -- "$path") ||
      return 1
  else
    return 1
  fi
  identity_after=$(_dot_path_identity "$path") || return 1
  metadata_after=$(stat -c '%f:%s' "$path" 2>/dev/null ||
    stat -f '%p:%z' "$path" 2>/dev/null) || return 1
  [[ $identity_after == "$identity" && $metadata_after == "$metadata" ]] ||
    return 1
  printf '%s:%s:%s\n' "$identity" "$metadata" "$digest"
}

_overlay_replacement_generation_matches() {
  local path=$1 expected=$2 identity_kind=$3 observed

  case $identity_kind in
    content)
      observed=$(_overlay_replacement_identity "$path" 2>/dev/null) || return 1
      ;;
    legacy)
      # Pre-content-identity releases stored only the no-follow device/inode
      # pair. Limit that weaker authority to recovery records which have passed
      # the surrounding private-record, exact-path, parent, and transaction
      # validation; new publications never create this form.
      observed=$(_dot_path_identity "$path" 2>/dev/null) || return 1
      ;;
    *) return 1 ;;
  esac
  [[ $observed == "$expected" ]]
}

_overlay_replacement_transaction_safe() {
  local transaction=$1 entry nullglob_was_set=0 dotglob_was_set=0
  local -a entries=()
  _overlay_private_directory "$transaction" || return 1
  shopt -q nullglob && nullglob_was_set=1
  shopt -q dotglob && dotglob_was_set=1
  shopt -s nullglob dotglob
  entries=("$transaction"/*)
  [[ $nullglob_was_set -eq 1 ]] || shopt -u nullglob
  [[ $dotglob_was_set -eq 1 ]] || shopt -u dotglob
  for entry in "${entries[@]+"${entries[@]}"}"; do
    case ${entry##*/} in
      next | previous) ;;
      *) return 1 ;;
    esac
  done
}

_overlay_replacement_read() {
  local record=$1 line destination physical target expected transaction parent_identity extra
  local hash expected_record expected_transaction identity_kind
  _overlay_private_regular_file "$record" || return 1
  line=$(<"$record")
  [[ $line != *$'\n'* ]] || return 1
  IFS=$'\t' read -r destination physical target expected transaction parent_identity extra <<<"$line"
  [[ -z $extra && $destination == /* && $physical == /* && $transaction == /* &&
    $target != *$'\t'* && $target != *$'\n'* && $target != *$'\r'* &&
    $parent_identity =~ ^[0-9]+:[0-9]+$ ]] ||
    return 1
  if [[ $expected =~ ^[0-9]+:[0-9]+:[0-9A-Fa-f]+:[0-9]+:([0-9a-f]{40}|[0-9a-f]{64})$ ]]; then
    identity_kind=content
  elif [[ $expected =~ ^[0-9]+:[0-9]+$ ]]; then
    identity_kind=legacy
  else
    return 1
  fi
  hash=$(printf '%s' "$destination" |
    _overlay_replacement_hash_object --stdin) || return 1
  expected_record=${DOT_OVERLAY_MANIFEST}.replace.$hash
  if [[ $record != "$expected_record" ]]; then
    [[ $identity_kind == legacy ]] &&
      _overlay_replacement_legacy_record_path_matches \
        "$record" "$destination" "$hash" || return 1
  fi
  expected_transaction=${physical%/*}/.${physical##*/}.dot-overlay-replace-v1
  [[ $transaction == "$expected_transaction" ]] || return 1
  OVERLAY_REPLACE_DESTINATION=$destination
  OVERLAY_REPLACE_PHYSICAL=$physical
  OVERLAY_REPLACE_TARGET=$target
  OVERLAY_REPLACE_EXPECTED=$expected
  OVERLAY_REPLACE_IDENTITY_KIND=$identity_kind
  OVERLAY_REPLACE_TRANSACTION=$transaction
  OVERLAY_REPLACE_PARENT_IDENTITY=$parent_identity
}

_overlay_replacement_cleanup() {
  local record=$1 transaction=$2 target=$3
  if [[ -e $transaction/next || -L $transaction/next ]]; then
    [[ -L $transaction/next && $(readlink "$transaction/next") == "$target" ]] ||
      return 1
    rm -f -- "$transaction/next" || return 1
  fi
  [[ ! -e $transaction/previous && ! -L $transaction/previous ]] || return 1
  rmdir "$transaction" || return 1
  rm -f -- "$record"
}

_overlay_recover_replacement() {
  local record=$1 destination physical target expected transaction parent_identity
  local identity_kind previous=$record physical_parent previous_identity
  local lexical_parent_current=0
  _overlay_replacement_read "$record" || return 1
  destination=$OVERLAY_REPLACE_DESTINATION
  physical=$OVERLAY_REPLACE_PHYSICAL
  target=$OVERLAY_REPLACE_TARGET
  expected=$OVERLAY_REPLACE_EXPECTED
  identity_kind=$OVERLAY_REPLACE_IDENTITY_KIND
  transaction=$OVERLAY_REPLACE_TRANSACTION
  parent_identity=$OVERLAY_REPLACE_PARENT_IDENTITY
  previous=$transaction/previous
  physical_parent=${physical%/*}
  [[ $(_dot_path_identity "$physical_parent" 2>/dev/null || true) == "$parent_identity" ]] ||
    return 1
  if _dot_physical_leaf_candidate "$destination" &&
    [[ $REPLY == "$physical" && $REPLY_PARENT_IDENTITY == "$parent_identity" ]]; then
    lexical_parent_current=1
  fi

  if [[ ! -e $transaction && ! -L $transaction ]]; then
    _overlay_replacement_generation_matches "$physical" "$expected" "$identity_kind" ||
      return 1
    rm -f -- "$record"
    return 0
  fi
  _overlay_replacement_transaction_safe "$transaction" || return 1
  if [[ -e $transaction/next || -L $transaction/next ]]; then
    [[ -L $transaction/next && $(readlink "$transaction/next") == "$target" ]] ||
      return 1
  fi

  if [[ -e $previous || -L $previous ]]; then
    _overlay_replacement_generation_matches "$previous" "$expected" "$identity_kind" ||
      return 1
    previous_identity=$(_overlay_replacement_identity "$previous") || return 1
    if [[ ! -e $physical && ! -L $physical ]]; then
      _dot_move_noreplace "$previous" "$physical" || return 1
      [[ $(_overlay_replacement_identity "$physical" 2>/dev/null || true) == "$previous_identity" ]] ||
        return 1
      _overlay_replacement_cleanup "$record" "$transaction" "$target"
      return
    fi
    if [[ -L $physical &&
      $(readlink "$physical") == "$target" ]]; then
      if [[ $lexical_parent_current -eq 1 ]]; then
        rm -f -- "$previous" || return 1
      else
        # The desired link was published into the original physical parent,
        # but the user's lexical parent now names a different generation. Park
        # the exact generated link before restoring the exact old generation;
        # every move is exclusive so a late third-party winner survives.
        [[ ! -e $transaction/next && ! -L $transaction/next ]] || return 1
        _dot_move_noreplace "$physical" "$transaction/next" || return 1
        if [[ ! -L $transaction/next ||
          $(readlink "$transaction/next") != "$target" ]]; then
          _dot_move_noreplace "$transaction/next" "$physical" || true
          return 1
        fi
        _dot_move_noreplace "$previous" "$physical" || return 1
      fi
      _overlay_replacement_cleanup "$record" "$transaction" "$target"
      return
    fi
    return 1
  fi

  if _overlay_replacement_generation_matches "$physical" "$expected" "$identity_kind"; then
    _overlay_replacement_cleanup "$record" "$transaction" "$target"
  elif [[ -L $physical && $(readlink "$physical") == "$target" ]]; then
    _overlay_replacement_cleanup "$record" "$transaction" "$target"
  else
    return 1
  fi
}

_overlay_recover_replacements() {
  local nullglob_was_set=0 record
  local -a records=()
  shopt -q nullglob && nullglob_was_set=1
  shopt -s nullglob
  records=("${DOT_OVERLAY_MANIFEST}.replace."*)
  [[ $nullglob_was_set -eq 1 ]] || shopt -u nullglob
  for record in "${records[@]+"${records[@]}"}"; do
    _overlay_recover_replacement "$record" || {
      REPLY=$record
      return 1
    }
  done
}

_overlay_publish_link() {
  local target=$1 destination=$2 expected_identity=${3:-}
  local parent stage staged physical parent_identity record transaction previous
  local parked_identity current_parent current_parent_identity
  _overlay_destination_context "$destination" || return 1
  physical=$OVERLAY_PHYSICAL_DESTINATION
  parent=$OVERLAY_PHYSICAL_PARENT
  parent_identity=$OVERLAY_PARENT_IDENTITY
  stage=$(mktemp -d "$parent/.${destination##*/}.overlay-link.XXXXXX") || return 1
  chmod 0700 "$stage" || {
    rmdir "$stage" 2>/dev/null || true
    return 1
  }
  staged=$stage/link
  if ! ln -s "$target" "$staged"; then
    rm -f -- "$staged"
    rmdir "$stage" 2>/dev/null || true
    return 1
  fi
  if [[ -n $expected_identity ]]; then
    if [[ $(_overlay_replacement_identity "$destination" 2>/dev/null || true) != "$expected_identity" ]]; then
      rm -f -- "$staged"
      rmdir "$stage" 2>/dev/null || true
      return 1
    fi
    _overlay_replacement_record_path "$destination" || return 1
    record=$REPLY
    if [[ -e $record || -L $record ]]; then
      _overlay_recover_replacement "$record" || return 1
    fi
    transaction=$parent/.${physical##*/}.dot-overlay-replace-v1
    [[ ! -e $transaction && ! -L $transaction ]] || return 1
    _overlay_write_private_line "$record" \
      "$destination"$'\t'"$physical"$'\t'"$target"$'\t'"$expected_identity"$'\t'"$transaction"$'\t'"$parent_identity" ||
      return 1
    # The directory becomes durable recovery authority as soon as mkdir
    # succeeds. Create it under a private umask so SIGKILL cannot strand a
    # briefly world-readable transaction before the defensive chmod below.
    (umask 077 && mkdir "$transaction") || return 1
    chmod 0700 "$transaction" || return 1
    _dot_move_noreplace "$staged" "$transaction/next" || return 1
    rmdir "$stage" 2>/dev/null || return 1
    previous=$transaction/previous
    if ! _dot_move_noreplace "$physical" "$previous"; then
      _overlay_recover_replacement "$record" || true
      return 1
    fi
    parked_identity=$(_overlay_replacement_identity "$previous") || return 1
    if [[ $parked_identity != "$expected_identity" ]]; then
      _dot_move_noreplace "$previous" "$physical" || return 1
      _overlay_replacement_cleanup "$record" "$transaction" "$target" || return 1
      return 1
    fi
    if ! _dot_move_noreplace "$transaction/next" "$physical"; then
      _overlay_recover_replacement "$record" || true
      return 1
    fi
    [[ -L $physical && $(readlink "$physical") == "$target" ]] || return 1
    _dot_physical_leaf_candidate "$destination" || return 1
    current_parent=$REPLY_PHYSICAL_PARENT
    current_parent_identity=$REPLY_PARENT_IDENTITY
    if [[ $current_parent != "$parent" || $current_parent_identity != "$parent_identity" ]]; then
      _dot_move_noreplace "$physical" "$transaction/next" || return 1
      _dot_move_noreplace "$previous" "$physical" || return 1
      _overlay_replacement_cleanup "$record" "$transaction" "$target" || return 1
      return 1
    fi
    rm -f -- "$previous" || return 1
    _overlay_replacement_cleanup "$record" "$transaction" "$target" || return 1
    return 0
  elif ! _dot_move_noreplace "$staged" "$physical"; then
    rm -f -- "$staged"
    rmdir "$stage" 2>/dev/null || true
    return 1
  fi
  rmdir "$stage" 2>/dev/null || true
  _dot_physical_leaf_candidate "$destination" || return 1
  if [[ $REPLY_PHYSICAL_PARENT != "$parent" ||
    $REPLY_PARENT_IDENTITY != "$parent_identity" ]]; then
    [[ -L $physical && $(readlink "$physical") == "$target" ]] && rm -f -- "$physical"
    return 1
  fi
  [[ -L $physical && $(readlink "$physical") == "$target" ]]
}

_overlay_record_final() {
  local rel="$1" owner="$2" target="$3"
  printf '%s\t%s\t%s\n' "$rel" "$owner" "$target" \
    >>"$_overlay_manifest_new" || return 1
  _overlay_current_paths["$rel"]=1
}

# Restore only skip-worktree paths whose live symlink proves current overlay
# ownership. Other tools and users can set the same index bit, while the prior
# manifest can become stale, so neither signal alone authorizes a destructive
# checkout. _link_overlays re-symlinks owned paths after pull.
_overlay_restore_tracked_path() {
  local rel="$1"
  if ! _overlay_destination_outside_local_sources "$rel"; then
    _warn "  warning: refusing to restore a base path inside a local overlay source: ${REPLY:-$rel}"
    return 1
  fi
  # shellcheck disable=SC2086  # _base_git is intentionally word-split (multi-word command).
  if ! _base_git update-index --no-skip-worktree "$rel" 2>/dev/null; then
    _warn "  warning: could not clear overlay index state: $rel"
    return 1
  fi
  # shellcheck disable=SC2086  # _base_git is intentionally word-split (multi-word command).
  if ! _base_git checkout -- "$rel" 2>/dev/null; then
    _warn "  warning: could not restore overlay base path: $rel"
    return 1
  fi
}

_unstash_overlay_overrides() {
  _base_repo_exists || return 0

  if ! _overlay_recover_replacements; then
    _warn "  warning: unsafe overlay replacement recovery record: $REPLY"
    return 1
  fi

  local -A _overlay_authority_paths=()
  local -A _overlay_authority_targets=()
  local -a OVERLAY_AUTHORITY_MANIFESTS=()
  if ! _overlay_load_authority; then
    _warn "  warning: unsafe overlay recovery manifest; leaving overlay paths untouched: $REPLY"
    return 1
  fi

  local entry f owned
  while IFS= read -r -d '' entry; do
    [[ "${entry:0:2}" == "S " ]] || continue
    f="${entry:2}"
    owned=0

    if [[ -n "${_overlay_authority_paths[$f]+x}" ]]; then
      owned=1
      if _overlay_authority_link_matches "$f"; then
        _overlay_restore_tracked_path "$f" || return 1
        continue
      fi
    fi
    if _overlay_active_provides "$f"; then
      owned=1
    fi
    if _overlay_active_link_matches "$f"; then
      _overlay_restore_tracked_path "$f" || return 1
    elif [[ "$owned" -eq 1 ]]; then
      _warn "  warning: preserving replaced overlay path: $f"
    fi
  done < <(_base_git ls-files -v -z 2>/dev/null)
}

# Link a single overlay's home/ directory into $HOME.
# Creates record-specific symlinks. Sets skip-worktree on base-repo files
# that overlay symlinks shadow.
# Appends linked paths to $_overlay_manifest_new (set by _link_overlays).
# Uses $_base_tracked (associative array) for O(1) tracked-file lookups.
# Sets REPLY to display text and reports the outcome through REPLY_STATUS
# (changed|current, or empty when there is no overlay home to process).
_link_overlay() {
  local name="$1" path="$2" inventory="$3" sync="${4:-git}"
  local overlay_home="$path/home"
  local source_root_real="" source_root_identity=""
  REPLY=""
  REPLY_STATUS=""
  [[ -d "$overlay_home" ]] || return 0
  [[ -f "$inventory" && ! -L "$inventory" ]] || return 1
  if [[ "${DOT_VERBOSE:-0}" -eq 1 ]]; then
    _ui_status running "$name overlay: linking"
  fi
  if [[ "$sync" == "none" ]]; then
    source_root_real="${_overlay_inventory_source_roots[$name]-}"
    source_root_identity="${_overlay_inventory_source_identities[$name]-}"
    if ! _overlay_local_source_snapshot_matches \
      "$path" "$source_root_real" "$source_root_identity"; then
      _warn "  warning: $name overlay source changed after inventory: $overlay_home"
      return 1
    fi
  fi
  local linked=0
  local current=0
  while IFS= read -r -d '' src; do
    local rel="${src#"$overlay_home"/}"
    local dst="$HOME/$rel"
    if _overlay_path_is_authority "$rel"; then
      _warn "  warning: $name overlay contains a reserved path: $rel"
      return 1
    fi
    if [[ "$sync" == "none" ]] && ! _overlay_local_inventory_entry_current \
      "$path" "$src" "$rel" "$source_root_real" "$source_root_identity"; then
      _warn "  warning: $name overlay source changed after inventory: ${REPLY:-$src}"
      return 1
    fi
    if ! _overlay_destination_outside_local_sources "$rel"; then
      _warn "  warning: $name overlay destination is unsafe: ${REPLY:-$rel}"
      return 1
    fi
    # Warm convergence visits every managed link even though its parent almost
    # always exists. Keep the repair path for a missing/non-directory parent,
    # but avoid spawning both dirname and mkdir for every already-current link.
    # `-d` deliberately follows a symlink to a directory, matching mkdir -p's
    # support for user-owned parent-directory indirection documented below.
    local dst_parent="${dst%/*}"
    [[ -d "$dst_parent" ]] || mkdir -p "$dst_parent" || return 1
    local target replace_identity=''
    _overlay_record_link_target "$rel" "$name" "$path" "$sync" || return 1
    target="$REPLY"
    if [[ -L "$dst" && "$(readlink "$dst")" == "$target" ]]; then
      if [[ "$sync" == "none" ]] && ! _overlay_local_inventory_entry_current \
        "$path" "$src" "$rel" "$source_root_real" "$source_root_identity"; then
        _warn "  warning: $name overlay source changed before link acceptance: ${REPLY:-$src}"
        return 1
      fi
      if [[ -n "${_base_tracked[$rel]+x}" ]]; then
        _base_git update-index --skip-worktree "$rel" 2>/dev/null || return 1
      fi
      _overlay_record_final "$rel" "$name" "$target" || return 1
      current=$((current + 1))
      continue
    fi
    # Validate the destination before replacing it. A tracked regular file is
    # safe only when it is unchanged from the index. Filesystem-managed sources
    # require exact active or manifest authority before replacing any symlink;
    # Git overlays retain their historical replacement of untracked symlinks.
    # Everything else may be user-owned state and must survive relinking.
    # Scope: this validates the leaf $dst only. A pre-existing symlinked PARENT
    # component (e.g. the user's own `$HOME/.config -> /elsewhere`) is honored,
    # not blocked — that is the user's intentional layout, and `find` never
    # descends an overlay-shipped symlinked dir, so an overlay cannot inject one.
    if [[ -L "$dst" ]]; then
      if [[ "$sync" == "none" || -n "${_base_tracked[$rel]+x}" ]] &&
        ! _overlay_active_link_matches "$rel" &&
        ! _overlay_authority_link_matches "$rel"; then
        _warn "  skip (would replace unmanaged symlink): $rel"
        continue
      fi
      replace_identity=$(_overlay_replacement_identity "$dst") || return 1
    elif [[ -e "$dst" ]]; then
      if [[ -d "$dst" ]]; then
        _warn "  skip (directory in the way): $rel"
        continue
      fi
      if [[ -z "${_base_tracked[$rel]+x}" ]]; then
        _warn "  skip (would clobber untracked file): $rel"
        continue
      fi
      if ! _overlay_tracked_path_clean "$rel"; then
        _warn "  skip (would clobber modified tracked file): $rel"
        continue
      fi
      replace_identity=$(_overlay_replacement_identity "$dst") || return 1
    fi
    # Re-check immediately before the link mutation. A previous writer in this
    # run may have introduced a symlinked parent after the initial preflight.
    if ! _overlay_destination_outside_local_sources "$rel"; then
      _warn "  warning: $name overlay destination became unsafe: ${REPLY:-$rel}"
      return 1
    fi
    if [[ "$sync" == "none" ]] && ! _overlay_local_inventory_entry_current \
      "$path" "$src" "$rel" "$source_root_real" "$source_root_identity"; then
      _warn "  warning: $name overlay source changed before link creation: ${REPLY:-$src}"
      return 1
    fi
    _overlay_publish_link "$target" "$dst" "$replace_identity" || return 1
    linked=$((linked + 1))
    if [[ -n "${_base_tracked[$rel]+x}" ]]; then
      _base_git update-index --skip-worktree "$rel" 2>/dev/null || return 1
      if [[ "${DOT_UI_TOTAL:-0}" -eq 0 || "${DOT_VERBOSE:-0}" -eq 1 ]]; then
        _log "  linked (override): $rel"
      fi
    else
      if [[ "${DOT_UI_TOTAL:-0}" -eq 0 || "${DOT_VERBOSE:-0}" -eq 1 ]]; then
        _log "  linked: $rel"
      fi
    fi
    _overlay_record_final "$rel" "$name" "$target" || return 1
  done <"$inventory"
  if [[ "$linked" -gt 0 ]]; then
    REPLY_STATUS="changed"
    REPLY="$name overlay linked $linked"
    if [[ "${DOT_VERBOSE:-0}" -eq 1 ]]; then
      _ui_status changed "$REPLY"
    fi
  else
    REPLY_STATUS="current"
    REPLY="$name overlay current"
    if [[ "${DOT_VERBOSE:-0}" -eq 1 ]]; then
      _ui_status ok "$REPLY"
    fi
  fi
  return 0
}

# Link all active overlays and clean up stale symlinks from removed overlays.
_link_overlays() {
  local manifest="$DOT_OVERLAY_MANIFEST" pending
  if ! _preflight_local_overlays; then
    return 1
  fi
  if ! mkdir -p "${manifest%/*}"; then
    _warn "  warning: could not create overlay manifest directory: ${manifest%/*}"
    return 1
  fi
  if ! _overlay_recover_replacements; then
    _warn "  warning: unsafe overlay replacement recovery record: $REPLY"
    return 1
  fi
  if [[ (-e "$manifest" || -L "$manifest") &&
    (! -f "$manifest" || -L "$manifest") ]]; then
    _warn "  warning: overlay manifest path is not a regular file: $manifest"
    return 1
  fi

  local -A _overlay_authority_paths=()
  local -A _overlay_authority_targets=()
  local -A _overlay_current_paths=()
  local -A _overlay_inventory_files=()
  local -A _overlay_inventory_source_roots=()
  local -A _overlay_inventory_source_identities=()
  local -a OVERLAY_AUTHORITY_MANIFESTS=()
  if ! _overlay_load_authority; then
    _warn "  warning: unsafe overlay recovery manifest; refusing to link: $REPLY"
    return 1
  fi
  local adopted_legacy=0 authority
  if [[ "$DOT_OVERLAY_LEGACY_MANIFEST" != "$manifest" ]]; then
    for authority in "${OVERLAY_AUTHORITY_MANIFESTS[@]+"${OVERLAY_AUTHORITY_MANIFESTS[@]}"}"; do
      if [[ "$authority" == "$DOT_OVERLAY_LEGACY_MANIFEST" ]]; then
        adopted_legacy=1
        break
      fi
    done
  fi

  local _has_overlay_home=0
  local _overlay_total=0
  local _entry
  for _entry in "${OVERLAYS[@]+"${OVERLAYS[@]}"}"; do
    local _name _path _url _sync
    IFS='|' read -r _name _path _url _ _ _sync <<<"$_entry"
    _sync="${_sync:-git}"
    if [[ -d "$_path/home" ]] &&
      { [[ "$_sync" == "none" ]] || _overlay_checkout_matches "$_path" "$_url"; }; then
      _has_overlay_home=1
      _overlay_total=$((_overlay_total + 1))
    fi
  done
  if [[ "$_has_overlay_home" -eq 1 || "${DOT_UI_TOTAL:-0}" -gt 0 ]]; then
    if [[ "${DOT_UI_TOTAL:-0}" -gt 0 ]]; then
      _ui_stage_start "Overlays" "checking overlay links"
    else
      _ui_stage "Overlays"
    fi
    if [[ "$_has_overlay_home" -eq 0 && "${#OVERLAY_AUTHORITY_MANIFESTS[@]}" -eq 0 ]]; then
      [[ "${DOT_UI_TOTAL:-0}" -gt 0 ]] && _ui_stage_finish ok "0 overlays current"
      return 0
    fi
  fi

  # Replaces per-file _base_git ls-files --error-unmatch subprocesses.
  declare -A _base_tracked=()
  if _base_repo_exists; then
    local _tf
    while IFS= read -r _tf; do
      _base_tracked["$_tf"]=1
    done < <(_base_git ls-files 2>/dev/null)
  fi

  local _overlay_manifest_new inventory_root
  if ! _overlay_manifest_new=$(mktemp "${manifest}.tmp.XXXXXX" 2>/dev/null); then
    _warn "  warning: could not create overlay manifest temp file: ${manifest%/*}"
    return 1
  fi
  if ! chmod 600 "$_overlay_manifest_new"; then
    _warn "  warning: could not secure overlay manifest temp file: $_overlay_manifest_new"
    rm -f -- "$_overlay_manifest_new"
    return 1
  fi
  if ! inventory_root=$(mktemp -d "${manifest}.inventory.XXXXXX" 2>/dev/null) ||
    ! chmod 700 "$inventory_root" ||
    ! _overlay_prepare_inventories "$inventory_root"; then
    _warn "  warning: could not inventory overlay recovery candidates: ${manifest%/*}"
    rm -f -- "$_overlay_manifest_new"
    [[ -z "${inventory_root:-}" ]] || rm -rf -- "$inventory_root"
    return 1
  fi
  if ! _overlay_publish_pending; then
    rm -f -- "$_overlay_manifest_new"
    rm -rf -- "$inventory_root"
    return 1
  fi
  pending="$REPLY"

  # Reload after publication so stale cleanup accepts both prior owners and
  # every candidate target that may be left by an interrupted mutation phase.
  _overlay_authority_paths=()
  _overlay_authority_targets=()
  if ! _overlay_load_authority; then
    _warn "  warning: could not load published overlay recovery authority: $REPLY"
    rm -f -- "$_overlay_manifest_new"
    rm -rf -- "$inventory_root"
    return 1
  fi

  local entry
  local _overlay_done=0
  local _overlay_current=0
  local _overlay_changed=0
  local -a _overlay_changed_items=()
  for entry in "${OVERLAYS[@]+"${OVERLAYS[@]}"}"; do
    local name path url sync actual_origin expected_url status
    IFS='|' read -r name path url _ _ sync <<<"$entry"
    sync="${sync:-git}"
    [[ -d "$path/home" ]] || continue
    if [[ "$sync" == "git" ]]; then
      if ! _overlay_is_worktree "$path"; then
        _warn "  warning: $name overlay path exists but is not a Git worktree; leaving it untouched: $path"
        continue
      fi
      if ! _overlay_checkout_matches "$path" "$url"; then
        actual_origin="$REPLY"
        _overlay_effective_url "$url"
        expected_url="$REPLY"
        _overlay_origin_mismatch "$name" "$path" "$expected_url" "$actual_origin"
        continue
      fi
    fi
    _overlay_done=$((_overlay_done + 1))
    _dot_maybe_stage_progress "$name" "$_overlay_done" "$_overlay_total"
    local inventory="${_overlay_inventory_files[$name]-}"
    if [[ -z "$inventory" ]] || ! _link_overlay "$name" "$path" "$inventory" "$sync"; then
      _warn "  warning: could not link $name overlay; recovery authority retained: $pending"
      rm -f -- "$_overlay_manifest_new"
      rm -rf -- "$inventory_root"
      return 1
    fi
    status="${REPLY_STATUS:-}"
    case "$status" in
      changed)
        _overlay_changed=$((_overlay_changed + 1))
        _overlay_changed_items+=("$REPLY")
        ;;
      current)
        _overlay_current=$((_overlay_current + 1))
        ;;
      "") ;;
      *)
        _warn "  warning: unexpected link status for $name overlay: $status"
        rm -f -- "$_overlay_manifest_new"
        rm -rf -- "$inventory_root"
        return 1
        ;;
    esac
  done

  # Clean up every previously or provisionally authoritative path omitted from
  # the final manifest. Exact target validation prevents candidate
  # over-approximation from authorizing removal of user-owned paths.
  local rel dst stale_header=0
  for rel in "${!_overlay_authority_paths[@]}"; do
    [[ -z "${_overlay_current_paths[$rel]+x}" ]] || continue
    dst="$HOME/$rel"
    if [[ -L "$dst" ]]; then
      if ! _overlay_authority_link_matches "$rel"; then
        _warn "  skip (stale overlay link was replaced): $rel"
        continue
      fi
      if ! _overlay_destination_outside_local_sources "$rel"; then
        _warn "  warning: refusing to clean a link inside a local overlay source: ${REPLY:-$rel}"
        rm -f -- "$_overlay_manifest_new"
        rm -rf -- "$inventory_root"
        return 1
      fi
      if [[ "$stale_header" -eq 0 &&
        ("${DOT_UI_TOTAL:-0}" -eq 0 || "${DOT_VERBOSE:-0}" -eq 1) ]]; then
        _log_header "==> Cleaning stale overlay symlinks..."
        stale_header=1
      fi
      if ! rm -f -- "$dst"; then
        _warn "  warning: could not remove stale overlay link: $rel"
        rm -f -- "$_overlay_manifest_new"
        rm -rf -- "$inventory_root"
        return 1
      fi
      [[ "${DOT_UI_TOTAL:-0}" -eq 0 || "${DOT_VERBOSE:-0}" -eq 1 ]] && _log "  removed: $rel"
    elif [[ -e "$dst" ]]; then
      if [[ -z "${_base_tracked[$rel]+x}" ]] ||
        ! _overlay_tracked_path_clean "$rel"; then
        _warn "  skip (stale overlay path has local content): $rel"
      fi
      continue
    fi
    if [[ -n "${_base_tracked[$rel]+x}" ]]; then
      if ! _overlay_restore_tracked_path "$rel"; then
        _warn "  warning: could not restore stale base path: $rel"
        rm -f -- "$_overlay_manifest_new"
        rm -rf -- "$inventory_root"
        return 1
      fi
    fi
  done

  # Commit final state before retiring either recovery authority. Checking the
  # inode closes portable mv's "destination became a directory" behavior and
  # proves the prepared file, rather than an intervening replacement, landed at
  # the selected path.
  local final_identity
  if ! _overlay_file_identity "$_overlay_manifest_new"; then
    _warn "  warning: could not identify prepared overlay manifest: $_overlay_manifest_new"
    rm -f -- "$_overlay_manifest_new"
    rm -rf -- "$inventory_root"
    return 1
  fi
  final_identity="$REPLY"
  local manifest_exists=0
  [[ -e $manifest || -L $manifest ]] && manifest_exists=1
  if { [[ $manifest_exists -eq 0 ]] &&
    ! _dot_move_noreplace "$_overlay_manifest_new" "$manifest"; } ||
    { [[ $manifest_exists -eq 1 ]] &&
      ! _dot_move_replace_nodir "$_overlay_manifest_new" "$manifest"; }; then
    _warn "  warning: could not write overlay manifest: $manifest"
    rm -f -- "$_overlay_manifest_new"
    rm -rf -- "$inventory_root"
    return 1
  fi
  if ! _overlay_private_regular_file "$manifest" ||
    ! _overlay_file_identity "$manifest" || [[ "$REPLY" != "$final_identity" ]]; then
    _warn "  warning: overlay manifest publication could not be verified: $manifest"
    rm -f -- "$_overlay_manifest_new"
    rm -rf -- "$inventory_root"
    return 1
  fi
  rm -rf -- "$inventory_root"
  if ! rm -f -- "$pending"; then
    _warn "  warning: could not remove overlay recovery manifest: $pending"
  fi
  if [[ "$adopted_legacy" -eq 1 ]]; then
    if [[ -f "$DOT_OVERLAY_LEGACY_MANIFEST" && ! -L "$DOT_OVERLAY_LEGACY_MANIFEST" ]]; then
      rm -f -- "$DOT_OVERLAY_LEGACY_MANIFEST" ||
        _warn "  warning: could not remove adopted overlay manifest: $DOT_OVERLAY_LEGACY_MANIFEST"
    else
      _warn "  warning: adopted overlay manifest changed type; leaving it untouched: $DOT_OVERLAY_LEGACY_MANIFEST"
    fi
  fi

  if [[ "${DOT_UI_TOTAL:-0}" -gt 0 ]]; then
    local _summary _status
    local _overlay_parts=()
    [[ "$_overlay_changed" -gt 0 ]] &&
      _overlay_parts+=("$(_ui_count_phrase "$_overlay_changed" overlay overlays) changed")
    [[ "$_overlay_current" -gt 0 || "$_overlay_changed" -eq 0 ]] &&
      _overlay_parts+=("$(_ui_count_phrase "$_overlay_current" overlay overlays) current")
    _summary=$(_join_comma "${_overlay_parts[@]}")
    _status=ok
    [[ "$_overlay_changed" -gt 0 ]] && _status=changed
    _ui_stage_finish "$_status" "$_summary"
    if [[ "${DOT_VERBOSE:-0}" -eq 0 ]]; then
      local _overlay_item
      for _overlay_item in "${_overlay_changed_items[@]+"${_overlay_changed_items[@]}"}"; do
        _ui_stage_note changed "$_overlay_item"
      done
    fi
  fi
}
