# shellcheck shell=bash
# Process-owned child, descriptor, and temporary-path cleanup.

_dot_cleanup_reset() {
  DOT_CLEANUP_PIDS=()
  DOT_CLEANUP_PID_IDENTITIES=()
  DOT_CLEANUP_PID_GROUPS=()
  DOT_CLEANUP_PATHS=()
  DOT_CLEANUP_FDS=()
  DOT_CLEANUP_REGISTRATION_DEPTH=0
  DOT_CLEANUP_PENDING_STATUS=0
  DOT_CLEANUP_RUNNING=0
  DOT_CLEANUP_GRACE_ATTEMPTS=20
  DOT_CLEANUP_OWNER_PID=${BASHPID:-$$}
  DOT_CLEANUP_LAUNCH_ISOLATED=0
  DOT_CLEANUP_LAUNCH_RESTORE_MONITOR=0
  DOT_CLEANUP_LAUNCH_STDIN_FD=0
}

# Sourcing shared helpers twice in one process must not discard resources that
# process already owns. A fork gets a new BASHPID and starts with an empty
# registry; explicit worker setup below still resets unconditionally.
if [[ "${DOT_CLEANUP_OWNER_PID:-}" != "${BASHPID:-$$}" ]]; then
  _dot_cleanup_reset
fi

_dot_cleanup_ignore_signals() {
  # A trap protects this shell, but external commands spawned by cleanup reset
  # trapped dispositions to their defaults. Ignore the handled signals while
  # cleanup runs so a later terminal-group signal cannot kill `ps`, `pgrep`,
  # `rm`, or lock-release helpers midway through an otherwise exact cleanup.
  trap '' HUP
  trap '' INT
  trap '' QUIT
  trap '' TERM
}

_dot_cleanup_begin_registration() {
  DOT_CLEANUP_REGISTRATION_DEPTH=$((DOT_CLEANUP_REGISTRATION_DEPTH + 1))
}

_dot_cleanup_end_registration() {
  local pending=0
  if [[ "$DOT_CLEANUP_REGISTRATION_DEPTH" -gt 0 ]]; then
    DOT_CLEANUP_REGISTRATION_DEPTH=$((DOT_CLEANUP_REGISTRATION_DEPTH - 1))
  fi
  if [[ "$DOT_CLEANUP_REGISTRATION_DEPTH" -eq 0 &&
    "$DOT_CLEANUP_PENDING_STATUS" -ne 0 ]]; then
    pending=$DOT_CLEANUP_PENDING_STATUS
    _dot_cleanup_signal "$pending"
  fi
}

_dot_cleanup_signal() {
  local status="$1"
  if [[ "$DOT_CLEANUP_RUNNING" -eq 1 ]]; then
    return 0
  fi
  # Latch before requesting exit. EXIT traps do not begin atomically with the
  # `exit` builtin, so a second signal in that boundary must not replace the
  # first status or interrupt owned-resource cleanup.
  if [[ "$DOT_CLEANUP_PENDING_STATUS" -eq 0 ]]; then
    DOT_CLEANUP_PENDING_STATUS=$status
  fi
  if [[ "$DOT_CLEANUP_REGISTRATION_DEPTH" -gt 0 ]]; then
    return 0
  fi

  # Do not reap children from inside Bash's active signal-trap context. A wait
  # for the job that was interrupted by this signal can redispatch the same
  # pending trap even after its disposition has been changed, abandoning the
  # outer handler before it reaches the authoritative exit below. Install the
  # ignored dispositions first, then let the already-installed EXIT trap own
  # all blocking cleanup and any following owner action such as lock release.
  # RUNNING is only a short re-entry guard here; reset it before exit so the
  # EXIT trap performs the cleanup exactly once.
  DOT_CLEANUP_RUNNING=1
  _dot_cleanup_ignore_signals
  DOT_CLEANUP_RUNNING=0
  exit "$DOT_CLEANUP_PENDING_STATUS"
}

