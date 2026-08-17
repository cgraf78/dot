# shellcheck shell=bash
# Live progress and status-line helpers for `dot update`.

_ui_color() {
  case "$1" in
    ok) printf '%s' "$_C_GREEN" ;;
    changed) printf '%s' "$_C_BLUE" ;;
    running) printf '%s' "$_C_CYAN" ;;
    warning) printf '%s' "$_C_YELLOW" ;;
    failed) printf '%s' "$_C_RED" ;;
    detail | hint) printf '%s' "$_C_DIM" ;;
    *) printf '%s' "" ;;
  esac
}

_ui_ascii_mode() {
  local locale="${LC_ALL:-${LC_CTYPE:-${LANG:-}}}"
  [[ "${DOT_UI_ASCII:-0}" -eq 1 || "$locale" == C* || "$locale" == POSIX ]] &&
    return 0

  # Some CI images advertise a UTF-8 locale that Bash cannot actually use for
  # multibyte length calculations. In that state our fixed-width cells truncate
  # Unicode progress glyphs by byte count, so prefer the stable ASCII renderer.
  local glyph="━"
  [[ ${#glyph} -ne 1 ]]
}

_ui_begin() {
  DOT_UI_TOTAL="$1"
  DOT_UI_INDEX=0
  DOT_UI_STARTED="$SECONDS"
  DOT_UI_LIVE_ACTIVE=0
}

_ui_elapsed() {
  local started="${1:-${SECONDS:-0}}"
  printf '%ss' "$((${SECONDS:-0} - started))"
}

_ui_now_ms() {
  local now=""
  now=$(date +%s%3N 2>/dev/null || true)
  if [[ "$now" =~ ^[0-9]+$ ]]; then
    printf '%s' "$now"
  else
    printf '%s' "$((${SECONDS:-0} * 1000))"
  fi
}

_ui_live_enabled() {
  [[ "${DOT_QUIET:-0}" -ne 1 ]] || return 1
  [[ -t 1 || "${DOT_UI_FORCE_LIVE:-0}" -eq 1 ]]
}

# Right-pad text to width. With truncate=1, over-width text is cut to width
# (fixed-width columns); with truncate=0 it is left intact (min-width padding).
_ui_fit() {
  local text="$1" width="$2" truncate="${3:-0}" len pad
  len=${#text}
  if [[ "$len" -ge "$width" ]]; then
    if [[ "$truncate" -eq 1 && "$len" -gt "$width" ]]; then
      printf '%s' "${text:0:width}"
    else
      printf '%s' "$text"
    fi
    return 0
  fi
  pad=$((width - len))
  printf '%s%*s' "$text" "$pad" ''
}

# Fixed-width cell: pad to width, truncate if longer.
_ui_cell() { _ui_fit "$1" "$2" 1; }

# Minimum-width pad: pad to width, never truncate.
_ui_pad() { _ui_fit "$1" "$2" 0; }

_ui_clear_live() {
  if [[ "${DOT_UI_LIVE_ACTIVE:-0}" -eq 1 ]]; then
    printf '\r\033[K'
    DOT_UI_LIVE_ACTIVE=0
  fi
}

_ui_line() {
  [[ "$DOT_QUIET" -eq 1 ]] && return 0
  local index="$1" total="$2" label="$3" status="$4" detail="$5" elapsed="$6"
  local color label_cell status_cell detail_cell
  color="$(_ui_color "$status")"
  label_cell=$(_ui_cell "$label" 10)
  status_cell=$(_ui_cell "$status" 8)
  detail_cell=$(_ui_cell "$detail" 42)
  printf '%s[%s/%s]%s %s %s%s%s %s %6s\n' \
    "$_C_CYAN" "$index" "$total" "$_C_RESET" \
    "$label_cell" "$color" "$status_cell" "$_C_RESET" "$detail_cell" "$elapsed"
}

_ui_live_line() {
  [[ "$DOT_QUIET" -eq 1 ]] && return 0
  local index="$1" total="$2" label="$3" status="$4" detail="$5" elapsed="$6"
  local color status_text label_cell status_cell detail_cell
  color="$(_ui_color "$status")"
  status_text="$status"
  if [[ "$status" == "running" ]]; then
    local -a frames
    if _ui_ascii_mode; then
      frames=("/" "-" "\\" "|")
    else
      frames=("⠋" "⠙" "⠹" "⠸" "⠼" "⠴" "⠦" "⠧" "⠇" "⠏")
    fi
    local frame_index="${DOT_UI_SPINNER_INDEX:-0}"
    status_text="${frames[$((frame_index % ${#frames[@]}))]}"
    DOT_UI_SPINNER_INDEX=$(((frame_index + 1) % ${#frames[@]}))
  fi
  label_cell=$(_ui_cell "$label" 10)
  status_cell=$(_ui_cell "$status_text" 8)
  detail_cell=$(_ui_cell "$detail" 42)
  printf '\r\033[K%s[%s/%s]%s %s %s%s%s %s %6s' \
    "$_C_CYAN" "$index" "$total" "$_C_RESET" \
    "$label_cell" "$color" "$status_cell" "$_C_RESET" "$detail_cell" "$elapsed"
  DOT_UI_LIVE_ACTIVE=1
}

_ui_stage_start() {
  [[ "$DOT_QUIET" -eq 1 ]] && return 0
  local label="$1"
  local detail="${2:-working}"
  DOT_UI_INDEX=$((${DOT_UI_INDEX:-0} + 1))
  DOT_UI_STAGE_LABEL="$label"
  DOT_UI_STAGE_DETAIL="$detail"
  DOT_UI_STAGE_STARTED="$SECONDS"
  DOT_UI_SPINNER_INDEX=0
  if _ui_live_enabled; then
    [[ "${DOT_VERBOSE:-0}" -eq 1 ]] &&
      _ui_line "$DOT_UI_INDEX" "${DOT_UI_TOTAL:-0}" "$label" running "$detail" "0s"
    _ui_live_line "$DOT_UI_INDEX" "${DOT_UI_TOTAL:-0}" "$label" running "$detail" "0s"
  else
    _ui_line "$DOT_UI_INDEX" "${DOT_UI_TOTAL:-0}" "$label" running "$detail" "0s"
  fi
}

_ui_stage_update() {
  [[ "$DOT_QUIET" -eq 1 ]] && return 0
  local detail="$1"
  DOT_UI_STAGE_DETAIL="$detail"
  if _ui_live_enabled; then
    _ui_live_line \
      "${DOT_UI_INDEX:-0}" \
      "${DOT_UI_TOTAL:-0}" \
      "${DOT_UI_STAGE_LABEL:-}" \
      running \
      "$detail" \
      "$(_ui_elapsed "${DOT_UI_STAGE_STARTED:-${SECONDS:-0}}")"
  fi
}

_ui_stage_tick() {
  [[ "$DOT_QUIET" -eq 1 ]] && return 0
  _ui_live_enabled || return 0
  _ui_live_line \
    "${DOT_UI_INDEX:-0}" \
    "${DOT_UI_TOTAL:-0}" \
    "${DOT_UI_STAGE_LABEL:-}" \
    running \
    "${DOT_UI_STAGE_DETAIL:-working}" \
    "$(_ui_elapsed "${DOT_UI_STAGE_STARTED:-${SECONDS:-0}}")"
}

_ui_stage_finish() {
  [[ "$DOT_QUIET" -eq 1 ]] && return 0
  local status="$1"
  local detail="$2"
  local elapsed
  elapsed="$(_ui_elapsed "${DOT_UI_STAGE_STARTED:-${SECONDS:-0}}")"
  _ui_clear_live
  _ui_line \
    "${DOT_UI_INDEX:-0}" \
    "${DOT_UI_TOTAL:-0}" \
    "${DOT_UI_STAGE_LABEL:-}" \
    "$status" \
    "$detail" \
    "$elapsed"
}

_ui_stage_note() {
  [[ "$DOT_QUIET" -eq 1 ]] && return 0
  local status="$1" detail="$2"
  _ui_status "$status" "$detail"
}

_ui_stage() {
  [[ "$DOT_QUIET" -eq 1 ]] && return 0
  local label="$1"
  DOT_UI_INDEX=$((${DOT_UI_INDEX:-0} + 1))
  if [[ "${DOT_UI_TOTAL:-0}" -gt 0 ]]; then
    _log_header "${_C_CYAN}[$DOT_UI_INDEX/$DOT_UI_TOTAL]${_C_RESET} ${_C_BOLD}${_C_WHITE}$label${_C_RESET}"
  else
    _log_header "${_C_BOLD}${_C_WHITE}$label${_C_RESET}"
  fi
}

_ui_status() {
  [[ "$DOT_QUIET" -eq 1 ]] && return 0
  local status="$1"
  local detail="$2"
  local color status_cell
  _ui_clear_live
  color="$(_ui_color "$status")"
  status_cell=$(_ui_cell "$status" 8)
  printf '  %s%s%s %s\n' "$color" "$status_cell" "$_C_RESET" "$detail"
}

_ui_section() {
  [[ "$DOT_QUIET" -eq 1 ]] && return 0
  local title="$1"
  _ui_clear_live
  printf '  %s%s%s\n' "$_C_BOLD$_C_WHITE" "$title" "$_C_RESET"
}

_ui_detail() {
  [[ "$DOT_QUIET" -eq 1 ]] && return 0
  local detail="$1"
  _ui_clear_live
  printf '    %s%s%s\n' "$_C_DIM" "$detail" "$_C_RESET"
}

_ui_item() {
  [[ "$DOT_QUIET" -eq 1 ]] && return 0
  local status="$1" name="$2" detail="${3:-}"
  local color status_cell name_cell
  _ui_clear_live
  color="$(_ui_color "$status")"
  status_cell=$(_ui_cell "$status" 8)
  name_cell=$(_ui_pad "$name" 28)
  if [[ -n "$detail" ]]; then
    printf '  %s%s%s %s %s%s%s\n' \
      "$color" "$status_cell" "$_C_RESET" "$name_cell" "$_C_DIM" "$detail" "$_C_RESET"
  else
    printf '  %s%s%s %s\n' "$color" "$status_cell" "$_C_RESET" "$name"
  fi
}

_ui_normal_shell_name() {
  local shell_name="$1"
  shell_name="${shell_name##*/}"
  shell_name="${shell_name#-}"
  case "$shell_name" in
    bash | zsh)
      printf '%s' "$shell_name"
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

_ui_parent_shell_name() {
  local parent=""
  if [[ -n "${PPID:-}" && -r "/proc/$PPID/comm" ]]; then
    IFS= read -r parent <"/proc/$PPID/comm" || parent=""
    _ui_normal_shell_name "$parent" && return 0
  fi

  if [[ -n "${PPID:-}" ]] && command -v ps >/dev/null 2>&1; then
    parent=$(ps -p "$PPID" -o comm= 2>/dev/null || true)
    parent="${parent%%$'\n'*}"
    parent="${parent#"${parent%%[![:space:]]*}"}"
    parent="${parent%"${parent##*[![:space:]]}"}"
    _ui_normal_shell_name "$parent" && return 0
  fi

  return 1
}

_ui_shell_reload_hint() {
  [[ "${DOT_UPDATE_RELOADS_SHELL:-0}" -eq 1 ]] && return 0
  local shell_name=""
  shell_name="$(_ui_parent_shell_name || true)"
  [[ -n "$shell_name" ]] || shell_name="$(_ui_normal_shell_name "${SHELL:-}" || true)"
  case "$shell_name" in
    bash) printf 'Reload your shell: source ~/.bashrc' ;;
    zsh) printf 'Reload your shell: source ~/.zshrc' ;;
    *)
      if [[ -f "$HOME/.bashrc" && ! -f "$HOME/.zshrc" ]]; then
        printf 'Reload your shell: source ~/.bashrc'
      else
        printf 'Reload your shell: source ~/.zshrc'
      fi
      ;;
  esac
}

_ui_done() {
  [[ "$DOT_QUIET" -eq 1 ]] && return 0
  local status="${1:-0}"
  local elapsed=$((${SECONDS:-0} - ${DOT_UI_STARTED:-${SECONDS:-0}}))
  local hint message="Done"
  [[ "$status" -eq 0 ]] || message="Done with errors"
  hint="$(_ui_shell_reload_hint)"
  if [[ -n "$hint" ]]; then
    printf '%s%s in %ss.%s %s\n' \
      "$_C_BOLD$_C_WHITE" "$message" "$elapsed" "$_C_RESET" "$hint"
  else
    printf '%s%s in %ss%s\n' \
      "$_C_BOLD$_C_WHITE" "$message" "$elapsed" "$_C_RESET"
  fi
}

# Extract a string field from one JSONL event. Prefer a real decoder: the sed
# fallback's `[^"]*` truncates values containing an escaped quote and its greedy
# `.*` can match the wrong field. The fallback stays for minimal/bootstrap
# environments that run before jq is installed.
_json_get() {
  local key="$1" line="$2"
  if command -v jq >/dev/null 2>&1; then
    printf '%s' "$line" | jq -r --arg k "$key" '.[$k] // empty' 2>/dev/null
  else
    sed -n "s/.*\"$key\":\"\\([^\"]*\\)\".*/\\1/p" <<<"$line"
  fi
}

# Extract a numeric field from one JSONL event (empty if absent or non-numeric).
_json_num() {
  local key="$1" line="$2"
  if command -v jq >/dev/null 2>&1; then
    printf '%s' "$line" | jq -r --arg k "$key" 'if (.[$k] | type) == "number" then .[$k] else empty end' 2>/dev/null
  else
    sed -n "s/.*\"$key\":\\([0-9][0-9]*\\).*/\\1/p" <<<"$line"
  fi
}

_join_comma() {
  local out="" item
  for item in "$@"; do
    [[ -n "$item" ]] || continue
    if [[ -n "$out" ]]; then
      out+=", $item"
    else
      out="$item"
    fi
  done
  printf '%s' "$out"
}

_ui_count_phrase() {
  local count="$1" singular="$2" plural
  plural="${3:-${singular}s}"
  if [[ "$count" -eq 1 ]]; then
    printf '1 %s' "$singular"
  else
    printf '%s %s' "$count" "$plural"
  fi
}

_ui_duration_ms() {
  local ms="${1:-0}"
  if [[ "$ms" -ge 10000 ]]; then
    printf '%ss' $(((ms + 500) / 1000))
  elif [[ "$ms" -ge 1000 ]]; then
    printf '%s.%ss' "$((ms / 1000))" "$(((ms % 1000) / 100))"
  else
    printf '%sms' "$ms"
  fi
}

_ui_progress_bar() {
  local done="$1" total="$2" width="${DOT_UI_PROGRESS_WIDTH:-8}"
  [[ "$total" -gt 0 ]] || return 0
  local filled=$(((done * width) / total))
  [[ "$filled" -gt "$width" ]] && filled="$width"
  local empty=$((width - filled))
  local fill_char empty_char
  if _ui_ascii_mode; then
    fill_char="#"
    empty_char="-"
  else
    fill_char="━"
    empty_char="·"
  fi
  local bar=""
  local i
  for ((i = 0; i < filled; i++)); do bar+="$fill_char"; done
  for ((i = 0; i < empty; i++)); do bar+="$empty_char"; done
  local digits=${#total}
  [[ ${#done} -gt "$digits" ]] && digits=${#done}
  printf '[%s] %*s/%s' "$bar" "$digits" "$done" "$total"
}

_ui_progress_detail_with_label() {
  local label="$1" done="$2" total="$3" suffix="${4:-}"
  local label_width="${5:-${DOT_UI_PROGRESS_LABEL_WIDTH:-18}}"
  printf '%s %s' "$(_ui_cell "$label" "$label_width")" \
    "$(_ui_progress_bar "$done" "$total")"
  [[ -n "$suffix" ]] && printf ' %s' "$suffix"
  return 0
}
