# shellcheck shell=bash
# Shared helpers for safe sibling temp files.

_dot_sibling_tmp_for() {
  local dst="$1" dir base tmp
  dir="$(dirname "$dst")"
  base="$(basename "$dst")"

  mkdir -p "$dir" || return 1
  tmp=$(mktemp "$dir/${base}.tmp.XXXXXX" 2>/dev/null) || return 1
  REPLY="$tmp"
}

_dot_path_identity() {
  stat -c '%d:%i' "$1" 2>/dev/null || stat -f '%d:%i' "$1" 2>/dev/null
}

_dot_apply_tracked_file_mode() {
  local path=$1 mode=$2

  [[ -f $path && ! -L $path ]] || return 1
  case $mode in
    100644 | 100755)
      # Omitted-who symbolic modes honor the effective umask even when a
      # parent default ACL granted broader permissions at file creation.
      chmod '=rw' "$path" || return 1
      [[ $mode == 100644 ]] || chmod +x "$path"
      ;;
    *) return 1 ;;
  esac
}

_dot_apply_umask_ceiling() {
  local path=$1 ceiling=${2:-07777} identity mode mask normalized

  identity=$(_dot_path_identity "$path") || return 1
  mode=$(stat -c '%a' "$path" 2>/dev/null || stat -f '%Lp' "$path" 2>/dev/null) ||
    return 1
  mask=$(umask) || return 1
  [[ $mode != *[!0-7]* && $mask != *[!0-7]* && $ceiling != *[!0-7]* ]] ||
    return 1
  printf -v normalized '%04o' \
    "$((8#$mode & 8#$ceiling & ~(8#$mask & 0777)))"
  [[ $(_dot_path_identity "$path" 2>/dev/null || true) == "$identity" ]] ||
    return 1
  chmod "$normalized" "$path" || return 1
  [[ $(_dot_path_identity "$path" 2>/dev/null || true) == "$identity" ]]
}

_dot_apply_git_metadata_modes() {
  local root=$1 inventory path valid=1

  [[ -d $root && ! -L $root ]] || return 1
  _dot_cleanup_mktemp || return 1
  inventory=$REPLY
  if ! find "$root" -print0 >"$inventory"; then
    _dot_cleanup_remove_path "$inventory" || true
    return 1
  fi
  while IFS= read -r -d '' path; do
    if [[ (-d $path || -f $path) && ! -L $path ]]; then
      _dot_apply_umask_ceiling "$path" || {
        valid=0
        break
      }
    else
      valid=0
      break
    fi
  done <"$inventory"
  _dot_cleanup_remove_path "$inventory" || return 1
  [[ $valid -eq 1 ]]
}

# Internal Git policy must not inherit a caller's selected repository, object
# store, hash default, or configuration. Keep that shared isolation boundary in
# one place for content comparison and durable overlay identities.
_dot_sanitized_git() (
  unset GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR GIT_OBJECT_DIRECTORY
  unset GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_INDEX_FILE GIT_CONFIG
  unset GIT_CONFIG_GLOBAL GIT_CONFIG_SYSTEM GIT_CONFIG_COUNT
  unset GIT_CONFIG_PARAMETERS GIT_CONFIG_NOSYSTEM GIT_DEFAULT_HASH
  export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
  command git "$@"
)

# Read the selected Dot checkout without depending on whichever HOME or global
# Git config a client operation installs. Container workspaces can be owned by
# the host runner, so trust only this already-selected physical source root.
_dot_source_git() {
  local source_root=${DOT_SOURCE_ROOT:-$PWD}

  _dot_sanitized_git -c "safe.directory=$source_root" \
    -C "$source_root" "$@"
}

# Git is already a required Dot dependency and provides a raw, filter-free
# content hash. Keep exact-content checks on that shared baseline so minimal
# distributions do not also need a standalone `cmp` or `diff` executable.
_dot_hash_object() {
  _dot_source_git hash-object "$@"
}