_dot_cleanup_register_pid() {
  local pid="$1" group="${2:-}" backend="" identity=""
  [[ "$pid" =~ ^[1-9][0-9]*$ ]] || return 2
  # A launch helper may claim only the job-control group whose leader is this
  # exact Bash job. Arbitrary numeric groups never enter the cleanup registry.
  [[ -z "$group" || "$group" == "$pid" ]] || return 2
  if _dot_cleanup_job_matches "$pid" all; then
    _dot_cleanup_process_backend backend
    if _dot_cleanup_observe_process "$backend" "$pid"; then
      identity=$DOT_CLEANUP_OBS_ID
    fi
  fi
  # The caller defers signals across child launch and registration. Nest a
  # smaller critical region here so the parallel arrays are never observable
  # with a partially published PID, identity, or owned-group record.
  _dot_cleanup_begin_registration
  DOT_CLEANUP_PIDS+=("$pid")
  DOT_CLEANUP_PID_IDENTITIES+=("$identity")
  DOT_CLEANUP_PID_GROUPS+=("$group")
  _dot_cleanup_end_registration
}

_dot_cleanup_register_path() {
  local path="$1"
  [[ -n "$path" ]] || return 2
  DOT_CLEANUP_PATHS+=("$path")
}

_dot_cleanup_register_fd() {
  local fd="$1"
  [[ "$fd" =~ ^[0-9]+$ ]] || return 2
  DOT_CLEANUP_FDS+=("$fd")
}

_dot_cleanup_mktemp_allocator() {
  local allocator_output="" allocator_path="" allocator_rc=0

  # This process is the ownership handoff point between mktemp and the parent.
  # Publish both fields even under inherited errexit, then remain alive until
  # the parent has registered any returned path. Without that acknowledgement,
  # a fast child can exit and make Bash discard COPROC_PID and its descriptors
  # before the parent executes the very next command.
  # Append a non-newline sentinel inside command substitution. Bash otherwise
  # strips every trailing newline, which makes a pathname ending in newline
  # indistinguishable from mktemp's record terminator. Remove exactly the
  # terminator after capture, preserving all pathname bytes Bash can represent.
  if allocator_output=$(
    command mktemp "$@"
    allocator_rc=$?
    printf '\034'
    exit "$allocator_rc"
  ); then
    allocator_rc=0
  else
    allocator_rc=$?
  fi
  if [[ "$allocator_output" == *$'\034' ]]; then
    allocator_output=${allocator_output%$'\034'}
    if [[ "$allocator_output" == *$'\n' ]]; then
      allocator_path=${allocator_output%$'\n'}
    elif [[ "$allocator_rc" -eq 0 ]]; then
      # A successful mktemp must publish one newline-terminated pathname.
      # Refuse an ambiguous record rather than registering a guessed path.
      allocator_rc=1
    fi
  elif [[ "$allocator_rc" -eq 0 ]]; then
    allocator_rc=1
  fi
  # NUL framing keeps embedded newlines unambiguous on the parent pipe.
  printf '%s\0%s\0' "$allocator_rc" "$allocator_path"
  IFS= read -r || true
  return "$allocator_rc"
}

