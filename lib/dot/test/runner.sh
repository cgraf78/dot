# shellcheck shell=bash
# shellcheck disable=SC2154 # The coordinator and discovery stage own state.
# Parallel execution, timeout, cancellation, rendering, and cleanup.

if [[ -z "$max_jobs" ]]; then
  max_jobs=$(_default_jobs)
fi
case "$max_jobs" in
  '' | *[!0-9]*)
    echo "invalid jobs value: $max_jobs" >&2
    exit 2
    ;;
  0[0-9]*)
    echo "invalid jobs value: $max_jobs" >&2
    exit 2
    ;;
esac
if [[ ${#max_jobs} -gt 9 ]]; then
  echo "invalid jobs value: $max_jobs" >&2
  exit 2
fi
max_jobs=$((10#$max_jobs))
[[ "$max_jobs" -lt 1 ]] && max_jobs=1
[[ "$max_jobs" -gt "${#scripts[@]}" ]] && max_jobs=${#scripts[@]}

# Auto-verbose in CI so test output is visible (folded via ::group::)
[[ -n "${GITHUB_ACTIONS:-}" ]] && verbose=true

passed=0
failed=0
skipped=0
failed_names=()
# shellcheck disable=SC2034 # Shared resource cleanup reads this dynamic policy.
# The timeout supervisor performs a graceful TERM-to-KILL escalation before it
# exits. Give the shared owner enough time to let that exact child finish on
# platforms without process-tree discovery.
DOT_CLEANUP_GRACE_ATTEMPTS=80

_run_suite() {
  local detach="$1" name="$2" script="$3" suite_tmpdir="$4" result="$5"
  local parallel_flag=0 suite_timeout timeout_supervisor
  local suite_cmd=()
  local child_home=$DOT_TEST_SOURCE_HOME suite_path=$PATH
  _dot_test_suite_revalidate "$script" || {
    printf 'dot: test suite changed after discovery: %s\n' "$name" >&2
    return 126
  }
  $parallel && parallel_flag=1
  if [[ $DOT_TEST_SOURCE_HOME != "$DOT_TEST_HOST_HOME" ]]; then
    suite_path=$suite_path:$DOT_TEST_HOST_HOME/.local/bin
  fi
  if [[ ${suite_sources[$script]} == provider &&
    $DOT_TEST_SOURCE_HOME != "$DOT_TEST_HOST_HOME" ]]; then
    # Provider self-tests are not consumer tests. Keep a linked client
    # worktree from changing their HOME or putting its launchers ahead of the
    # isolated Git backend selected by the coordinator.
    child_home=$DOT_TEST_HOST_HOME
    case $suite_path in
      "$DOT_TEST_SOURCE_HOME/.local/bin:"*)
        suite_path=${suite_path#"$DOT_TEST_SOURCE_HOME/.local/bin:"}
        ;;
    esac
  fi
  local env_cmd=(
    env
    -u DOT_CLIENT_GIT_DIR
    "HOME=$child_home"
    "PATH=$suite_path"
    "TMPDIR=$suite_tmpdir"
    # A suite may run commands before creating its own fixture HOME. Keep
    # launcher caches and other runtime state out of the source checkout even
    # during that setup window.
    "XDG_CACHE_HOME=$suite_tmpdir/xdg-cache"
    "XDG_STATE_HOME=$suite_tmpdir/xdg-state"
    "DOT_TEST=1"
    "DOT_TEST_STYLE=$_child_style"
    "DOT_TEST_JOBS=$max_jobs"
    "DOT_TEST_PARALLEL=$parallel_flag"
    "DOT_TEST_DOT_ROOT=$DOT_SOURCE_ROOT"
    "DOT_TEST_HOST_HOME=$DOT_TEST_HOST_HOME"
    "DOT_TEST_SOURCE_HOME=$child_home"
    "DOT_TEST_RESULT_FILE=$result"
    "DOT_TEST_REPORTER=$DOT_SOURCE_ROOT/lib/dot/public/test-reporter-v1"
    "DOT_TEST_TIMEOUT=$DOT_SOURCE_ROOT/lib/dot/public/test-timeout-v1"
  )

  if [[ $DOT_TEST_SOURCE_HOME != "$DOT_TEST_HOST_HOME" ]]; then
    env_cmd+=(
      "MISE_DATA_DIR=${MISE_DATA_DIR:-$DOT_TEST_HOST_HOME/.local/share/mise}"
      "MISE_STATE_DIR=${MISE_STATE_DIR:-$DOT_TEST_HOST_HOME/.local/state/mise}"
      "MISE_CACHE_DIR=${MISE_CACHE_DIR:-$DOT_TEST_HOST_HOME/.cache/mise}"
    )
  fi

  suite_timeout=$(_dot_test_suite_timeout "${suite_sources[$script]}")
  timeout_supervisor=$DOT_SOURCE_ROOT/lib/dot/public/test-timeout-v1
  if ! command -v python3 >/dev/null 2>&1 || [[ ! -x "$timeout_supervisor" ]]; then
    echo "dot test: suite timeout requires python3 and $timeout_supervisor" >&2
    return 127
  fi
  suite_cmd=(python3 "$timeout_supervisor" "$suite_timeout" "${env_cmd[@]}" "$script")

  if $detach && [[ ${DOT_CLEANUP_INHERIT_GROUP:-0} == 1 ]]; then
    exec "${suite_cmd[@]}"
  elif $detach && command -v setsid >/dev/null 2>&1; then
    exec setsid "${suite_cmd[@]}"
  elif $detach && command -v python3 >/dev/null 2>&1; then
    # macOS lacks GNU `setsid`, but `dot test` still needs background suites
    # out of the caller's terminal session. Otherwise any nested tool that
    # reopens `/dev/tty` can intermittently SIGTTIN-suspend the whole runner.
    exec python3 -c '
import os
import sys

os.setsid()
os.execvpe(sys.argv[1], sys.argv[1:], os.environ)
' "${suite_cmd[@]}"
  else
    exec "${suite_cmd[@]}"
  fi
}

_wait_job_bounded() {
  local pid="$1" limit="$2" remaining
  # SECONDS has integer precision, so a one-second grace period can otherwise
  # expire immediately at a tick boundary before a finished pipe drains.
  remaining=$((limit * 20))

  # Every caller passes an exact child job. Bash's job table distinguishes an
  # active child from a completed-but-unreaped zombie without depending on an
  # optional or platform-specific `ps`, and it cannot mistake a reused PID for
  # the original job.
  while _dot_cleanup_job_matches "$pid" active; do
    ((remaining > 0)) || return 1
    remaining=$((remaining - 1))
    sleep 0.05
  done
  wait "$pid" 2>/dev/null || true
}

_stop_job() {
  local pid="$1" signal_name="${2:-TERM}"
  if _dot_cleanup_job_matches "$pid" active; then
    kill -"$signal_name" "$pid" 2>/dev/null || true
  fi
  _wait_job_bounded "$pid" 3 && return 0
  if _dot_cleanup_job_matches "$pid" active; then
    kill -KILL "$pid" 2>/dev/null || true
  fi
  _wait_job_bounded "$pid" 1 || true
}

_prune_stale_test_roots() {
  local parent="$1" now="$2" candidate marker owner_pid started
  for candidate in "$parent"/run.*; do
    [[ -d "$candidate" && ! -L "$candidate" && -O "$candidate" ]] || continue
    marker="$candidate/.dot-suite-owner-v3"
    [[ -f "$marker" && -O "$marker" ]] || continue
    IFS=$'\t' read -r owner_pid started <"$marker" || continue
    [[ "$owner_pid" =~ ^[1-9][0-9]{0,9}$ ]] || continue
    kill -0 "$owner_pid" 2>/dev/null && continue
    [[ "$started" =~ ^[1-9][0-9]{0,17}$ ]] || continue
    ((started <= now && now - started > 86400)) || continue
    rm -rf -- "$candidate"
  done
}

_dot_test_tmp_base="${TMPDIR:-/tmp}"
_dot_test_tmp_base="${_dot_test_tmp_base%/}"
[[ -n "$_dot_test_tmp_base" ]] || _dot_test_tmp_base=/
_dot_test_tmp_parent="$_dot_test_tmp_base/dot-suite-runs.$EUID"
if ! mkdir -m 700 "$_dot_test_tmp_parent" 2>/dev/null &&
  [[ ! -e "$_dot_test_tmp_parent" ]]; then
  echo "dot test: could not create temporary root" >&2
  exit 1
fi
if [[ ! -d "$_dot_test_tmp_parent" || -L "$_dot_test_tmp_parent" ||
  ! -O "$_dot_test_tmp_parent" ]]; then
  echo "dot test: unsafe temporary root: $_dot_test_tmp_parent" >&2
  exit 1
fi
chmod 700 "$_dot_test_tmp_parent" || {
  echo "dot test: could not secure temporary root" >&2
  exit 1
}
_dot_test_now=$(date +%s) || {
  echo "dot test: could not read system time" >&2
  exit 1
}
_prune_stale_test_roots "$_dot_test_tmp_parent" "$_dot_test_now"
if ! _dot_cleanup_mktemp -d "$_dot_test_tmp_parent/run.XXXXXXXX"; then
  echo "dot test: could not create temporary directory" >&2
  exit 1
fi
tmpdir=$REPLY
if ! printf '%s\t%s\n' "$$" "$_dot_test_now" >"$tmpdir/.dot-suite-owner-v3"; then
  echo "dot test: could not mark temporary directory" >&2
  exit 1
fi
_dot_test_configure_git_backend "$tmpdir/system-git" || exit 2

# shellcheck disable=SC2329 # Signal traps invoke this cancellation handler.
_cancel_suite() {
  local signal_name="$1" status="$2" pid
  trap - HUP INT TERM
  if [[ -n "${suite_pid:-}" ]]; then
    _stop_job "$suite_pid" "$signal_name"
  else
    for pid in $(jobs -p); do
      _stop_job "$pid" "$signal_name"
    done
  fi
  if [[ -n "${tee_pid:-}" ]]; then
    _stop_job "$tee_pid"
  fi
  exit "$status"
}

if $parallel; then
  _title "dot test"
  echo ""
  _ansi dim "Running ${#scripts[@]} test suites with up to $max_jobs jobs..."
  echo ""

  _start_script() {
    local script="$1" name suite_tmpdir result worker_pid
    name=$(_dot_test_suite_label "$script")
    suite_tmpdir="$tmpdir/$name.tmp"
    result="$tmpdir/$name.result"
    mkdir -p "$suite_tmpdir" || {
      printf 'dot test: could not create suite directory: %s\n' \
        "$suite_tmpdir" >&2
      exit 1
    }
    : >"$result" || {
      printf 'dot test: could not create result file: %s\n' "$result" >&2
      exit 1
    }
    chmod 0600 "$result" || {
      printf 'dot test: could not secure result file: %s\n' "$result" >&2
      exit 1
    }
    : >"$tmpdir/$name.out" || {
      printf 'dot test: could not create output file for %s\n' "$name" >&2
      exit 1
    }
    # The worker's signal trap owns the timeout supervisor, which in turn owns
    # the suite session. Register the worker directly instead of freezing an
    # outer process-group leader during shared cleanup; a stopped worker cannot
    # perform that orderly, portable handoff on macOS or Linux.
    _dot_cleanup_begin_registration
    (
      _dot_cleanup_prepare_subshell
      suite_pid=""
      trap '_cancel_suite HUP 129' HUP
      trap '_cancel_suite INT 130' INT
      trap '_cancel_suite TERM 143' TERM

      SECONDS=0
      _run_suite true "$name" "$script" "$suite_tmpdir" "$result" \
        </dev/null >"$tmpdir/$name.out" 2>&1 &
      suite_pid=$!
      wait "$suite_pid"
      suite_status=$?
      _dot_cleanup_install_signal_traps
      echo "$SECONDS" >"$tmpdir/$name.time"
      echo "$suite_status" >"$tmpdir/$name.rc"
    ) </dev/null &
    worker_pid=$!
    _dot_cleanup_register_pid "$worker_pid"
    _dot_cleanup_end_registration
    worker_pids[$name]=$worker_pid
  }

  _worker_active() {
    local pid="$1" state
    kill -0 "$pid" 2>/dev/null || return 1
    state=$(ps -o stat= -p "$pid" 2>/dev/null) || return 1
    [[ ! "$state" =~ ^[[:space:]]*Z ]]
  }

  _collect_dead_worker() {
    local name="$1" pid="$2" worker_status=0
    wait "$pid" 2>/dev/null || worker_status=$?
    _dot_cleanup_unregister_pid "$pid"
    [[ -f "$tmpdir/$name.rc" ]] && return 0

    # The wrapper publishes the exit code last. If it vanished first, retain
    # progress by synthesizing a failed terminal state instead of waiting for
    # a record that can never appear.
    worker_failures[$name]=$worker_status
    printf 'dot test: worker exited before publishing a result (status %s)\n' \
      "$worker_status" >>"$tmpdir/$name.out" || exit 1
  }

  # Monitor completion as tests finish
  completed=0
  running=0
  next=0
  declare -A worker_pids=()
  declare -A worker_failures=()
  declare -A finished_names=()
  declare -A classifications=()
  while [[ $completed -lt ${#scripts[@]} ]]; do
    while [[ $running -lt $max_jobs && $next -lt ${#scripts[@]} ]]; do
      _start_script "${scripts[$next]}"
      running=$((running + 1))
      next=$((next + 1))
    done

    for script in "${scripts[@]}"; do
      name=$(_dot_test_suite_label "$script")
      [[ ${finished_names[$name]+set} == set ]] && continue
      [[ ${worker_pids[$name]+set} == set ]] || continue
      if [[ ! -f "$tmpdir/$name.rc" ]]; then
        _worker_active "${worker_pids[$name]}" && continue
        _collect_dead_worker "$name" "${worker_pids[$name]}"
      fi

      if [[ ${worker_failures[$name]+set} == set ]]; then
        suite_status=1
        elapsed=0
      else
        suite_status=$(cat "$tmpdir/$name.rc")
        elapsed=$(cat "$tmpdir/$name.time")
      fi
      classification=$(_classify_suite "$suite_status" "$tmpdir/$name.result")
      if [[ ${worker_failures[$name]+set} != set ]]; then
        wait "${worker_pids[$name]}" 2>/dev/null || true
        _dot_cleanup_unregister_pid "${worker_pids[$name]}"
      fi
      classifications[$name]=$classification
      case $classification in
        skip)
          IFS=$'\t' read -r _ skip_detail <"$tmpdir/$name.result" || true
          _mark_skip "$name" "${elapsed}s" "$skip_detail"
          skipped=$((skipped + 1))
          ;;
        pass)
          _mark_pass "$name" "${elapsed}s"
          passed=$((passed + 1))
          ;;
        incomplete)
          _mark_fail "$name" "${elapsed}s"
          echo "  $name: completed without a structured result" >&2
          failed=$((failed + 1))
          failed_names+=("$name")
          ;;
        invalid)
          _mark_fail "$name" "${elapsed}s"
          echo "  $name: emitted an invalid structured result" >&2
          failed=$((failed + 1))
          failed_names+=("$name")
          ;;
        *)
          _mark_fail "$name" "${elapsed}s"
          failed=$((failed + 1))
          failed_names+=("$name")
          ;;
      esac
      finished_names[$name]=1
      completed=$((completed + 1))
      running=$((running - 1))
    done
    [[ $completed -lt ${#scripts[@]} ]] && sleep 0.2
  done

  # Print output for failed tests (or all if verbose / CI)
  for script in "${scripts[@]}"; do
    name=$(_dot_test_suite_label "$script")
    classification=${classifications[$name]}
    if [[ $classification == fail || $classification == incomplete ||
      $classification == invalid ]] || $verbose; then
      echo ""
      if [[ -n "${GITHUB_ACTIONS:-}" ]]; then
        echo "::group::$name output"
      else
        _header "── $name output ──"
      fi
      cat "$tmpdir/$name.out"
      [[ -n "${GITHUB_ACTIONS:-}" ]] && echo "::endgroup::"
    fi
  done

else
  _title "dot test"
  echo ""
  _ansi dim "Running ${#scripts[@]} test suites..."

  for script in "${scripts[@]}"; do
    name=$(_dot_test_suite_label "$script")
    echo ""
    if [[ -n "${GITHUB_ACTIONS:-}" ]]; then
      echo "::group::$name"
    else
      _header "── $name ──"
    fi
    SECONDS=0
    suite_tmpdir="$tmpdir/$name.tmp"
    result="$tmpdir/$name.result"
    output_fifo="$suite_tmpdir/output.fifo"
    mkdir -p "$suite_tmpdir" || {
      printf 'dot test: could not create suite directory: %s\n' \
        "$suite_tmpdir" >&2
      exit 1
    }
    : >"$result" || {
      printf 'dot test: could not create result file: %s\n' "$result" >&2
      exit 1
    }
    chmod 0600 "$result" || {
      printf 'dot test: could not secure result file: %s\n' "$result" >&2
      exit 1
    }
    : >"$tmpdir/$name.out" || {
      printf 'dot test: could not create output file for %s\n' "$name" >&2
      exit 1
    }
    mkfifo "$output_fifo" || {
      printf 'dot test: could not create output pipe for %s\n' "$name" >&2
      exit 1
    }
    _dot_cleanup_begin_registration
    tee "$tmpdir/$name.out" <"$output_fifo" &
    tee_pid=$!
    _dot_cleanup_register_pid "$tee_pid"
    _dot_cleanup_end_registration
    suite_pid=""
    trap '_cancel_suite HUP 129' HUP
    trap '_cancel_suite INT 130' INT
    trap '_cancel_suite TERM 143' TERM
    _dot_cleanup_begin_registration
    _run_suite false "$name" "$script" "$suite_tmpdir" "$result" \
      </dev/null >"$output_fifo" 2>&1 &
    suite_pid=$!
    _dot_cleanup_register_pid "$suite_pid"
    _dot_cleanup_end_registration
    wait "$suite_pid"
    suite_status=$?
    _dot_cleanup_unregister_pid "$suite_pid"
    _wait_job_bounded "$tee_pid" 1 || _stop_job "$tee_pid"
    _dot_cleanup_unregister_pid "$tee_pid"
    rm -f "$output_fifo"
    _dot_cleanup_install_signal_traps
    elapsed=$SECONDS
    case "$(_classify_suite "$suite_status" "$result")" in
      skip)
        IFS=$'\t' read -r _ skip_detail <"$result" || true
        _mark_skip "$name" "${elapsed}s" "$skip_detail"
        skipped=$((skipped + 1))
        ;;
      pass)
        _mark_pass "$name" "${elapsed}s"
        passed=$((passed + 1))
        ;;
      incomplete)
        _mark_fail "$name" "${elapsed}s"
        echo "  $name: completed without a structured result" >&2
        failed=$((failed + 1))
        failed_names+=("$name")
        ;;
      invalid)
        _mark_fail "$name" "${elapsed}s"
        echo "  $name: emitted an invalid structured result" >&2
        failed=$((failed + 1))
        failed_names+=("$name")
        ;;
      *)
        _mark_fail "$name" "${elapsed}s"
        failed=$((failed + 1))
        failed_names+=("$name")
        ;;
    esac
    [[ -n "${GITHUB_ACTIONS:-}" ]] && echo "::endgroup::"
  done
fi

# Remove the invocation root before rendering the authoritative result so a
# cleanup failure cannot be hidden behind a green summary.
_dot_cleanup_remove_path "$tmpdir" || {
  printf 'dot test: could not remove temporary directory: %s\n' "$tmpdir" >&2
  failed=$((failed + 1))
  failed_names+=(cleanup)
}

# Summary
summary="Suites: $passed passed"
[[ $skipped -gt 0 ]] && summary+=", $skipped skipped"
[[ $failed -gt 0 ]] && summary+=", $failed failed"
summary+=" (${#scripts[@]} total)"
if [[ $failed -gt 0 ]]; then
  _summary_box red "✗ $summary"
  _styled red "Failed: ${failed_names[*]}"
else
  _summary_box green "✓ $summary"
fi
[[ $failed -eq 0 ]]