_dot_hash_pair_equal() {
  local hashes=$1 first second

  [[ $hashes == *$'\n'* ]] || return 1
  first=${hashes%%$'\n'*}
  second=${hashes#*$'\n'}
  [[ $second != *$'\n'* ]] || return 1
  [[ $first =~ ^[0-9a-f]{40}$ || $first =~ ^[0-9a-f]{64}$ ]] || return 1
  [[ $second == "$first" ]]
}

_dot_files_equal() {
  local hashes

  hashes=$(_dot_hash_object --no-filters -- "$1" "$2" 2>/dev/null) || return 1
  _dot_hash_pair_equal "$hashes"
}

_dot_file_stat_mode() {
  stat -c '%a' "$1" 2>/dev/null || stat -f '%Lp' "$1" 2>/dev/null
}

_dot_file_stat_size() {
  stat -c '%s' "$1" 2>/dev/null || stat -f '%z' "$1" 2>/dev/null
}

_dot_path_uid() {
  stat -c '%u' "$1" 2>/dev/null || stat -f '%u' "$1" 2>/dev/null
}

_dot_path_nlink() {
  stat -c '%h' "$1" 2>/dev/null || stat -f '%l' "$1" 2>/dev/null
}

_dot_private_dir_validate() {
  local path=$1 mode uid
  [[ -d $path && ! -L $path ]] || return 1
  mode=$(_dot_file_stat_mode "$path") || return 1
  uid=$(_dot_path_uid "$path") || return 1
  [[ $mode == 700 && $uid == "$(id -u)" ]]
}

_dot_private_control_file_validate() {
  local path=$1 mode uid nlink
  [[ -f $path && ! -L $path ]] || return 1
  mode=$(_dot_file_stat_mode "$path") || return 1
  uid=$(_dot_path_uid "$path") || return 1
  nlink=$(_dot_path_nlink "$path") || return 1
  [[ $mode == 600 && $uid == "$(id -u)" && $nlink == 1 ]]
}

_dot_file_digest() {
  _dot_hash_object --no-filters -- "$1" 2>/dev/null
}

_dot_file_text_digest() {
  printf '%s' "$1" | _dot_hash_object --stdin 2>/dev/null
}

# Resolve a logical destination once to a stable physical parent. Generation
# tokens bind both names so replacing a parent symlink cannot redirect a later
# conditional update into a different directory.
_dot_file_target_resolve() {
  local path=$1 dir base physical parent_identity path_digest

  [[ $path == /* && $path != *$'\n'* && $path != *$'\r'* &&
    $path != *$'\t'* ]] || return 1
  dir=${path%/*}
  base=${path##*/}
  [[ -n $base && $base != . && $base != .. ]] || return 1
  [[ -n $dir ]] || dir=/
  physical=$(cd -P -- "$dir" 2>/dev/null && pwd -P) || return 1
  [[ -d $physical && ! -L $physical ]] || return 1
  parent_identity=$(_dot_path_identity "$physical") || return 1
  path_digest=$(_dot_file_text_digest "$physical/$base") || return 1

  DOT_FILE_TARGET_PARENT=$physical
  DOT_FILE_TARGET_PATH=$physical/$base
  DOT_FILE_TARGET_PARENT_ID=$parent_identity
  DOT_FILE_TARGET_PATH_DIGEST=$path_digest
  DOT_FILE_TARGET_TRANSACTION=$physical/.$base.dot-file-transaction-v1
}

_dot_file_signature() {
  local path=$1 identity mode size digest

  [[ -f $path && ! -L $path ]] || return 1
  identity=$(_dot_path_identity "$path") || return 1
  mode=$(_dot_file_stat_mode "$path") || return 1
  size=$(_dot_file_stat_size "$path") || return 1
  digest=$(_dot_file_digest "$path") || return 1
  printf '%s|%s|%s|%s\n' "${identity/:/|}" "$mode" "$size" "$digest"
}

_dot_file_generation_raw() {
  local path=$1 payload checksum signature state

  _dot_file_target_resolve "$path" || return 1
  if [[ -e $DOT_FILE_TARGET_PATH || -L $DOT_FILE_TARGET_PATH ]]; then
    [[ -f $DOT_FILE_TARGET_PATH && ! -L $DOT_FILE_TARGET_PATH ]] || return 1
    signature=$(_dot_file_signature "$DOT_FILE_TARGET_PATH") || return 1
    state="file|$signature"
  else
    state='absent|-|-|-|-|-'
  fi
  payload="v1|$DOT_FILE_TARGET_PATH_DIGEST|${DOT_FILE_TARGET_PARENT_ID/:/|}|$state"
  checksum=$(_dot_file_text_digest "dot-file-generation-v1|$payload") || return 1
  printf '%s|%s\n' "$payload" "$checksum"
}

_dot_file_generation_validate() {
  local token=$1 version path_digest parent_device parent_inode state
  local leaf_device leaf_inode mode size digest checksum extra payload expected

  [[ $token != *$'\n'* && $token != *$'\r'* && $token != *$'\t'* ]] || return 1
  IFS='|' read -r version path_digest parent_device parent_inode state \
    leaf_device leaf_inode mode size digest checksum extra <<<"$token"
  [[ -z $extra && $version == v1 &&
    $path_digest =~ ^[0-9a-f]{40}$|^[0-9a-f]{64}$ &&
    $parent_device =~ ^[0-9]+$ && $parent_inode =~ ^[0-9]+$ ]] || return 1
  case $state in
    absent)
      [[ $leaf_device == - && $leaf_inode == - && $mode == - &&
        $size == - && $digest == - ]] || return 1
      ;;
    file)
      [[ $leaf_device =~ ^[0-9]+$ && $leaf_inode =~ ^[0-9]+$ &&
        $mode =~ ^[0-7]+$ && $size =~ ^[0-9]+$ &&
        $digest =~ ^[0-9a-f]{40}$|^[0-9a-f]{64}$ ]] || return 1
      ;;
    *) return 1 ;;
  esac
  [[ $checksum =~ ^[0-9a-f]{40}$|^[0-9a-f]{64}$ ]] || return 1
  payload=${token%|*}
  expected=$(_dot_file_text_digest "dot-file-generation-v1|$payload") || return 1
  [[ $checksum == "$expected" ]] || return 1

  DOT_FILE_GENERATION_STATE=$state
  DOT_FILE_GENERATION_PATH_DIGEST=$path_digest
  DOT_FILE_GENERATION_PARENT_ID="$parent_device:$parent_inode"
  DOT_FILE_GENERATION_SIGNATURE="$leaf_device|$leaf_inode|$mode|$size|$digest"
}