_dot_cleanup_mktemp() {
  local path="" allocator_status="" allocator_pid="" read_fd="" write_fd=""
  local allocator_rc=0 status_read_rc=1 path_read_rc=1 had_monitor=0 tmp_root=""
  local -a allocator_args=("$@")
  # Bash writes unnamed-coprocess state into these special variables. Make
  # them function-local so allocating dot scratch cannot overwrite a coprocess
  # that belongs to the caller.
  local COPROC_PID=""
  local -a COPROC=()

  # GNU mktemp consults TMPDIR when no template is provided, while BSD mktemp
  # may select its platform default instead. Always make the operation root
  # explicit for the two shorthand forms used by dot so nested worker scratch
  # stays beneath its registered parent on every supported platform.
  if [[ "$#" -eq 0 || ("$#" -eq 1 && "$1" == -d) ]]; then
    tmp_root=${TMPDIR:-/tmp}
    tmp_root=${tmp_root%/}
    if [[ "$#" -eq 0 ]]; then
      allocator_args=("$tmp_root/dot.XXXXXXXX")
    else
      allocator_args=(-d "$tmp_root/dot.XXXXXXXX")
    fi
  fi

  _dot_cleanup_begin_registration
  [[ "$-" == *m* ]] && had_monitor=1
  # Job control assigns this short-lived coprocess a private process group at
  # launch. A terminal-group signal can therefore reach and latch in the parent
  # without killing mktemp between filesystem creation and pathname output.
  # The allocator's acknowledgement handshake keeps the job and descriptors
  # alive until the parent has captured them and registered any published path.
  # Restore monitor mode immediately after capture; the already-created group
  # remains isolated while the parent synchronously drains and reaps it.
  set -m
  coproc _dot_cleanup_mktemp_allocator "${allocator_args[@]}"
  allocator_pid=$COPROC_PID
  read_fd=${COPROC[0]}
  write_fd=${COPROC[1]}
  [[ "$had_monitor" == 1 ]] || set +m

  # A handled signal can interrupt either read. The child deliberately remains
  # blocked on its acknowledgement, so a retry is safe while this exact Bash
  # job is active. Registration depth prevents cancellation from exiting before
  # a successfully published pathname becomes owned by the parent.
  while [[ "$status_read_rc" -ne 0 ]]; do
    if IFS= read -r -d '' allocator_status <&"$read_fd"; then
      status_read_rc=0
      break
    else
      # Capture inside the branch: the status of an `if` with no selected body
      # is zero, which would otherwise disguise an interrupted/EOF read.
      status_read_rc=$?
    fi
    _dot_cleanup_job_matches "$allocator_pid" active || break
  done
  while [[ "$path_read_rc" -ne 0 ]]; do
    if IFS= read -r -d '' path <&"$read_fd"; then
      path_read_rc=0
      break
    else
      path_read_rc=$?
    fi
    _dot_cleanup_job_matches "$allocator_pid" active || break
  done

  # Register before releasing the allocator. A defensive mktemp wrapper can
  # publish a valid path and still fail; that object is already ours and must
  # be removed on failure rather than silently leaked.
  [[ -z "$path" ]] || _dot_cleanup_register_path "$path"
  printf '.\n' 1>&"$write_fd" 2>/dev/null || true
  { exec {write_fd}>&-; } 2>/dev/null || true

  while :; do
    if wait "$allocator_pid" 2>/dev/null; then
      allocator_rc=0
      break
    else
      allocator_rc=$?
    fi
    _dot_cleanup_job_matches "$allocator_pid" active || break
  done
  { exec {read_fd}<&-; } 2>/dev/null || true

  # Require the child's structured record and wait status to agree. Treat a
  # malformed or truncated record as failure even if wait happened to succeed;
  # ownership safety is more important than accepting an ambiguous allocation.
  if [[ "$status_read_rc" -ne 0 || "$path_read_rc" -ne 0 ||
    ! "$allocator_status" =~ ^([0-9]|[1-9][0-9]{1,2})$ ||
    "$allocator_status" -gt 255 || "$allocator_rc" -ne "$allocator_status" ||
    "$allocator_rc" -ne 0 || -z "$path" ]]; then
    [[ -z "$path" ]] || _dot_cleanup_remove_path "$path" || true
    _dot_cleanup_end_registration
    REPLY=""
    return 1
  fi
  _dot_cleanup_end_registration
  REPLY="$path"
}

_dot_cleanup_unregister_pid() {
  local pid="$1" index
  [[ ${DOT_CLEANUP_PIDS[*]+set} == set ]] || return 0
  # Snapshotting compacts these sparse arrays independently. Defer handled
  # signals until every field is gone so cleanup cannot pair a surviving PID
  # with the removed worker's identity or group.
  _dot_cleanup_begin_registration
  for index in "${!DOT_CLEANUP_PIDS[@]}"; do
    [[ "${DOT_CLEANUP_PIDS[$index]}" == "$pid" ]] || continue
    unset "DOT_CLEANUP_PIDS[$index]"
    unset "DOT_CLEANUP_PID_IDENTITIES[$index]"
    unset "DOT_CLEANUP_PID_GROUPS[$index]"
  done
  _dot_cleanup_end_registration
}

_dot_cleanup_unregister_path() {
  local path="$1" index
  [[ ${DOT_CLEANUP_PATHS[*]+set} == set ]] || return 0
  for index in "${!DOT_CLEANUP_PATHS[@]}"; do
    [[ "${DOT_CLEANUP_PATHS[$index]}" == "$path" ]] || continue
    unset "DOT_CLEANUP_PATHS[$index]"
  done
}

_dot_cleanup_unregister_fd() {
  local fd="$1" index
  [[ ${DOT_CLEANUP_FDS[*]+set} == set ]] || return 0
  for index in "${!DOT_CLEANUP_FDS[@]}"; do
    [[ "${DOT_CLEANUP_FDS[$index]}" == "$fd" ]] || continue
    unset "DOT_CLEANUP_FDS[$index]"
  done
}

