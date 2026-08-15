# shellcheck shell=bash
# Dot update adapter for shdeps' JSONL progress stream.
#
# shdeps owns dependency update behavior and emits machine-readable events.
# Dot owns the terminal experience around `dot update`, so this module maps
# shdeps events into dot's generic `_ui_*` primitives without changing the
# update contract exposed by shdeps itself.

_shdeps_group_label() {
  case "$1" in
    packages) printf 'Packages' ;;
    github-releases | github-repos) printf 'GitHub' ;;
    cargo) printf 'Cargo' ;;
    go) printf 'Go' ;;
    uv) printf 'UV' ;;
    npm) printf 'NPM' ;;
    custom) printf 'Custom' ;;
    other | "") printf 'Other' ;;
    # shdeps owns group names. If a newer shdeps emits a group dot does not yet
    # know, show that key instead of collapsing distinct work under "Other".
    *) printf '%s' "$1" ;;
  esac
}

# Canonical display order of the dependency groups dot knows about. shdeps owns
# the group names; this list only fixes their order in dot's output. Groups a
# newer shdeps emits that are not listed here still render, appended in
# discovery order via DOT_UI_SHDEPS_GROUP_ORDER. Keep aligned with the keys in
# _shdeps_group_label.
_SHDEPS_KNOWN_GROUPS=(packages github-releases github-repos cargo go uv npm custom other)

# The parent holds the progress FIFO open read/write so it can keep rendering
# while shdeps is quiet. That deliberately prevents EOF from signaling normal
# completion. Have the producer append one non-JSON control line instead: FIFO
# ordering proves every earlier JSONL event has already been written, allowing
# the normal path to finish immediately while killed children still use the
# conservative process/status polling fallback below.
_SHDEPS_PROGRESS_COMPLETE=$'\036dot-shdeps-progress-complete'

_shdeps_summary_text() {
  local changed="$1" current="$2" skipped="$3" failed="$4" warnings="${5:-0}"
  local -a parts=()
  [[ "$failed" -gt 0 ]] && parts+=("$failed failed")
  [[ "$warnings" -gt 0 ]] && parts+=("$warnings warning")
  [[ "$changed" -gt 0 ]] && parts+=("$changed changed")
  [[ "$current" -gt 0 || "${#parts[@]}" -eq 0 ]] && parts+=("$current current")
  [[ "$skipped" -gt 0 ]] && parts+=("$skipped skipped")
  _join_comma "${parts[@]}"
}

_shdeps_prompt_pause() {
  DOT_UI_SHDEPS_PROMPT_ACTIVE=1
}

_shdeps_prompt_resume() {
  DOT_UI_SHDEPS_PROMPT_ACTIVE=0
}

_shdeps_ui_reset() {
  DOT_UI_SHDEPS_STATUS=ok
  DOT_UI_SHDEPS_SUMMARY="dependencies checked"
  DOT_UI_SHDEPS_HAS_JQ=0
  command -v jq >/dev/null 2>&1 && DOT_UI_SHDEPS_HAS_JQ=1
  _shdeps_prompt_resume
  DOT_UI_SHDEPS_GROUP_ORDER=()
  declare -gA DOT_UI_SHDEPS_GROUP_SEEN=()
  declare -gA DOT_UI_SHDEPS_GROUP_LABELS=()
  declare -gA DOT_UI_SHDEPS_GROUP_ITEMS=()
  declare -gA DOT_UI_SHDEPS_GROUP_SUMMARIES=()
}

_shdeps_remember_group() {
  local group="$1"
  if [[ -z "${DOT_UI_SHDEPS_GROUP_SEEN[$group]+x}" ]]; then
    DOT_UI_SHDEPS_GROUP_SEEN["$group"]=1
    DOT_UI_SHDEPS_GROUP_ORDER+=("$group")
  fi
}

_shdeps_record_item() {
  local group="$1" status="$2" name="$3" detail="$4"
  _shdeps_remember_group "$group"
  DOT_UI_SHDEPS_GROUP_ITEMS["$group"]+="${status}"$'\t'"${name}"$'\t'"${detail}"$'\n'
}