_dot_file_transaction_lock_valid() {
  [[ ${DOT_TEST:-0} == 1 || -n ${DOT_UPDATE_LOCK_TOKEN:-} ]]
}

_dot_file_transaction_record_read() {
  local transaction=$1 record version operation phase expected candidate extra
  record=$transaction/record
  _dot_private_control_file_validate "$record" || return 1
  IFS=$'\t' read -r version operation phase expected candidate extra <"$record" ||
    return 1
  [[ -z $extra && $version == v1 ]] || return 1
  [[ $operation == replace || $operation == remove ]] || return 1
  [[ $phase == prepared || $phase == quarantined || $phase == committed ]] ||
    return 1
  _dot_file_generation_validate "$expected" || return 1
  if [[ $operation == replace ]]; then
    [[ $candidate =~ ^[0-9]+\|[0-9]+\|[0-7]+\|[0-9]+\|[0-9a-f]{40}$ ||
      $candidate =~ ^[0-9]+\|[0-9]+\|[0-7]+\|[0-9]+\|[0-9a-f]{64}$ ]] ||
      return 1
  else
    [[ $candidate == - ]] || return 1
  fi
  DOT_FILE_TRANSACTION_OPERATION=$operation
  DOT_FILE_TRANSACTION_PHASE=$phase
  DOT_FILE_TRANSACTION_EXPECTED=$expected
  DOT_FILE_TRANSACTION_CANDIDATE=$candidate
}

_dot_file_transaction_record_write() {
  local transaction=$1 operation=$2 phase=$3 expected=$4 candidate=$5
  local record=$transaction/record next=$transaction/record.next

  [[ -d $transaction && ! -L $transaction ]] || return 1
  [[ ! -e $next && ! -L $next ]] || return 1
  (
    umask 077
    printf 'v1\t%s\t%s\t%s\t%s\n' \
      "$operation" "$phase" "$expected" "$candidate" >"$next"
  ) || return 1
  chmod 600 "$next" || {
    rm -f -- "$next"
    return 1
  }
  _dot_private_control_file_validate "$next" || {
    rm -f -- "$next"
    return 1
  }
  if [[ -e $record || -L $record ]]; then
    [[ -f $record && ! -L $record ]] || return 1
    _dot_move_replace_nodir "$next" "$record"
  else
    _dot_move_noreplace "$next" "$record"
  fi
}