_dot_cleanup_job_matches() {
  local target="$1" mode="${2:-all}" job
  local jobs_output=""
  # Bash's job table is the ownership proof here; kill -0 would accept an
  # unrelated process after numeric PID reuse. The `all` lookup gates only the
  # optional process-identity snapshot at registration; the exact child PID is
  # recorded regardless so a fast completion remains waitable by its parent.
  # Signalling explicitly unions running and stopped jobs: a completed job must
  # never authorize a signal, while a leader stopped as a PGID anchor remains
  # active cleanup state.
  case "$mode" in
    active) jobs_output=$(
      jobs -pr
      jobs -ps
    ) ;;
    all) jobs_output=$(jobs -p) ;;
    *) return 2 ;;
  esac
  while IFS= read -r job; do
    [[ "$job" == "$target" ]] && return 0
  done <<<"$jobs_output"
  return 1
}

_dot_cleanup_process_backend() {
  local output_name="$1"
  if [[ -r "/proc/$$/stat" ]]; then
    printf -v "$output_name" '%s' proc
  else
    # Portable `ps` exposes process start time only to the second on macOS.
    # That is not a strong enough PID-reuse identity for signalling arbitrary
    # descendants. Process-tree discovery therefore fails closed; the launch
    # layer uses owned process groups only for noninteractive workers.
    printf -v "$output_name" '%s' none
  fi
}

_dot_cleanup_has_controlling_tty() {
  # stdin is commonly redirected during a normal interactive update. Opening
  # /dev/tty is the relevant capability test for prompt-capable hooks.
  (: </dev/tty) 2>/dev/null
}

_dot_cleanup_should_isolate_job() {
  # A worker already inside an outer owned group must keep its descendants in
  # that group, not create nested PGIDs that the outer coordinator cannot reap.
  [[ "${DOT_CLEANUP_INHERIT_GROUP:-0}" != 1 ]] || return 1
  # Noninteractive jobs use a group on every platform. Even Linux procfs does
  # not provide child enumeration by itself, and minimal systems need not ship
  # pgrep. Making containment independent of that optional helper prevents a
  # wrapper exit from orphaning synchronous work in CI or cron. A controlling
  # TTY deliberately keeps the foreground-group path so prompts still work.
  ! _dot_cleanup_has_controlling_tty
}

# Start one indivisible launch/publication region. Callers still own the actual
# asynchronous command and immediate `$!` capture because redirections differ;
# `_dot_cleanup_finish_job_launch` publishes that PID before releasing signals.
_dot_cleanup_begin_job_launch() {
  local mode=${1:-auto}
  case $mode in
    auto | closed-stdin) ;;
    *) return 2 ;;
  esac
  _dot_cleanup_begin_registration
  DOT_CLEANUP_LAUNCH_ISOLATED=0
  DOT_CLEANUP_LAUNCH_RESTORE_MONITOR=0
  DOT_CLEANUP_LAUNCH_STDIN_FD=0
  if [[ $mode == closed-stdin ]]; then
    # A caller that closes stdin cannot prompt through the terminal and gains
    # nothing from sharing its foreground process group. Give it the same
    # kernel-owned cancellation handle on macOS and minimal systems that
    # noninteractive CI workers receive automatically.
    [[ ${DOT_CLEANUP_INHERIT_GROUP:-0} != 1 ]] || return 0
  else
    _dot_cleanup_should_isolate_job || return 0
  fi

  # A noninteractive worker gets a private job-control group. That gives
  # CI/cron cleanup one kernel-owned handle for the wrapper and every
  # synchronous external descendant without parsing reusable PIDs or depending
  # on an optional process-discovery executable.
  # Enabling monitor mode changes Bash's asynchronous-stdin default from
  # /dev/null to inherited fd 0. Open an explicit EOF source before the launch
  # so CI/cron workers cannot consume a caller's pipe or file. Interactive jobs
  # are not isolated and continue to use fd 0 for prompts.
  if ! { exec {DOT_CLEANUP_LAUNCH_STDIN_FD}</dev/null; }; then
    DOT_CLEANUP_LAUNCH_STDIN_FD=0
    return 0
  fi
  if [[ "$-" != *m* ]]; then
    set -m
    DOT_CLEANUP_LAUNCH_RESTORE_MONITOR=1
  fi
  DOT_CLEANUP_INHERIT_GROUP=1
  export DOT_CLEANUP_INHERIT_GROUP
  DOT_CLEANUP_LAUNCH_ISOLATED=1
}

