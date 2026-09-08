#!/data/data/com.termux/files/usr/bin/bash
set -euo pipefail

# The standard matrix owns the full test suite. This job executes the
# NDK-built Android binary inside the real Termux app sandbox and
# verifies the transported release's startup contract plus one native
# signal-cancellation lifecycle path.
binary=.termux-ci/dot
metadata=.termux-ci/.dot-install.json
absolute_binary=$PWD/$binary

[[ -x $binary ]] || {
  printf 'termux-ci: transported binary missing: %s\n' "$binary" >&2
  exit 1
}
[[ -f $metadata ]] || {
  printf 'termux-ci: transported release metadata missing: %s\n' "$metadata" >&2
  exit 1
}

help_expected='usage: dot <command> [<args>]'
help_actual=$("$binary" help)
[[ $help_actual == "$help_expected"* ]] || {
  printf 'termux-ci: unexpected help output: %s\n' "$help_actual" >&2
  exit 1
}

version_actual=$("$binary" version)
case $version_actual in
  'dot commit '*'(config 1; extensions 1; library 1)') ;;
  *)
    printf 'termux-ci: unexpected version output: %s\n' "$version_actual" >&2
    exit 1
    ;;
esac

set +e
unknown_output=$("$binary" frobnicate 2>&1)
unknown_status=$?
set -e
if [[ $unknown_status -ne 1 ]]; then
  printf 'termux-ci: unknown command unexpectedly succeeded\n' >&2
  exit 1
fi
[[ $unknown_output == 'dot: startup: cannot resolve source root from executable' ]] || {
  printf 'termux-ci: unexpected operational failure: %s\n' "$unknown_output" >&2
  exit 1
}

# Operational commands require an absolute argv[0] under Termux's linker
# interposition contract. Exercise native SIGQUIT handling inside the real app
# sandbox with the complete transported release tree.
signal_root=$(mktemp -d "${TMPDIR:-$PREFIX/tmp}/dot-termux-signal.XXXXXX")
signal_home=$signal_root/home
signal_state=$signal_root/state
signal_suites=$signal_root/suites
signal_pid_file=$signal_root/suite.pid
runner_pid=''
suite_pid=''
watchdog_pid=''
cleanup_signal_test() {
  local owned_runner=$runner_pid owned_suite=$suite_pid
  local owned_watchdog=$watchdog_pid cleanup_deadline
  runner_pid=''
  suite_pid=''
  watchdog_pid=''
  if [[ -n $owned_watchdog ]]; then
    kill "$owned_watchdog" 2>/dev/null || true
    wait "$owned_watchdog" 2>/dev/null || true
  fi
  if [[ -n $owned_runner ]] && kill -0 "$owned_runner" 2>/dev/null; then
    kill -QUIT "$owned_runner" 2>/dev/null || true
    cleanup_deadline=$((SECONDS + 2))
    while kill -0 "$owned_runner" 2>/dev/null &&
      ((SECONDS < cleanup_deadline)); do
      sleep 0.05
    done
    kill -KILL "$owned_runner" 2>/dev/null || true
    wait "$owned_runner" 2>/dev/null || true
  fi
  if [[ -z $owned_suite && -s $signal_pid_file ]]; then
    owned_suite=$(<"$signal_pid_file")
  fi
  if [[ $owned_suite =~ ^[1-9][0-9]*$ ]] &&
    kill -0 "$owned_suite" 2>/dev/null; then
    kill -KILL -- "-$owned_suite" 2>/dev/null || true
    kill -KILL "$owned_suite" 2>/dev/null || true
  fi
  rm -rf -- "$signal_root"
}
trap cleanup_signal_test EXIT
mkdir -p "$signal_home" "$signal_state" "$signal_suites"
cat >"$signal_suites/quit-test" <<EOF
#!$PREFIX/bin/bash
trap '' HUP INT QUIT TERM
printf '%s\n' "\$BASHPID" >"\$DOT_TEST_SIGNAL_PID_FILE"
while :; do sleep 1; done
EOF
chmod 0755 "$signal_suites/quit-test"
HOME=$signal_home XDG_STATE_HOME=$signal_state DOT_BASH=$PREFIX/bin/bash \
  DOT_TEST_TESTS_DIR=$signal_suites DOT_TEST_SIGNAL_PID_FILE=$signal_pid_file \
  DOT_TEST_NO_COLOR=1 "$absolute_binary" test -s >/dev/null 2>&1 &
runner_pid=$!
deadline=$((SECONDS + 15))
while [[ ! -s $signal_pid_file && $SECONDS -lt $deadline ]]; do
  sleep 0.05
done
if [[ ! -s $signal_pid_file ]]; then
  printf 'termux-ci: signal fixture did not start\n' >&2
  exit 1
fi
suite_pid=$(<"$signal_pid_file")
kill -QUIT "$runner_pid"
signal_status=0
watchdog_marker=$signal_root/watchdog-fired
(
  sleep 15
  if kill -0 "$runner_pid" 2>/dev/null; then
    : >"$watchdog_marker"
    kill -KILL "$runner_pid" 2>/dev/null || true
  fi
) &
watchdog_pid=$!
wait "$runner_pid" || signal_status=$?
runner_pid=''
kill "$watchdog_pid" 2>/dev/null || true
wait "$watchdog_pid" 2>/dev/null || true
watchdog_pid=''
[[ ! -e $watchdog_marker ]] || {
  printf 'termux-ci: SIGQUIT cancellation did not finish before deadline\n' >&2
  exit 1
}
suite_survived=0
kill -0 "$suite_pid" 2>/dev/null && suite_survived=1
if ((suite_survived)); then
  kill -KILL -- "-$suite_pid" 2>/dev/null || true
  kill -KILL "$suite_pid" 2>/dev/null || true
fi
rm -rf -- "$signal_root"
[[ $signal_status -eq 131 ]] || {
  printf 'termux-ci: SIGQUIT status was %s, expected 131\n' "$signal_status" >&2
  exit 1
}
((suite_survived == 0)) || {
  printf 'termux-ci: SIGQUIT left the suite process running\n' >&2
  exit 1
}
signal_root=''
suite_pid=''
trap - EXIT

printf 'termux-ci: ok\n'