_shdeps_record_group_summary() {
  local group="$1" label="$2" status="$3" changed="$4" current="$5" skipped="$6" failed="$7" elapsed_ms="$8" warnings="${9:-0}"
  _shdeps_remember_group "$group"
  [[ -n "$label" ]] || label=$(_shdeps_group_label "$group")
  DOT_UI_SHDEPS_GROUP_LABELS["$group"]="$label"
  local detail
  detail="${label}: $(_shdeps_summary_text "${changed:-0}" "${current:-0}" "${skipped:-0}" "${failed:-0}" "${warnings:-0}")"
  DOT_UI_SHDEPS_GROUP_SUMMARIES["$group"]="${status}"$'\t'"${detail}"$'\t'"${elapsed_ms:-0}"
}

_shdeps_display_label() {
  local group="$1"
  printf '%s' "${DOT_UI_SHDEPS_GROUP_LABELS[$group]:-$(_shdeps_group_label "$group")}"
}

_shdeps_print_verbose_group_rows() {
  local label="$1" group rows status name detail seen=" "
  for group in "${_SHDEPS_KNOWN_GROUPS[@]}" "${DOT_UI_SHDEPS_GROUP_ORDER[@]+"${DOT_UI_SHDEPS_GROUP_ORDER[@]}"}"; do
    [[ "$seen" == *" $group "* ]] && continue
    seen+="$group "
    [[ "$(_shdeps_display_label "$group")" == "$label" ]] || continue
    rows="${DOT_UI_SHDEPS_GROUP_ITEMS[$group]:-}"
    [[ -n "$rows" ]] || continue
    while IFS=$'\t' read -r status name detail; do
      [[ -n "$status" ]] || continue
      _ui_item "$status" "$name" "$detail"
    done <<<"$rows"
  done
}

_shdeps_print_verbose_items() {
  [[ "${DOT_VERBOSE:-0}" -eq 1 ]] || return 0
  local group rows label seen_groups=" " seen_labels=$'\n'
  for group in "${_SHDEPS_KNOWN_GROUPS[@]}" "${DOT_UI_SHDEPS_GROUP_ORDER[@]+"${DOT_UI_SHDEPS_GROUP_ORDER[@]}"}"; do
    [[ "$seen_groups" == *" $group "* ]] && continue
    seen_groups+="$group "
    rows="${DOT_UI_SHDEPS_GROUP_ITEMS[$group]:-}"
    [[ -n "$rows" ]] || continue
    label=$(_shdeps_display_label "$group")
    [[ "$seen_labels" == *$'\n'"$label"$'\n'* ]] && continue
    seen_labels+="$label"$'\n'
    _ui_section "$label"
    _shdeps_print_verbose_group_rows "$label"
  done
}

_shdeps_print_group_items_with_status() {
  local group="$1" wanted="$2" rows status name detail
  rows="${DOT_UI_SHDEPS_GROUP_ITEMS[$group]:-}"
  [[ -n "$rows" ]] || return 0
  while IFS=$'\t' read -r status name detail; do
    [[ "$status" == "$wanted" ]] || continue
    _ui_item "$status" "$name" "$detail"
  done <<<"$rows"
}

