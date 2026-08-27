# shellcheck shell=bash
# One-use, data-only authorization context for isolated extension workers.

_DOT_OVERLAY_CONTEXT_MAGIC=DOT_OVERLAY_CONTEXT
_DOT_OVERLAY_CONTEXT_VERSION=1
_DOT_OVERLAY_CONTEXT_MAX_BYTES=1048576
_DOT_OVERLAY_CONTEXT_MAX_RECORDS=256

_dot_overlay_context_error() {
  printf 'dot: overlay context: %s\n' "$*" >&2
  return 1
}

# Overlay descriptors and worker contexts share one field representation.
# Reject the record delimiter plus every representable C0 control byte and DEL
# before a value crosses either boundary. Bash strings cannot contain NUL; the
# NUL-framed decoder validates that byte separately as structure.
_dot_overlay_field_safe() {
  local value=${1:-}
  [[ $value != *'|'* ]] || return 1
  LC_ALL=C printf '%s' "$value" | LC_ALL=C od -An -t u1 |
    awk '{ for (i = 1; i <= NF; i++) if ($i < 32 || $i == 127) exit 1 }'
}

_dot_overlay_context_stat() {
  local path=$1 output
  if output=$(command stat -c '%u %a %h %d %i' "$path" 2>/dev/null); then
    :
  elif output=$(command stat -f '%u %Lp %l %d %i' "$path" 2>/dev/null); then
    :
  else
    return 1
  fi
  read -r REPLY_UID REPLY_MODE REPLY_LINKS REPLY_DEV REPLY_INO <<<"$output"
  [[ $REPLY_UID == "$EUID" && $REPLY_MODE != *[!0-7]* ]]
}

_dot_overlay_context_directory_safe() {
  local path=$1 REPLY_UID REPLY_MODE REPLY_LINKS REPLY_DEV REPLY_INO
  [[ -d $path && ! -L $path ]] || return 1
  _dot_overlay_context_stat "$path" || return 1
  (((8#$REPLY_MODE & 077) == 0))
}

_dot_overlay_context_file_safe() {
  local path=$1 size mtime now
  # shellcheck disable=SC2034 # Shared stat helper publishes the complete identity tuple.
  local REPLY_UID REPLY_MODE REPLY_LINKS REPLY_DEV REPLY_INO
  [[ -f $path && ! -L $path ]] || return 1
  _dot_overlay_context_stat "$path" || return 1
  [[ $REPLY_MODE == 600 && $REPLY_LINKS == 1 ]] || return 1
  size=$(LC_ALL=C wc -c <"$path" 2>/dev/null | tr -d '[:space:]') || return 1
  [[ $size =~ ^[0-9]+$ && $size -le $_DOT_OVERLAY_CONTEXT_MAX_BYTES ]] || return 1
  mtime=$(command stat -c '%Y' "$path" 2>/dev/null ||
    command stat -f '%m' "$path" 2>/dev/null) || return 1
  now=$(date +%s) || return 1
  [[ $mtime =~ ^[0-9]+$ && $now =~ ^[0-9]+$ ]] || return 1
  ((mtime <= now + 5 && now - mtime <= 300))
}

_dot_overlay_context_lsof() {
  local fd=$1 lsof_bin process_id=${BASHPID:-$$}
  for lsof_bin in /usr/sbin/lsof /usr/bin/lsof; do
    [[ -x $lsof_bin ]] || continue
    command "$lsof_bin" -a -p "$process_id" -d "$fd" -FDiku 2>/dev/null
    return
  done
  return 1
}

# Read metadata from an already-open descriptor. Linux exposes the underlying
# file through procfs. Darwin's devfs entry may instead report synthetic
# metadata, so accept it only when the identity matches and otherwise use the
# system lsof's direct descriptor report. The path is revalidated immediately
# before unlink, so the lsof fallback carries forward its verified 0600 mode.
_dot_overlay_context_open_file_stat() {
  local fd=$1 expected_dev=$2 expected_ino=$3 output line raw_dev
  local fd_uid='' fd_mode='' fd_links='' fd_dev='' fd_ino=''

  if output=$(command stat -Lc '%u %a %h %d %i' "/proc/self/fd/$fd" 2>/dev/null) ||
    output=$(command stat -Lf '%u %Lp %l %d %i' "/dev/fd/$fd" 2>/dev/null); then
    read -r fd_uid fd_mode fd_links fd_dev fd_ino <<<"$output"
    if [[ $fd_uid == "$EUID" && $fd_mode == 600 &&
      $fd_dev == "$expected_dev" && $fd_ino == "$expected_ino" ]]; then
      REPLY_UID=$fd_uid
      REPLY_MODE=$fd_mode
      REPLY_LINKS=$fd_links
      REPLY_DEV=$fd_dev
      REPLY_INO=$fd_ino
      return 0
    fi
  fi

  output=$(_dot_overlay_context_lsof "$fd") || return 1
  fd_uid=''
  fd_links=''
  fd_dev=''
  fd_ino=''
  while IFS= read -r line; do
    case $line in
      D*)
        raw_dev=${line#D}
        printf -v fd_dev '%d' "$raw_dev" 2>/dev/null || return 1
        ;;
      i*) fd_ino=${line#i} ;;
      k*) fd_links=${line#k} ;;
      u*) fd_uid=${line#u} ;;
    esac
  done <<<"$output"
  [[ $fd_uid == "$EUID" && $fd_links =~ ^[0-9]+$ &&
    $fd_dev == "$expected_dev" && $fd_ino == "$expected_ino" ]] || return 1
  REPLY_UID=$fd_uid
  REPLY_MODE=600
  REPLY_LINKS=$fd_links
  REPLY_DEV=$fd_dev
  REPLY_INO=$fd_ino
}

_dot_overlay_context_absolute_canonical() {
  local path=$1
  _dot_overlay_field_safe "$path" || return 1
  case $path in
    '' | / | */ | *//* | */./* | */. | */../* | */..)
      return 1
      ;;
    /*) ;;
    *) return 1 ;;
  esac
}

_dot_overlay_record_validate() {
  local record=$1 name path url descriptor optional sync extra
  IFS='|' read -r name path url descriptor optional sync extra <<<"$record"
  [[ -z $extra && -n $sync ]] || return 1
  _dot_overlay_field_safe "$name" &&
    _dot_overlay_field_safe "$url" &&
    _dot_overlay_field_safe "$optional" &&
    _dot_overlay_field_safe "$sync" || return 1
  [[ $name =~ ^[a-z][a-z0-9-]*$ && $name != dotfiles ]] || return 1
  _dot_overlay_context_absolute_canonical "$path" || return 1
  _dot_overlay_context_absolute_canonical "$descriptor" || return 1
  [[ $descriptor == *.conf ]] || return 1
  local descriptor_name=${descriptor##*/}
  descriptor_name=${descriptor_name%.conf}
  descriptor_name=${descriptor_name%.local}
  [[ $descriptor_name =~ ^[0-9]+-(.+)$ ]] && descriptor_name=${BASH_REMATCH[1]}
  [[ $descriptor_name == "$name" ]] || return 1
  case $optional in true | false) ;; *) return 1 ;; esac
  case $sync in
    git)
      [[ -n $url ]] || return 1
      [[ $path == "$HOME/.dotfiles-$name" ]] || return 1
      ;;
    none)
      [[ -z $url && $optional == false ]] || return 1
      ;;
    *) return 1 ;;
  esac
}

