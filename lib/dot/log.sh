# shellcheck shell=bash
# Logging and color helpers shared by dot runtime modules.

# ---------------------------------------------------------------------------
# Colors — disabled when not a terminal or NO_COLOR is set.
# ---------------------------------------------------------------------------

if [[ -t 1 && -z "${NO_COLOR:-}" ]]; then
  _C_RESET=$'\033[0m'
  _C_BOLD=$'\033[1m'
  _C_DIM=$'\033[0;90m'
  _C_GREEN=$'\033[32m'
  _C_YELLOW=$'\033[33m'
  _C_RED=$'\033[31m'
  _C_BLUE=$'\033[34m'
  _C_CYAN=$'\033[36m'
  _C_WHITE=$'\033[38;2;255;255;255m'
else
  _C_RESET="" _C_BOLD="" _C_DIM="" _C_GREEN="" _C_YELLOW=""
  _C_RED="" _C_BLUE="" _C_CYAN="" _C_WHITE=""
fi

# Print a message unless quiet mode is active.
_log() {
  [[ "$DOT_QUIET" -eq 1 ]] || echo "$@"
}

# Section header (bright bold white, always prints).
_header() {
  echo "${_C_BOLD}${_C_WHITE}$*${_C_RESET}"
}

# Section header (bright bold white, respects quiet mode).
_log_header() {
  [[ "$DOT_QUIET" -eq 1 ]] || echo "${_C_BOLD}${_C_WHITE}$*${_C_RESET}"
}

# Success message (green, respects quiet mode).
_log_ok() {
  [[ "$DOT_QUIET" -eq 1 ]] || echo "${_C_GREEN}$*${_C_RESET}"
}

# Muted message (dim, respects quiet mode).
_log_dim() {
  [[ "$DOT_QUIET" -eq 1 ]] || echo "${_C_DIM}$*${_C_RESET}"
}

# Warning message (yellow, always prints to stderr).
_warn() {
  echo "${_C_YELLOW}$*${_C_RESET}" >&2
}