_shdeps_print_group_summaries() {
  [[ "${DOT_VERBOSE:-0}" -eq 0 ]] || return 0
  local threshold="${DOT_UPDATE_SUBPHASE_THRESHOLD_MS:-}"
  local group record status detail elapsed_ms rendered_elapsed seen=" "
  for group in "${_SHDEPS_KNOWN_GROUPS[@]}" "${DOT_UI_SHDEPS_GROUP_ORDER[@]+"${DOT_UI_SHDEPS_GROUP_ORDER[@]}"}"; do
    [[ "$seen" == *" $group "* ]] && continue
    seen+="$group "
    record="${DOT_UI_SHDEPS_GROUP_SUMMARIES[$group]:-}"
    [[ -n "$record" ]] || continue
    IFS=$'\t' read -r status detail elapsed_ms <<<"$record"
    [[ -n "$elapsed_ms" ]] || elapsed_ms=0
    if [[ -n "$threshold" && "$elapsed_ms" -ge "$threshold" ]]; then
      rendered_elapsed=$(_ui_duration_ms "$elapsed_ms")
      _ui_stage_note "$status" "$detail, $rendered_elapsed"
      [[ "$status" == "changed" ]] && _shdeps_print_group_items_with_status "$group" changed
      # Failed rows are the ones a user must act on; anonymous failure counts
      # made real incidents (rate-limited GitHub fetches) opaque.
      [[ "$status" == "failed" ]] && _shdeps_print_group_items_with_status "$group" failed
      [[ "$status" == "warning" ]] && _shdeps_print_group_items_with_status "$group" warning
      continue
    fi
    case "$status" in
      changed)
        _ui_stage_note "$status" "$detail"
        _shdeps_print_group_items_with_status "$group" changed
        continue
        ;;
      ok | skipped) continue ;;
    esac
    rendered_elapsed=$(_ui_duration_ms "$elapsed_ms")
    _ui_stage_note "$status" "$detail, $rendered_elapsed"
    [[ "$status" == "failed" ]] && _shdeps_print_group_items_with_status "$group" failed
    [[ "$status" == "warning" ]] && _shdeps_print_group_items_with_status "$group" warning
  done
  return 0
}

_shdeps_parse_event() {
  local line="$1" field
  local -a fields=()

  if [[ "${DOT_UI_SHDEPS_HAS_JQ:-0}" -eq 1 ]]; then
    while IFS= read -r -d '' field; do
      fields+=("$field")
    done < <(
      printf '%s' "$line" | jq -j '
        def text($key):
          .[$key] | if type == "string" then . else "" end;
        def number($key):
          .[$key] | if type == "number" then tostring else "" end;
        [
          text("event"), text("group"), text("label"), text("status"),
          text("detail"), number("done"), number("total"), text("name"),
          number("changed"), number("warnings"), number("current"),
          number("skipped"), number("failed"), number("elapsed_ms")
        ] | .[] + "\u0000"
      ' 2>/dev/null
    )
    if [[ "${#fields[@]}" -eq 14 ]]; then
      SHDEPS_EVENT="${fields[0]}"
      SHDEPS_GROUP="${fields[1]}"
      SHDEPS_LABEL="${fields[2]}"
      SHDEPS_STATUS="${fields[3]}"
      SHDEPS_DETAIL="${fields[4]}"
      SHDEPS_DONE="${fields[5]}"
      SHDEPS_TOTAL="${fields[6]}"
      SHDEPS_NAME="${fields[7]}"
      SHDEPS_CHANGED="${fields[8]}"
      SHDEPS_WARNINGS="${fields[9]}"
      SHDEPS_CURRENT="${fields[10]}"
      SHDEPS_SKIPPED="${fields[11]}"
      SHDEPS_FAILED="${fields[12]}"
      SHDEPS_ELAPSED_MS="${fields[13]}"
      return 0
    fi
  fi

  return 1
}

