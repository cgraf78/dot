# shellcheck shell=bash
# Result rendering and counters shared by dot doctor section modules.
#
# Section modules intentionally report through this tiny API instead of
# manipulating counters directly. The `_dr_*` prefix marks private doctor
# internals; the public command entry point remains `_dot_doctor` in the
# parent `doctor.sh` file.

# shellcheck disable=SC2088  # tilde-paths in _dr_* arg strings are for display, not expansion

_DR_PASS_COUNT=0
_DR_WARN_COUNT=0
_DR_FAIL_COUNT=0

if [[ -t 1 && -z "${NO_COLOR:-}" ]]; then
  _DR_GREEN=$'\033[32m'
  _DR_YELLOW=$'\033[33m'
  _DR_RED=$'\033[31m'
  _DR_DIM=$'\033[2m'
  _DR_BOLD=$'\033[1m'
  _DR_RESET=$'\033[0m'
else
  _DR_GREEN='' _DR_YELLOW='' _DR_RED='' _DR_DIM='' _DR_BOLD='' _DR_RESET=''
fi

_dr_ok() {
  printf '  %s✓%s %s' "$_DR_GREEN" "$_DR_RESET" "$1"
  [[ $# -gt 1 ]] && printf ' %s(%s)%s' "$_DR_DIM" "$2" "$_DR_RESET"
  printf '\n'
  _DR_PASS_COUNT=$((_DR_PASS_COUNT + 1))
}
_dr_warn() {
  printf '  %s⚠%s %s' "$_DR_YELLOW" "$_DR_RESET" "$1"
  [[ $# -gt 1 ]] && printf '\n    %s%s%s' "$_DR_DIM" "$2" "$_DR_RESET"
  printf '\n'
  _DR_WARN_COUNT=$((_DR_WARN_COUNT + 1))
}
_dr_fail() {
  printf '  %s✗%s %s' "$_DR_RED" "$_DR_RESET" "$1"
  [[ $# -gt 1 ]] && printf '\n    %s%s%s' "$_DR_DIM" "$2" "$_DR_RESET"
  printf '\n'
  _DR_FAIL_COUNT=$((_DR_FAIL_COUNT + 1))
}
_dr_skip() {
  printf '  %s·%s %s' "$_DR_DIM" "$_DR_RESET" "$1"
  [[ $# -gt 1 ]] && printf ' %s(%s)%s' "$_DR_DIM" "$2" "$_DR_RESET"
  printf '\n'
}
_dr_section() {
  printf '\n%s%s%s\n' "$_DR_BOLD" "$1" "$_DR_RESET"
}