_dot_file_transaction_entries_validate() {
  local transaction=$1 path base

  _dot_private_dir_validate "$transaction" || return 1
  for path in "$transaction"/* "$transaction"/.[!.]* "$transaction"/..?*; do
    [[ -e $path || -L $path ]] || continue
    base=${path##*/}
    case $base in
      record | record.next)
        _dot_private_control_file_validate "$path" || return 1
        ;;
      candidate | previous)
        [[ -f $path && ! -L $path ]] || return 1
        ;;
      *) return 1 ;;
    esac
  done
}

_dot_file_transaction_discard_private() {
  local transaction=$1 path

  _dot_file_transaction_entries_validate "$transaction" || return 1
  for path in "$transaction/candidate" "$transaction/previous" \
    "$transaction/record.next" "$transaction/record"; do
    if [[ -e $path || -L $path ]]; then
      [[ -f $path && ! -L $path ]] || return 1
      rm -f -- "$path" || return 1
    fi
  done
  rmdir "$transaction"
}

_dot_file_transaction_cleanup() {
  local transaction=$1 cleanup_root path
  _dot_file_transaction_entries_validate "$transaction" || return 1
  _dot_private_control_file_validate "$transaction/record" || return 1
  # Retire config-bearing files while the authoritative journal remains at the
  # deterministic name. A crash can therefore be retried. Only after those
  # bytes are gone do we rename the record-only directory out of the active
  # namespace, avoiding both a blocking empty directory and orphaned backups.
  for path in "$transaction/candidate" "$transaction/previous" \
    "$transaction/record.next"; do
    if [[ -e $path || -L $path ]]; then
      [[ -f $path && ! -L $path ]] || return 1
      rm -f -- "$path" || return 1
    fi
  done
  cleanup_root=$(mktemp -d "${transaction}.cleanup.XXXXXX") || return 1
  if ! chmod 700 "$cleanup_root" ||
    ! _dot_private_dir_validate "$cleanup_root"; then
    rmdir "$cleanup_root" 2>/dev/null || true
    return 1
  fi
  if ! _dot_move_noreplace "$transaction" "$cleanup_root/transaction"; then
    rmdir "$cleanup_root" 2>/dev/null || true
    return 1
  fi
  _dot_file_transaction_discard_private "$cleanup_root/transaction" || return 1
  rmdir "$cleanup_root"
}

_dot_file_transaction_restore_previous() {
  local previous=$1 destination=$2

  if [[ -e $destination || -L $destination ]]; then
    return 1
  fi
  _dot_move_noreplace "$previous" "$destination"
}