_handle_shdeps_event() {
  local line="$1"
  local event group label status detail done_count total name changed warnings current skipped failed elapsed_ms
  local parsed=0
  if _shdeps_parse_event "$line"; then
    parsed=1
    event="$SHDEPS_EVENT"
    group="$SHDEPS_GROUP"
    label="$SHDEPS_LABEL"
    status="$SHDEPS_STATUS"
    detail="$SHDEPS_DETAIL"
    done_count="$SHDEPS_DONE"
    total="$SHDEPS_TOTAL"
    name="$SHDEPS_NAME"
    changed="$SHDEPS_CHANGED"
    warnings="$SHDEPS_WARNINGS"
    current="$SHDEPS_CURRENT"
    skipped="$SHDEPS_SKIPPED"
    failed="$SHDEPS_FAILED"
    elapsed_ms="$SHDEPS_ELAPSED_MS"
  else
    # Minimal/bootstrap environments retain the old lazy per-event parsing,
    # avoiding work for fields that the current event cannot use.
    event=$(_json_get event "$line")
  fi
  case "$event" in
    prompt)
      _shdeps_prompt_pause
      ;;
    phase)
      _shdeps_prompt_resume
      if [[ "$parsed" -eq 0 ]]; then
        label=$(_json_get label "$line")
        done_count=$(_json_num "done" "$line")
        total=$(_json_num total "$line")
      fi
      if [[ -n "$done_count" && -n "$total" && "$total" -gt 0 ]]; then
        _ui_stage_update "$(_ui_progress_detail_with_label "$label" "$done_count" "$total")"
      else
        _ui_stage_update "$label"
      fi
      ;;
    item)
      _shdeps_prompt_resume
      if [[ "$parsed" -eq 0 ]]; then
        group=$(_json_get group "$line")
        status=$(_json_get status "$line")
        name=$(_json_get name "$line")
        detail=$(_json_get detail "$line")
      fi
      _shdeps_record_item "$group" "$status" "$name" "$detail"
      ;;
    group_summary)
      _shdeps_prompt_resume
      if [[ "$parsed" -eq 0 ]]; then
        group=$(_json_get group "$line")
        label=$(_json_get label "$line")
        status=$(_json_get status "$line")
        changed=$(_json_num changed "$line")
        warnings=$(_json_num warnings "$line")
        current=$(_json_num current "$line")
        skipped=$(_json_num skipped "$line")
        failed=$(_json_num failed "$line")
        elapsed_ms=$(_json_num elapsed_ms "$line")
      fi
      _shdeps_record_group_summary "$group" "$label" "$status" \
        "${changed:-0}" "${current:-0}" "${skipped:-0}" "${failed:-0}" "${elapsed_ms:-0}" "${warnings:-0}"
      ;;
    summary)
      _shdeps_prompt_resume
      if [[ "$parsed" -eq 0 ]]; then
        status=$(_json_get status "$line")
        changed=$(_json_num changed "$line")
        warnings=$(_json_num warnings "$line")
        current=$(_json_num current "$line")
        skipped=$(_json_num skipped "$line")
        failed=$(_json_num failed "$line")
      fi
      # shellcheck disable=SC2034  # consumed by update.sh after shdeps exits.
      DOT_UI_SHDEPS_STATUS="$status"
      # shellcheck disable=SC2034  # consumed by update.sh after shdeps exits.
      DOT_UI_SHDEPS_SUMMARY=$(_shdeps_summary_text "${changed:-0}" "${current:-0}" "${skipped:-0}" "${failed:-0}" "${warnings:-0}")
      ;;
    warning | detail | hint)
      _shdeps_prompt_resume
      if [[ "$parsed" -eq 0 ]]; then
        status=$(_json_get status "$line")
        detail=$(_json_get detail "$line")
      fi
      _ui_stage_note "${status:-$event}" "$detail"
      ;;
  esac
}

_shdeps_proc_state() {
  local pid="$1" root="${DOT_PROC_ROOT:-/proc}" line rest state

  [[ "$pid" =~ ^[0-9]+$ ]] || return 1
  IFS= read -r line 2>/dev/null <"$root/$pid/stat" || return 1
  [[ "$line" == "$pid ("* ]] || return 1
  # A process name may contain ')', so split after the last closing delimiter.
  rest="${line##*) }"
  [[ "$rest" != "$line" ]] || return 1
  state="${rest%% *}"
  [[ "$state" =~ ^[A-Za-z]$ ]] || return 1
  printf '%s\n' "$state"
}

_shdeps_update_finished() {
  local child="$1" status_file="$2" state

  # The status is authoritative for normal completion. On some runners the
  # child remains visible to kill(2) as a zombie until the wait below reaps it.
  [[ -s "$status_file" ]] && return 0
  kill -0 "$child" 2>/dev/null || return 0
  if state=$(_shdeps_proc_state "$child"); then
    [[ "$state" == Z ]]
    return
  fi
  state=$(ps -o stat= -p "$child" 2>/dev/null) || return 0
  [[ "$state" =~ ^[[:space:]]*Z ]]
}