_dot_cleanup_finish_job_launch() {
  local pid="$1" stdin_fd="${DOT_CLEANUP_LAUNCH_STDIN_FD:-0}" group=""

  if [[ "$DOT_CLEANUP_LAUNCH_ISOLATED" -eq 1 ]]; then
    # Bash job control guarantees PGID == leader PID for the asynchronous job
    # just launched while monitor mode was enabled.
    group="$pid"
    unset DOT_CLEANUP_INHERIT_GROUP
  fi
  if [[ "$DOT_CLEANUP_LAUNCH_RESTORE_MONITOR" -eq 1 ]]; then
    set +m
  fi
  if [[ "$stdin_fd" =~ ^[0-9]+$ && "$stdin_fd" -ne 0 ]]; then
    { exec {stdin_fd}<&-; } 2>/dev/null || true
  fi
  DOT_CLEANUP_LAUNCH_ISOLATED=0
  DOT_CLEANUP_LAUNCH_RESTORE_MONITOR=0
  DOT_CLEANUP_LAUNCH_STDIN_FD=0
  _dot_cleanup_register_pid "$pid" "$group"
  _dot_cleanup_end_registration
}

_dot_cleanup_group_job_active() {
  local pid="$1" group="$2"
  [[ -n "$group" && "$group" == "$pid" ]] || return 1
  _dot_cleanup_job_matches "$pid" active || return 1
  kill -0 -- "-$group" 2>/dev/null
}

_dot_cleanup_group_live() {
  local group="$1"
  [[ "$group" =~ ^[1-9][0-9]*$ ]] || return 1
  kill -0 -- "-$group" 2>/dev/null
}

_dot_cleanup_observe_proc() {
  local pid="$1" line rest
  IFS= read -r line 2>/dev/null <"/proc/$pid/stat" || return 1
  [[ "$line" == "$pid ("* ]] || return 1
  # A process name may contain spaces or ')', so field extraction starts after
  # the final closing delimiter. In the remainder, state is field 3, parent is
  # field 4, and the kernel start tick is field 22 (the twentieth token here).
  rest="${line##*) }"
  [[ "$rest" != "$line" ]] || return 1
  # shellcheck disable=SC2086  # Kernel stat fields are intentionally tokenized.
  set -- $rest
  [[ "$#" -ge 20 && "$2" =~ ^[0-9]+$ && "${20}" =~ ^[0-9]+$ ]] || return 1
  DOT_CLEANUP_OBS_PPID=$2
  DOT_CLEANUP_OBS_STATE=$1
  DOT_CLEANUP_OBS_ID="proc:${20}"
}

_dot_cleanup_observe_process() {
  local backend="$1" pid="$2"
  case "$backend" in
    proc) _dot_cleanup_observe_proc "$pid" ;;
    *) return 2 ;;
  esac
}

_dot_cleanup_process_matches_live() {
  local backend="$1" pid="$2" identity="$3"
  [[ -n "$identity" ]] || return 1
  _dot_cleanup_observe_process "$backend" "$pid" || return 1
  [[ "$DOT_CLEANUP_OBS_ID" == "$identity" ]] || return 1
  [[ ! "$DOT_CLEANUP_OBS_STATE" =~ ^Z ]]
}