# Recover a transaction left by abrupt process termination. The record's
# atomic phase change is the remove commit point; replacement publication is
# independently recognizable from the recorded candidate inode and digest.
_dot_file_transaction_recover() {
  local destination=$1 transaction=$2 previous candidate live_signature=''
  local previous_signature='' target_path_digest=$DOT_FILE_TARGET_PATH_DIGEST
  local target_parent_id=$DOT_FILE_TARGET_PARENT_ID expected_signature

  [[ -e $transaction || -L $transaction ]] || return 0
  _dot_file_transaction_entries_validate "$transaction" || return 1
  _dot_file_transaction_record_read "$transaction" || return 1
  [[ $DOT_FILE_GENERATION_PATH_DIGEST == "$target_path_digest" &&
    $DOT_FILE_GENERATION_PARENT_ID == "$target_parent_id" ]] || return 1
  expected_signature=$DOT_FILE_GENERATION_SIGNATURE
  previous=$transaction/previous
  candidate=$transaction/candidate

  if [[ -e $destination || -L $destination ]]; then
    [[ -f $destination && ! -L $destination ]] || return 1
    live_signature=$(_dot_file_signature "$destination") || return 1
  fi
  if [[ -e $previous || -L $previous ]]; then
    [[ -f $previous && ! -L $previous ]] || return 1
    previous_signature=$(_dot_file_signature "$previous") || return 1
  fi
  if [[ -n $previous_signature ]] &&
    [[ $DOT_FILE_GENERATION_STATE != file ||
      $previous_signature != "$expected_signature" ]]; then
    # A replacement that won the final pre-mutation race was quarantined. Put
    # it back when possible; if another writer already filled the name, retain
    # both versions and fail closed for explicit operator recovery.
    if [[ -z $live_signature ]]; then
      _dot_file_transaction_restore_previous "$previous" "$destination" ||
        return 1
      _dot_file_transaction_cleanup "$transaction"
    else
      return 1
    fi
    return
  fi

  case $DOT_FILE_TRANSACTION_PHASE in
    prepared)
      if [[ -n $previous_signature && -z $live_signature ]]; then
        _dot_file_transaction_restore_previous "$previous" "$destination" ||
          return 1
      fi
      ;;
    quarantined)
      if [[ $DOT_FILE_TRANSACTION_OPERATION == replace &&
        -n $live_signature &&
        $live_signature == "$DOT_FILE_TRANSACTION_CANDIDATE" ]]; then
        : # Publication committed before the phase record advanced.
      elif [[ -n $previous_signature && -z $live_signature ]]; then
        _dot_file_transaction_restore_previous "$previous" "$destination" ||
          return 1
      fi
      ;;
    committed) : ;;
  esac
  _dot_file_transaction_cleanup "$transaction"
}

_dot_file_generation() {
  local path=$1

  _dot_file_transaction_lock_valid || return 1
  _dot_file_target_resolve "$path" || return 1
  _dot_file_transaction_recover \
    "$DOT_FILE_TARGET_PATH" "$DOT_FILE_TARGET_TRANSACTION" || return 1
  _dot_file_generation_raw "$path"
}

_dot_file_transaction_prepare() {
  local operation=$1 source=$2 destination=$3 expected=$4
  local current candidate=- moved_candidate transaction transaction_init

  _dot_file_transaction_lock_valid || return 1
  _dot_file_generation_validate "$expected" || return 1
  _dot_file_target_resolve "$destination" || return 1
  transaction=$DOT_FILE_TARGET_TRANSACTION
  _dot_file_transaction_recover "$DOT_FILE_TARGET_PATH" "$transaction" ||
    return 1
  current=$(_dot_file_generation_raw "$destination") || return 1
  [[ $current == "$expected" ]] || return 1

  if [[ $operation == replace ]]; then
    [[ -f $source && ! -L $source ]] || return 1
    _dot_file_target_resolve "$source" || return 1
    [[ $DOT_FILE_TARGET_PARENT == "${transaction%/*}" ]] || return 1
    candidate=$(_dot_file_signature "$source") || return 1
    _dot_file_target_resolve "$destination" || return 1
  fi

  transaction_init=$(mktemp -d "${transaction}.init.XXXXXX") || return 1
  if ! chmod 700 "$transaction_init" ||
    ! _dot_private_dir_validate "$transaction_init"; then
    rmdir "$transaction_init" 2>/dev/null || true
    return 1
  fi
  if ! _dot_file_transaction_record_write \
    "$transaction_init" "$operation" prepared "$expected" "$candidate"; then
    _dot_file_transaction_discard_private "$transaction_init" || true
    return 1
  fi
  if ! _dot_move_noreplace "$transaction_init" "$transaction"; then
    _dot_file_transaction_discard_private "$transaction_init" || true
    return 1
  fi
  if [[ $operation == replace ]]; then
    if ! _dot_move_noreplace "$source" "$transaction/candidate"; then
      _dot_file_transaction_cleanup "$transaction" || true
      return 1
    fi
    moved_candidate=$(_dot_file_signature "$transaction/candidate" 2>/dev/null ||
      true)
    if [[ $moved_candidate != "$candidate" ]]; then
      _dot_move_noreplace "$transaction/candidate" "$source" 2>/dev/null || true
      _dot_file_transaction_cleanup "$transaction" || true
      return 1
    fi
  fi

  DOT_FILE_TRANSACTION_PATH=$transaction
  DOT_FILE_TRANSACTION_DESTINATION=$DOT_FILE_TARGET_PATH
  DOT_FILE_TRANSACTION_SOURCE=$source
  DOT_FILE_TRANSACTION_EXPECTED=$expected
  DOT_FILE_TRANSACTION_CANDIDATE=$candidate
}

