# shellcheck shell=bash
# Sourced command stages consume these options and invoke the nested helpers.
# shellcheck disable=SC2034,SC2329

_DOT_TEST_DIR=${BASH_SOURCE[0]%/*}/test
# Built-in and client-extension test coordinator.
#
# Auto-discovers and runs all *-test scripts in ~/.local/lib/dotfiles/tests/.
# Runs in parallel by default for faster execution.
#
# Usage:
#   dot test                  run all tests in parallel
#   dot test core bootstrap   run named tests only
#   dot test -s               run sequentially (stream output)
#   dot test -v               verbose: show all output in parallel mode
#   dot test -j 4             run up to 4 suites concurrently
#   dot test -l               list available tests

dot_test_command() {
  # The scheduler records each child status explicitly. Inheriting the CLI's
  # errexit would terminate a worker wrapper before it can publish that status.
  set +e
  set -o pipefail

  # shellcheck source=test/source.sh
  . "$_DOT_TEST_DIR/source.sh"

  # Classify a finished suite from its exit code and private result record.
  #
  # A zero exit is NOT sufficient proof of success. Suites do not run under
  # `set -e`, so a setup command that errors mid-run (a failed `cd`, `mkdir`,
  # fixture build, or an early `return`) lets the suite keep going and exit 0 while
  # skipping the assertions that would have caught the problem — a false green.
  _classify_suite() {
    local rc="$1" result="$2" kind first second line_count last_byte
    if [[ "$rc" -ne 0 ]]; then
      printf 'fail\n'
    elif [[ ! -s $result ]]; then
      printf 'incomplete\n'
    else
      line_count=$(LC_ALL=C wc -l <"$result" 2>/dev/null | tr -d '[:space:]')
      last_byte=$(tail -c 1 "$result" 2>/dev/null | od -An -t u1 | tr -d '[:space:]')
      if [[ $line_count != 1 || $last_byte != 10 ]]; then
        printf 'invalid\n'
        return
      fi
      IFS=$'\t' read -r kind first second <"$result" || true
      if [[ $kind == complete && $first =~ ^(0|[1-9][0-9]*)$ &&
        $second =~ ^(0|[1-9][0-9]*)$ ]]; then
        if [[ $second == 0 ]]; then
          printf 'pass\n'
        else
          printf 'fail\n'
        fi
      elif [[ $kind == skip && -z $second ]]; then
        printf 'skip\n'
      else
        printf 'invalid\n'
      fi
    fi
  }

  parallel=true
  verbose=false
  max_jobs="${DOT_TEST_JOBS:-}"
  list_only=false
  filter=()
  while [[ $# -gt 0 ]]; do
    case "$1" in
      -s | --sequential)
        parallel=false
        shift
        ;;
      -v | --verbose)
        verbose=true
        shift
        ;;
      -j | --jobs)
        [[ $# -ge 2 ]] || {
          echo "missing value for $1" >&2
          exit 2
        }
        max_jobs="$2"
        shift 2
        ;;
      --jobs=*)
        max_jobs="${1#*=}"
        shift
        ;;
      -l | --list)
        list_only=true
        shift
        ;;
      -h | --help)
        printf '%s\n' \
          'usage: dot test [-s|--sequential] [-v|--verbose] [-j N|--jobs N] [--list] [name ...]'
        exit 0
        ;;
      -*)
        echo "unknown option: $1" >&2
        exit 2
        ;;
      *)
        filter+=("$1")
        shift
        ;;
    esac
  done

  # Detect gum for styled output. Test suites set NO_COLOR to keep underlying
  # tool output deterministic; Dot owns its own presentation layer, with a
  # dedicated escape hatch for plain logs.
  _color=true
  [[ "${DOT_TEST_NO_COLOR:-0}" = 1 ]] && _color=false
  _child_style=0
  if { $_color && dot_ui_has_gum; } || { $_color && [[ -t 1 ]]; }; then
    _child_style=1
  fi

  _ansi() {
    local color="$1"
    shift
    if $_color; then
      case "$color" in
        bold) printf '\033[1m%s\033[0m\n' "$*" ;;
        *) printf '\033[38;2;%sm%s\033[0m\n' "$(dot_ui_hex_to_rgb "$(dot_ui_color_hex "$color")")" "$*" ;;
      esac
    else
      echo "$*"
    fi
  }

  _styled() {
    local color="$1"
    shift
    if $_color; then
      # Status colors are semantic, not theme colors. Use explicit RGB ANSI so
      # Neovim terminals and external terminals render pass/fail consistently.
      _ansi "$color" "$*"
    else
      echo "$*"
    fi
  }

  _header() {
    if $_color; then
      _ansi bold "$*"
    else
      echo "$*"
    fi
  }

  _title() {
    if $_color; then
      dot_ui_title "$*"
    else
      printf '\n'
      echo "$*"
      printf '\n'
    fi
  }

  _summary_box() {
    local color="$1"
    shift
    if $_color; then
      echo
      dot_ui_summary_box "$color" "$*"
    else
      _header "════════════════════════════════"
      _styled "$color" "$*"
      _header "════════════════════════════════"
    fi
  }

  _mark() {
    local color="$1" glyph="$2" name="$3" elapsed="$4" detail=${5:-}
    if $_color; then
      # Match `dot doctor`: color only the status glyph, leaving the label and
      # detail text in the terminal default color for easier scanning.
      printf '  '
      printf '\033[38;2;%sm%s\033[0m' "$(dot_ui_hex_to_rgb "$(dot_ui_color_hex "$color")")" "$glyph"
      printf ' %s (%s)' "$name" "$elapsed"
    else
      printf '  %s %s (%s)' "$glyph" "$name" "$elapsed"
    fi
    [[ -z $detail ]] || printf ': %s' "$detail"
    printf '\n'
  }

  _mark_pass() { _mark green "✓" "$1" "$2"; }
  _mark_fail() { _mark red "✗" "$1" "$2"; }
  _mark_skip() { _mark yellow "○" "$1" "$2" "${3:-}"; }

  _default_jobs() {
    local n="" default_cap=24
    n=$(getconf _NPROCESSORS_ONLN 2>/dev/null || true)
    if [[ -z "$n" && "$(uname -s)" = "Darwin" ]]; then
      n=$(sysctl -n hw.ncpu 2>/dev/null || true)
    fi
    case "$n" in
      '' | *[!0-9]*) n=4 ;;
    esac
    if [[ ${#n} -gt 9 ]]; then
      n=4
    else
      n=$((10#$n))
    fi
    [[ "$n" -lt 1 ]] && n=1

    # Bound peak host load on high-core machines. Measured wall time is flat from
    # 24 workers through unbounded 44/55-way runs, while 12 workers does lengthen
    # the critical path. Cap only automatic selection so explicit -j and
    # DOT_TEST_JOBS requests still win.
    [[ "$n" -gt "$default_cap" ]] && n=$default_cap
    printf '%s\n' "$n"
  }

  _dot_test_runs_early() {
    local script="$1" line scanned=0

    # Keep priority self-contained with the suite, but inspect only its header.
    # The bound makes discovery cost independent of suite size and avoids treating
    # a fixture or assertion in the test body as scheduling metadata.
    while IFS= read -r line; do
      [[ "$line" == "# dot-suite-priority: early" ]] && return 0
      scanned=$((scanned + 1))
      [[ "$scanned" -ge 20 ]] && break
    done <"$script"
    return 1
  }

  # shellcheck source=test/discovery.sh
  . "$_DOT_TEST_DIR/discovery.sh"
  # shellcheck source=test/runner.sh
  . "$_DOT_TEST_DIR/runner.sh"

}