_run_shdeps_update_ui() {
  local status_file="" fifo="" tmpdir=""
  _dot_update_prepare_shdeps_jobs
  if ! _dot_cleanup_mktemp -d 2>/dev/null; then
    _run_shdeps_update_command
    return $?
  fi
  tmpdir=$REPLY
  status_file="$tmpdir/status"
  fifo="$tmpdir/progress"
  if ! mkfifo "$fifo" 2>/dev/null; then
    _dot_cleanup_remove_path "$tmpdir" || true
    _run_shdeps_update_command
    return $?
  fi
  _shdeps_ui_reset
  local line progress_fd child rc=0
  _dot_cleanup_begin_registration
  exec {progress_fd}<>"$fifo"
  _dot_cleanup_register_fd "$progress_fd"
  _dot_cleanup_end_registration
  _dot_cleanup_begin_job_launch
  (
    _dot_cleanup_prepare_subshell
    _run_shdeps_update_command jsonl >"$fifo"
    printf '%s' "$?" >"$status_file"
    printf '%s\n' "$_SHDEPS_PROGRESS_COMPLETE" >"$fifo"
  ) <&"$DOT_CLEANUP_LAUNCH_STDIN_FD" &
  child=$!
  _dot_cleanup_finish_job_launch "$child"
  while :; do
    if IFS= read -r -t "${DOT_UI_TICK_SECONDS:-0.2}" -u "$progress_fd" line; then
      if [[ "$line" == *"$_SHDEPS_PROGRESS_COMPLETE" ]]; then
        line=${line%"$_SHDEPS_PROGRESS_COMPLETE"}
        [[ -z "$line" ]] || _handle_shdeps_event "$line"
        break
      fi
      _handle_shdeps_event "$line"
      continue
    fi
    if _shdeps_update_finished "$child" "$status_file"; then
      # Child exited (cleanly or killed). Drain any complete lines still buffered
      # in the fifo, then stop. A normal child publishes status before exiting;
      # a child killed abnormally may instead disappear or remain a zombie.
      # Cover all three because the parent holds the fifo open R/W, so EOF cannot
      # distinguish completion. An empty status file becomes rc=1 below.
      while IFS= read -r -t "${DOT_UI_TICK_SECONDS:-0.2}" -u "$progress_fd" line; do
        if [[ "$line" == *"$_SHDEPS_PROGRESS_COMPLETE" ]]; then
          line=${line%"$_SHDEPS_PROGRESS_COMPLETE"}
          [[ -z "$line" ]] || _handle_shdeps_event "$line"
          break
        fi
        _handle_shdeps_event "$line"
      done
      break
    fi
    [[ "${DOT_UI_SHDEPS_PROMPT_ACTIVE:-0}" -eq 1 ]] || _ui_stage_tick
  done
  wait "$child" 2>/dev/null || true
  _dot_cleanup_unregister_pid "$child"
  _dot_cleanup_close_fd "$progress_fd"
  _shdeps_print_verbose_items
  rc=$(cat "$status_file" 2>/dev/null || printf '1')
  _dot_cleanup_remove_path "$tmpdir" || true
  return "$rc"
}

_run_shdeps_update_command() {
  local progress_mode="${1:-}"
  if [[ "${DOT_SHDEPS_ALLOW_GH_AUTH_TOKEN:-0}" -eq 1 && -z "${SHDEPS_ALLOW_GH_AUTH_TOKEN+x}" ]]; then
    if [[ "$progress_mode" == "jsonl" ]]; then
      SHDEPS_ALLOW_GH_AUTH_TOKEN=1 SHDEPS_NESTED=1 SHDEPS_PROGRESS=jsonl shdeps_update
    else
      SHDEPS_ALLOW_GH_AUTH_TOKEN=1 SHDEPS_NESTED=1 shdeps_update
    fi
  elif [[ "$progress_mode" == "jsonl" ]]; then
    SHDEPS_NESTED=1 SHDEPS_PROGRESS=jsonl shdeps_update
  else
    SHDEPS_NESTED=1 shdeps_update
  fi
}