_dot_file_transaction_quarantine() {
  local transaction=$DOT_FILE_TRANSACTION_PATH
  local destination=$DOT_FILE_TRANSACTION_DESTINATION current previous_signature

  current=$(_dot_file_generation_raw "$destination") || return 1
  if [[ $current != "$DOT_FILE_TRANSACTION_EXPECTED" ]]; then
    if [[ -e $transaction/candidate ]]; then
      _dot_move_noreplace "$transaction/candidate" \
        "$DOT_FILE_TRANSACTION_SOURCE" || return 1
    fi
    _dot_file_transaction_cleanup "$transaction" || return 1
    return 1
  fi
  _dot_file_generation_validate "$DOT_FILE_TRANSACTION_EXPECTED" || return 1
  if [[ $DOT_FILE_GENERATION_STATE == file ]]; then
    _dot_move_noreplace "$destination" "$transaction/previous" || return 1
    previous_signature=$(_dot_file_signature "$transaction/previous") || return 1
    if [[ $previous_signature != "$DOT_FILE_GENERATION_SIGNATURE" ]]; then
      _dot_file_transaction_restore_previous \
        "$transaction/previous" "$destination" || return 1
      return 1
    fi
  else
    [[ ! -e $destination && ! -L $destination ]] || return 1
  fi
  _dot_file_transaction_record_write "$transaction" \
    "$DOT_FILE_TRANSACTION_OPERATION" quarantined \
    "$DOT_FILE_TRANSACTION_EXPECTED" "$DOT_FILE_TRANSACTION_CANDIDATE"
}

_dot_commit_tmp_if_generation() {
  local source=$1 destination=$2 expected=$3 transaction

  _dot_file_transaction_prepare replace "$source" "$destination" "$expected" ||
    return 1
  DOT_FILE_TRANSACTION_OPERATION=replace
  transaction=$DOT_FILE_TRANSACTION_PATH
  if ! _dot_file_transaction_quarantine; then
    _dot_file_transaction_recover \
      "$DOT_FILE_TRANSACTION_DESTINATION" "$transaction" || true
    return 1
  fi
  if ! _dot_move_noreplace \
    "$transaction/candidate" "$DOT_FILE_TRANSACTION_DESTINATION"; then
    _dot_file_transaction_recover \
      "$DOT_FILE_TRANSACTION_DESTINATION" "$transaction" || true
    return 1
  fi
  if ! _dot_file_transaction_record_write "$transaction" replace committed \
    "$DOT_FILE_TRANSACTION_EXPECTED" "$DOT_FILE_TRANSACTION_CANDIDATE"; then
    return 1
  fi
  _dot_file_transaction_cleanup "$transaction"
}

_dot_remove_if_generation() {
  local destination=$1 expected=$2 transaction

  _dot_file_transaction_prepare remove '' "$destination" "$expected" || return 1
  DOT_FILE_TRANSACTION_OPERATION=remove
  transaction=$DOT_FILE_TRANSACTION_PATH
  if ! _dot_file_transaction_quarantine; then
    _dot_file_transaction_recover \
      "$DOT_FILE_TRANSACTION_DESTINATION" "$transaction" || true
    return 1
  fi
  [[ ! -e $DOT_FILE_TRANSACTION_DESTINATION &&
    ! -L $DOT_FILE_TRANSACTION_DESTINATION ]] || return 1
  _dot_file_transaction_record_write "$transaction" remove committed \
    "$DOT_FILE_TRANSACTION_EXPECTED" - || return 1
  _dot_file_transaction_cleanup "$transaction"
}

_dot_stdin_matches_file() {
  local hashes

  hashes=$(_dot_hash_object --no-filters --stdin -- "$1" 2>/dev/null) || return 1
  _dot_hash_pair_equal "$hashes"
}