_dot_overlay_context_matrix_valid() {
  local mode=$1 set_kind=$2 stage=$3
  case $mode:$set_kind:$stage in
    pre-sync:eligible:prepare | pre-sync:eligible:reconcile | \
      merge:active:none | doctor:active:none)
      return 0
      ;;
  esac
  return 1
}

_dot_overlay_context_token() {
  LC_ALL=C od -An -N32 -tx1 /dev/urandom 2>/dev/null | tr -d ' \n'
}

_dot_overlay_context_create() {
  local directory=$1 mode=$2 set_kind=$3 stage=$4
  local token path record name
  shift 4
  local -a records=("$@")
  local -A seen=()

  [[ $directory == /* ]] ||
    _dot_overlay_context_error "context directory is not absolute: $directory" || return
  _dot_overlay_context_directory_safe "$directory" ||
    _dot_overlay_context_error "unsafe context directory: $directory" || return
  _dot_overlay_context_matrix_valid "$mode" "$set_kind" "$stage" ||
    _dot_overlay_context_error "invalid mode/set/stage: $mode/$set_kind/$stage" || return
  ((${#records[@]} <= _DOT_OVERLAY_CONTEXT_MAX_RECORDS)) ||
    _dot_overlay_context_error 'too many overlay records' || return
  for record in "${records[@]+"${records[@]}"}"; do
    _dot_overlay_record_validate "$record" ||
      _dot_overlay_context_error 'invalid overlay record' || return
    name=${record%%|*}
    [[ -z ${seen[$name]+x} ]] ||
      _dot_overlay_context_error "duplicate overlay record: $name" || return
    seen["$name"]=1
  done
  token=$(_dot_overlay_context_token) || return 1
  [[ $token =~ ^[0-9a-f]{64}$ ]] || return 1
  path=$(mktemp "$directory/.dot-overlay-context.XXXXXXXX") || return 1
  chmod 0600 "$path" || {
    rm -f -- "$path"
    return 1
  }
  {
    printf '%s\0%s\0%s\0%s\0%s\0%s\0' \
      "$_DOT_OVERLAY_CONTEXT_MAGIC" "$_DOT_OVERLAY_CONTEXT_VERSION" \
      "$token" "$mode" "$set_kind" "$stage"
    printf '%s\0' "${#records[@]}"
    for record in "${records[@]+"${records[@]}"}"; do
      local name path_value url descriptor optional sync
      IFS='|' read -r name path_value url descriptor optional sync <<<"$record"
      printf '%s\0%s\0%s\0%s\0%s\0%s\0' \
        "$name" "$path_value" "$url" "$descriptor" "$optional" "$sync"
    done
  } >"$path" || {
    rm -f -- "$path"
    return 1
  }
  _dot_overlay_context_file_safe "$path" || {
    rm -f -- "$path"
    return 1
  }
  # shellcheck disable=SC2034 # Published to the coordinator call site.
  REPLY_PATH=$path
  # shellcheck disable=SC2034 # Published to the coordinator call site.
  REPLY_TOKEN=$token
}

_dot_overlay_context_consume() {
  local context=$1 token=$2 expected_mode=$3
  local parent field='' magic version stored_token mode set_kind stage count
  local index offset=0 record name path url descriptor optional sync
  local context_fd path_dev path_ino fd_uid fd_mode fd_links fd_dev fd_ino
  local REPLY_UID REPLY_MODE REPLY_LINKS REPLY_DEV REPLY_INO
  local -a fields=() decoded=()
  local -A seen=()

  [[ $# -eq 3 ]] || return 2
  [[ $context == /* ]] || return 1
  parent=${context%/*}
  _dot_overlay_context_directory_safe "$parent" || return 1
  _dot_overlay_context_file_safe "$context" || return 1
  _dot_overlay_context_stat "$context" || return 1
  path_dev=$REPLY_DEV
  path_ino=$REPLY_INO
  exec {context_fd}<"$context" || return 1
  _dot_overlay_context_file_safe "$context" || {
    exec {context_fd}<&-
    return 1
  }
  _dot_overlay_context_stat "$context" || {
    exec {context_fd}<&-
    return 1
  }
  [[ $REPLY_DEV == "$path_dev" && $REPLY_INO == "$path_ino" ]] || {
    exec {context_fd}<&-
    return 1
  }
  _dot_overlay_context_open_file_stat "$context_fd" "$path_dev" "$path_ino" || {
    exec {context_fd}<&-
    return 1
  }
  fd_uid=$REPLY_UID
  fd_mode=$REPLY_MODE
  fd_links=$REPLY_LINKS
  fd_dev=$REPLY_DEV
  fd_ino=$REPLY_INO
  if [[ $fd_uid != "$EUID" || $fd_mode != 600 || $fd_links != 1 ||
    $fd_dev != "$path_dev" || $fd_ino != "$path_ino" ]]; then
    exec {context_fd}<&-
    return 1
  fi
  # Remove the pathname before parsing the already-bound descriptor so
  # replacement or reuse cannot grant later authority.
  rm -f -- "$context" || {
    exec {context_fd}<&-
    return 1
  }
  _dot_overlay_context_open_file_stat "$context_fd" "$path_dev" "$path_ino" || {
    exec {context_fd}<&-
    return 1
  }
  fd_uid=$REPLY_UID
  fd_mode=$REPLY_MODE
  fd_links=$REPLY_LINKS
  fd_dev=$REPLY_DEV
  fd_ino=$REPLY_INO
  if [[ $fd_uid != "$EUID" || $fd_mode != 600 || $fd_links != 0 ||
    $fd_dev != "$path_dev" || $fd_ino != "$path_ino" ]]; then
    exec {context_fd}<&-
    return 1
  fi
  while IFS= read -r -d '' field; do
    fields+=("$field")
  done <&"$context_fd"
  exec {context_fd}<&-
  [[ -z $field ]] || {
    return 1
  }
  [[ ${#fields[@]} -ge 7 ]] || return 1
  magic=${fields[0]}
  version=${fields[1]}
  stored_token=${fields[2]}
  mode=${fields[3]}
  set_kind=${fields[4]}
  stage=${fields[5]}
  count=${fields[6]}
  [[ $magic == "$_DOT_OVERLAY_CONTEXT_MAGIC" &&
    $version == "$_DOT_OVERLAY_CONTEXT_VERSION" &&
    $stored_token == "$token" && $mode == "$expected_mode" ]] || return 1
  [[ $token =~ ^[0-9a-f]{64}$ && $count =~ ^(0|[1-9][0-9]*)$ &&
    $count -le $_DOT_OVERLAY_CONTEXT_MAX_RECORDS ]] || return 1
  _dot_overlay_context_matrix_valid "$mode" "$set_kind" "$stage" || return 1
  [[ ${#fields[@]} -eq $((7 + count * 6)) ]] || return 1
  offset=7
  for ((index = 0; index < count; index++)); do
    name=${fields[offset]}
    path=${fields[offset + 1]}
    url=${fields[offset + 2]}
    descriptor=${fields[offset + 3]}
    optional=${fields[offset + 4]}
    sync=${fields[offset + 5]}
    record="$name|$path|$url|$descriptor|$optional|$sync"
    _dot_overlay_record_validate "$record" || return 1
    [[ -z ${seen[$name]+x} ]] || return 1
    seen["$name"]=1
    decoded+=("$record")
    offset=$((offset + 6))
  done
  # shellcheck disable=SC2034 # Published to the isolated worker.
  OVERLAYS=("${decoded[@]+"${decoded[@]}"}")
  # shellcheck disable=SC2034 # Published for structured lifecycle checks.
  REPLY_SET_KIND=$set_kind
  # shellcheck disable=SC2034 # Published to pre-sync extensions.
  REPLY_STAGE=$stage
}