_dot_cleanup_descendant_records() {
  local backend="$1" root="$2" root_identity="$3"
  local index=0 parent parent_identity depth
  local children child child_identity child_depth observed_parent
  local -a queue_pids=("$root") queue_identities=("$root_identity") queue_depths=(0)
  local -A seen=(["$root"]=1)

  [[ "$backend" == proc ]] || return 0
  [[ "$root" =~ ^[1-9][0-9]*$ && -n "$root_identity" ]] || return 0
  command -v pgrep >/dev/null 2>&1 || return 0
  _dot_cleanup_observe_process "$backend" "$root" || return 0
  [[ "$DOT_CLEANUP_OBS_ID" == "$root_identity" ]] || return 0

  # Keep EXIT cleanup bounded even if a child is rapidly forking or procfs
  # reports a pathological tree. Hitting either cap fails closed: only the
  # identity-validated records already emitted are eligible for signalling.
  while ((index < ${#queue_pids[@]} && index < 512)); do
    parent=${queue_pids[$index]}
    parent_identity=${queue_identities[$index]}
    depth=${queue_depths[$index]}
    index=$((index + 1))
    ((depth < 64)) || continue

    children=$(command pgrep -P "$parent" . 2>/dev/null || true)
    # shellcheck disable=SC2086  # pgrep emits a whitespace-delimited PID list.
    for child in $children; do
      [[ "$child" =~ ^[1-9][0-9]*$ && "$child" != "$$" &&
        -z "${seen[$child]:-}" ]] || continue
      _dot_cleanup_observe_process "$backend" "$child" || continue
      [[ "$DOT_CLEANUP_OBS_PPID" == "$parent" ]] || continue
      child_identity=$DOT_CLEANUP_OBS_ID

      # Re-read the parent after observing the child. If its PID changed or the
      # edge was reparented during discovery, discard this subtree.
      _dot_cleanup_observe_process "$backend" "$parent" || continue
      observed_parent=$DOT_CLEANUP_OBS_ID
      [[ "$observed_parent" == "$parent_identity" ]] || continue

      child_depth=$((depth + 1))
      seen["$child"]=1
      printf '%s\t%s\t%s\n' "$child" "$child_identity" "$child_depth"
      queue_pids+=("$child")
      queue_identities+=("$child_identity")
      queue_depths+=("$child_depth")
    done
  done
}

_dot_cleanup_remove_path() {
  local path="$1" rc=0
  if [[ "$DOT_CLEANUP_RUNNING" -eq 1 ]]; then
    if rm -rf -- "$path"; then
      _dot_cleanup_unregister_path "$path"
      return 0
    fi
    return 1
  fi

  # Stop owning the name before deletion. Signals stay deferred across this
  # short region, so a path recreated after rm cannot be deleted by EXIT
  # cleanup as though it were still the original temporary object.
  _dot_cleanup_begin_registration
  _dot_cleanup_unregister_path "$path"
  if ! rm -rf -- "$path"; then
    _dot_cleanup_register_path "$path"
    rc=1
  fi
  _dot_cleanup_end_registration
  if [[ "$rc" -eq 0 ]]; then
    return 0
  fi
  return 1
}

_dot_cleanup_close_fd() {
  local fd="$1"
  if [[ "$fd" =~ ^[0-9]+$ ]]; then
    # Redirection on a bare `exec` persists in the current shell. Scope stderr
    # to this group so closing one progress FD cannot silence later diagnostics.
    { exec {fd}>&-; } 2>/dev/null || true
  fi
  _dot_cleanup_unregister_fd "$fd"
}

_dot_cleanup_wait_job() {
  local pid="$1"
  # Cleanup runs from EXIT rather than inside the signal handler, so an exact
  # wait can safely reap the owned Bash job and consume its retained status.
  wait "$pid" 2>/dev/null || true
}

_dot_cleanup_owned() {
  local fd pid path attempt active backend index root_index root_identity root_group
  local descendant descendant_identity depth level max_depth=0
  local -a fds=() pids=() root_identities=() root_groups=() paths=()
  local -a root_groups_armed=()
  local -a descendant_pids=() descendant_identities=() descendant_depths=()
  local -A descendant_seen=()

  fds=("${DOT_CLEANUP_FDS[@]+"${DOT_CLEANUP_FDS[@]}"}")
  pids=("${DOT_CLEANUP_PIDS[@]+"${DOT_CLEANUP_PIDS[@]}"}")
  root_identities=("${DOT_CLEANUP_PID_IDENTITIES[@]+"${DOT_CLEANUP_PID_IDENTITIES[@]}"}")
  root_groups=("${DOT_CLEANUP_PID_GROUPS[@]+"${DOT_CLEANUP_PID_GROUPS[@]}"}")
  paths=("${DOT_CLEANUP_PATHS[@]+"${DOT_CLEANUP_PATHS[@]}"}")
  _dot_cleanup_process_backend backend

  # Prompt-capable workers intentionally remain in the terminal's foreground
  # group and use strong procfs identities where available. Every
  # noninteractive worker instead carries one owned PGID, so its containment
  # does not depend on optional process-discovery tools. For ordinary roots,
  # conservatively capture validated descendants before terminating the exact
  # Bash jobs.
  index=0
  for pid in "${pids[@]+"${pids[@]}"}"; do
    root_identity="${root_identities[$index]:-}"
    root_group="${root_groups[$index]:-}"
    index=$((index + 1))
    [[ -z "$root_group" ]] || continue
    [[ -n "$pid" && -n "$root_identity" ]] || continue
    _dot_cleanup_job_matches "$pid" active || continue
    while IFS=$'\t' read -r descendant descendant_identity depth; do
      [[ "$descendant" =~ ^[1-9][0-9]*$ && -n "$descendant_identity" &&
        "$depth" =~ ^[1-9][0-9]*$ ]] || continue
      [[ -z "${descendant_seen[$descendant]:-}" ]] || continue
      descendant_seen["$descendant"]=1
      descendant_pids+=("$descendant")
      descendant_identities+=("$descendant_identity")
      descendant_depths+=("$depth")
      ((depth > max_depth)) && max_depth=$depth
    done < <(_dot_cleanup_descendant_records "$backend" "$pid" "$root_identity")
  done

  # TERM leaves first opportunity for orderly shutdown. Validate group
  # ownership while its exact Bash leader is still active, then retain that
  # claim through this cleanup pass. A cooperative leader may exit before a
  # TERM-ignoring child, but that child keeps the validated PGID allocated and
  # must still receive bounded KILL escalation.
  index=0
  for pid in "${pids[@]+"${pids[@]}"}"; do
    root_index=$index
    root_group="${root_groups[$root_index]:-}"
    root_groups_armed[root_index]=0
    index=$((index + 1))
    [[ -n "$pid" ]] || continue
    if [[ -n "$root_group" ]]; then
      if ! _dot_cleanup_group_job_active "$pid" "$root_group"; then
        continue
      fi
      # SIGSTOP is uncatchable and leaves the exact validated leader resident
      # in its original group. That kernel-held member prevents an empty PGID
      # from being reused between graceful TERM and numeric group escalation.
      # The parent owns all worker scratch, so cancellation may safely KILL the
      # stopped wrapper after giving its synchronous descendants a TERM grace.
      kill -STOP "$pid" 2>/dev/null || continue
      root_groups_armed[root_index]=1
      kill -TERM -- "-$root_group" 2>/dev/null || true
    else
      _dot_cleanup_job_matches "$pid" active || continue
      kill -TERM "$pid" 2>/dev/null || true
    fi
  done

  # Descendants still shut down deepest-first using the identities captured
  # while every validated ancestor was present.
  for ((level = max_depth; level >= 1; level--)); do
    [[ ${descendant_pids[*]+set} == set ]] || break
    for index in "${!descendant_pids[@]}"; do
      [[ "${descendant_depths[$index]}" -eq "$level" ]] || continue
      descendant=${descendant_pids[$index]}
      descendant_identity=${descendant_identities[$index]}
      if _dot_cleanup_process_matches_live "$backend" "$descendant" "$descendant_identity"; then
        kill -TERM "$descendant" 2>/dev/null || true
      fi
    done
  done

  for ((attempt = 0; attempt < DOT_CLEANUP_GRACE_ATTEMPTS; attempt++)); do
    active=0
    if [[ ${descendant_pids[*]+set} == set ]]; then
      for index in "${!descendant_pids[@]}"; do
        if _dot_cleanup_process_matches_live "$backend" \
          "${descendant_pids[$index]}" "${descendant_identities[$index]}"; then
          active=1
          break
        fi
      done
    fi
    index=0
    for pid in "${pids[@]+"${pids[@]}"}"; do
      root_index=$index
      root_group="${root_groups[$root_index]:-}"
      index=$((index + 1))
      [[ -n "$pid" ]] || continue
      if { [[ "${root_groups_armed[$root_index]:-0}" -eq 1 ]] &&
        _dot_cleanup_group_live "$root_group"; } ||
        { [[ -z "$root_group" ]] && _dot_cleanup_job_matches "$pid" active; }; then
        active=1
        break
      fi
    done
    [[ "$active" -eq 1 ]] || break
    # A later terminal-group signal can interrupt this external sleep even
    # though its trap correctly leaves the first signal authoritative. Do not
    # let errexit abort cleanup before escalation and exact reaping complete.
    sleep 0.05 || true
  done

  # Stop still-active roots before descendant KILL so they cannot launch more
  # work during escalation. Identity checks make every descendant signal
  # fail-closed if its PID no longer names the process captured above.
  index=0
  for pid in "${pids[@]+"${pids[@]}"}"; do
    root_index=$index
    root_group="${root_groups[$root_index]:-}"
    index=$((index + 1))
    [[ -n "$pid" ]] || continue
    if [[ "${root_groups_armed[$root_index]:-0}" -eq 1 ]]; then
      # Only groups armed against an exact live leader above are eligible for
      # escalation. Recheck liveness immediately before signalling so an empty
      # group is skipped instead of treating a bare numeric PGID as ownership.
      if _dot_cleanup_group_live "$root_group"; then
        kill -KILL -- "-$root_group" 2>/dev/null || true
      fi
    elif [[ -z "$root_group" ]] && _dot_cleanup_job_matches "$pid" active; then
      kill -KILL "$pid" 2>/dev/null || true
    fi
  done
  for ((level = 1; level <= max_depth; level++)); do
    [[ ${descendant_pids[*]+set} == set ]] || break
    for index in "${!descendant_pids[@]}"; do
      [[ "${descendant_depths[$index]}" -eq "$level" ]] || continue
      descendant=${descendant_pids[$index]}
      descendant_identity=${descendant_identities[$index]}
      if _dot_cleanup_process_matches_live "$backend" "$descendant" "$descendant_identity"; then
        kill -KILL "$descendant" 2>/dev/null || true
      fi
    done
  done

  for pid in "${pids[@]+"${pids[@]}"}"; do
    [[ -n "$pid" ]] || continue
    _dot_cleanup_wait_job "$pid"
    _dot_cleanup_unregister_pid "$pid"
  done

  # Keep coordinator descriptors valid until every worker is stopped and
  # reaped. A signal can interrupt a builtin read and enter this cleanup; if
  # its fd is closed first, Bash may resume the read long enough to replace the
  # authoritative signal status with EBADF. Worker termination no longer needs
  # those coordinator handles, so closing them here preserves both contracts.
  for fd in "${fds[@]+"${fds[@]}"}"; do
    [[ -n "$fd" ]] || continue
    _dot_cleanup_close_fd "$fd"
  done

  for path in "${paths[@]+"${paths[@]}"}"; do
    [[ -n "$path" ]] || continue
    _dot_cleanup_remove_path "$path" || true
  done

  return 0
}

_dot_cleanup_all() {
  [[ "$DOT_CLEANUP_RUNNING" -eq 0 ]] || return 0
  DOT_CLEANUP_RUNNING=1
  _dot_cleanup_owned
  DOT_CLEANUP_RUNNING=0
}

_dot_cleanup_install_signal_traps() {
  trap '_dot_cleanup_signal 129' HUP
  trap '_dot_cleanup_signal 130' INT
  trap '_dot_cleanup_signal 131' QUIT
  trap '_dot_cleanup_signal 143' TERM
}

_dot_cleanup_on_exit() {
  _dot_cleanup_ignore_signals
  # Cleanup is best effort at process exit. Never let a secondary removal error
  # replace the command or signal status that caused the owner to exit.
  _dot_cleanup_all || true
}

_dot_cleanup_install_owner_traps() {
  trap '_dot_cleanup_on_exit' EXIT
  _dot_cleanup_install_signal_traps
}

_dot_cleanup_prepare_subshell() {
  _dot_cleanup_reset
  if [[ "${DOT_CLEANUP_INHERIT_GROUP:-0}" == 1 ]]; then
    # The outer coordinator owns this noninteractive process group. Nested
    # background jobs must stay in it so one group escalation covers the whole
    # tree instead of creating unregistered child PGIDs.
    set +m
  fi
  # errtrace can copy a coordinator's ERR policy into background workers. The
  # worker reports through its result files, so an inherited handler would emit
  # duplicate diagnostics or run parent-only recovery inside the subshell.
  trap - ERR
  trap '_dot_cleanup_ignore_signals; _dot_cleanup_all' EXIT
  _dot_cleanup_install_signal_traps
}