_dot_detect_move_tool() {
  local probe source moved mv_bin
  mv_bin=$(type -P mv 2>/dev/null) || return 1
  if [[ -n ${DOT_MOVE_BIN:-} && -n ${DOT_MOVE_MODE:-} &&
    -x ${DOT_MOVE_BIN:-} && $DOT_MOVE_BIN == "$mv_bin" ]]; then
    return 0
  fi
  unset DOT_MOVE_BIN DOT_MOVE_MODE
  probe=$(mktemp -d "${TMPDIR:-/tmp}/dot-move-tools.XXXXXX") || return 1
  source=$probe/source
  moved=$probe/moved
  mkdir "$source"
  if "$mv_bin" -nT -- "$source" "$moved" 2>/dev/null &&
    [[ -d $moved && ! -e $source ]]; then
    DOT_MOVE_MODE=T
  else
    rm -rf "$source" "$moved"
    mkdir "$source"
    if "$mv_bin" -nh "$source" "$moved" 2>/dev/null &&
      [[ -d $moved && ! -e $source ]]; then
      DOT_MOVE_MODE=h
    else
      rm -rf "$source" "$moved" "$probe"
      return 1
    fi
  fi
  rm -rf "$source" "$moved"
  rmdir "$probe" 2>/dev/null || true
  DOT_MOVE_BIN=$mv_bin
}

# Move one prepared sibling into an absent destination without replacing a
# late file, symlink, or empty directory. BSD mv can briefly nest the source in
# a late directory; exact inode recovery moves only that source back out.
_dot_move_noreplace() {
  local source=$1 target=$2 identity target_identity nested
  _dot_detect_move_tool || return 1
  identity=$(_dot_path_identity "$source") || return 1
  if [[ $DOT_MOVE_MODE == T ]]; then
    "$DOT_MOVE_BIN" -nT -- "$source" "$target" 2>/dev/null || true
  else
    "$DOT_MOVE_BIN" -nh "$source" "$target" 2>/dev/null || true
  fi
  target_identity=$(_dot_path_identity "$target" 2>/dev/null || true)
  [[ $target_identity == "$identity" ]] && return 0
  nested=$target/${source##*/}
  if [[ -d $target && ! -L $target &&
    $(_dot_path_identity "$nested" 2>/dev/null || true) == "$identity" ]]; then
    mv "$nested" "$source" 2>/dev/null || return 1
  fi
  return 1
}

# Replace a known engine-owned non-directory destination without ever leaving
# the prepared source nested in a late directory or a symlink-to-directory.
# Callers must validate any existing destination before invoking this helper.
_dot_move_replace_nodir() {
  local source=$1 target=$2 identity target_identity nested
  _dot_detect_move_tool || return 1
  identity=$(_dot_path_identity "$source") || return 1
  if [[ $DOT_MOVE_MODE == T ]]; then
    "$DOT_MOVE_BIN" -fT -- "$source" "$target" 2>/dev/null || true
  else
    "$DOT_MOVE_BIN" -fh "$source" "$target" 2>/dev/null || true
  fi
  target_identity=$(_dot_path_identity "$target" 2>/dev/null || true)
  [[ $target_identity == "$identity" ]] && return 0
  nested=$target/${source##*/}
  if [[ -d $target && ! -L $target &&
    $(_dot_path_identity "$nested" 2>/dev/null || true) == "$identity" ]]; then
    mv "$nested" "$source" 2>/dev/null || return 1
  fi
  return 1
}

# Publish a prepared regular file without following or nesting into a late
# directory/symlink. Existing regular output may be replaced only while its
# no-follow identity still matches the generation validated here; an absent
# destination uses exclusive publication and therefore preserves every late
# winner.
_dot_publish_prepared_regular() {
  local source=$1 target=$2 expected_identity=''
  [[ -f $source && ! -L $source ]] || return 1
  if [[ -e $target || -L $target ]]; then
    [[ -f $target && ! -L $target ]] || return 1
    expected_identity=$(_dot_path_identity "$target") || return 1
    [[ $(_dot_path_identity "$target" 2>/dev/null || true) == "$expected_identity" ]] ||
      return 1
    _dot_move_replace_nodir "$source" "$target"
  else
    _dot_move_noreplace "$source" "$target"
  fi
}
