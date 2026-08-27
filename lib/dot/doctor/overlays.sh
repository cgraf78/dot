# shellcheck shell=bash
# dot doctor: Overlays checks.

_dr_check_overlays() {
  local conf_count=${#CONFIGURED_OVERLAY_NAMES[@]} manifest="$DOT_OVERLAY_MANIFEST"
  local entry name path url optional sync lifecycle state source
  local selector source_class selector_path _selector_user _selector_host selector_profile matched

  _dr_section 'Profiles'
  if [[ ${DOT_PROFILES_PRESENT:-0} -eq 0 ]]; then
    _dr_skip 'profile selection disabled' 'no profiles.d directory; using legacy overlay discovery'
  else
    if [[ -n ${DOT_PROFILE_CONFIGURATION_ERROR:-} ]]; then
      _dr_fail 'profile configuration invalid' "$DOT_PROFILE_CONFIGURATION_ERROR"
    fi
    if [[ -n ${DOT_PROFILE_CURRENT_USER:-} && -n ${DOT_PROFILE_CURRENT_HOST:-} ]]; then
      _dr_ok 'profile identity' "$DOT_PROFILE_CURRENT_USER@$DOT_PROFILE_CURRENT_HOST"
    fi
    if [[ -n ${SELECTED_PROFILE:-} ]]; then
      _dr_ok 'selected profile' "$SELECTED_PROFILE (${DOT_PROFILE_SELECTION_STATE:-unknown})"
    fi
    if ((${#INCLUDED_PROFILES[@]} > 0)); then
      _dr_ok 'included profiles' "${INCLUDED_PROFILES[*]}"
    fi
    if ((${#PHASE_ONE_SELECTED_OVERLAY_NAMES[@]} > 0)); then
      _dr_ok 'phase-one overlays' "${PHASE_ONE_SELECTED_OVERLAY_NAMES[*]}"
    fi
    for selector in "${DOT_PROFILE_SELECTOR_RECORDS[@]+"${DOT_PROFILE_SELECTOR_RECORDS[@]}"}"; do
      IFS='|' read -r source_class selector_path _selector_user _selector_host \
        selector_profile matched <<<"$selector"
      [[ $matched == true ]] || continue
      case $source_class in
        root) source='root' ;;
        local) source='machine-local' ;;
        personal) source='active personal overlay' ;;
        *) source=$source_class ;;
      esac
      _dr_ok "matching selector (${source})" \
        "${selector_path##*/} -> $selector_profile"
    done
  fi

  _dr_section "Overlays ($conf_count configured)"

  if [[ -n "${DOT_OVERLAY_DISCOVERY_ERROR:-}" ]]; then
    _dr_fail "overlay descriptor invalid" "$DOT_OVERLAY_DISCOVERY_ERROR"
  fi

  if [[ "$conf_count" -eq 0 && ! -f "$manifest" ]]; then
    _dr_skip "no overlays to check"
    return 0
  elif [[ "$conf_count" -eq 0 ]]; then
    _dr_skip "no active overlay descriptors"
  fi

  declare -A overlay_paths=() overlay_syncs=()
  declare -A active_records=()
  for entry in "${ACTIVE_OVERLAYS[@]+"${ACTIVE_OVERLAYS[@]}"}"; do
    name=${entry%%|*}
    active_records["$name"]=$entry
  done
  for lifecycle in "${DOT_OVERLAY_LIFECYCLE[@]+"${DOT_OVERLAY_LIFECYCLE[@]}"}"; do
    IFS='|' read -r name state _descriptor <<<"$lifecycle"
    case $state in
      not-selected)
        _dr_skip "$name: not selected"
        continue
        ;;
      selected-ineligible)
        _dr_skip "$name: selected but host/platform ineligible"
        continue
        ;;
      selected-optional-unavailable)
        _dr_warn "$name: selected optional but unavailable"
        continue
        ;;
      selected-unavailable)
        _dr_fail "$name: selected but unavailable"
        continue
        ;;
      active) ;;
      *)
        _dr_fail "$name: unknown overlay lifecycle state" "$state"
        continue
        ;;
    esac
    entry=${active_records[$name]:-}
    [[ -n $entry ]] || {
      _dr_fail "$name: active lifecycle record missing"
      continue
    }
    IFS='|' read -r name path url _descriptor optional sync <<<"$entry"
    sync=${sync:-git}
    overlay_paths["$name"]=$path
    overlay_syncs["$name"]=$sync

    if [[ $sync == none ]]; then
      if _overlay_local_source_validate "$path"; then
        _dr_ok "$name: local source available" "$(_dr_tilde "$path")"
      else
        _dr_fail "$name: local source unavailable" \
          "$(_dr_tilde "${REPLY:-$path/home}")"
      fi
      continue
    fi

    if ! _overlay_is_worktree "$path"; then
      if [[ "$optional" == "true" ]]; then
        _dr_skip "$name" "optional overlay not cloned"
        continue
      fi
      _dr_fail "$name: not cloned" "expected at $(_dr_tilde "$path")"
      continue
    fi
    _dr_ok "$name: cloned" "$(_dr_tilde "$path")"

    local actual_url expected_url
    _overlay_effective_url "$url"
    expected_url=$REPLY
    if _overlay_origin_matches "$path" "$expected_url"; then
      _dr_ok "$name: remote.origin.url matches conf"
    else
      actual_url=$REPLY
      _dr_warn "$name: remote URL drift" \
        "conf=$expected_url vs actual=$actual_url"
    fi

  done

  # Overlay symlinks — the manifest records which overlay owns each link.
  # Validate that links still resolve to that overlay, not merely to any
  # existing file, so stale/manual relinks are visible before hooks depend on
  # the wrong policy files.
  if [[ -f "$manifest" ]]; then
    declare -A manifest_owners=() manifest_targets=() manifest_exact=()
    # Keep the parser's dynamically scoped outputs local even though this call
    # uses it only as a validator and deliberately retains the raw values.
    # shellcheck disable=SC2034
    local issue_count=0 rel overlay_name line REPLY_REL REPLY_OWNER REPLY_TARGET
    local -a link_rels=() link_owners=() link_dsts=() link_expected=() link_exact=() link_targets=()
    while IFS= read -r line || [[ -n "$line" ]]; do
      if ! _overlay_parse_manifest_record "$line"; then
        ((issue_count++)) || true
        continue
      fi
      rel="$REPLY_REL"
      overlay_name="$REPLY_OWNER"
      manifest_owners["$rel"]="$overlay_name"
      manifest_targets["$rel"]="$REPLY_TARGET"
      if [[ "${line#*$'\t'}" == *$'\t'* ]]; then
        manifest_exact["$rel"]=1
      else
        manifest_exact["$rel"]=0
      fi
    done <"$manifest"

    for rel in "${!manifest_owners[@]}"; do
      overlay_name="${manifest_owners[$rel]}"
      local dst="$HOME/$rel"

      if [[ ! -L "$dst" ]]; then
        ((issue_count++)) || true
        continue
      fi
      if [[ ! -e "$dst" ]]; then
        ((issue_count++)) || true
        continue
      fi
      if [[ -z "${overlay_name:-}" || -z "${overlay_paths[$overlay_name]+x}" ]]; then
        ((issue_count++)) || true
        continue
      fi

      link_rels+=("$rel")
      link_owners+=("$overlay_name")
      link_dsts+=("$dst")
      link_expected+=("${manifest_targets[$rel]}")
      link_exact+=("${manifest_exact[$rel]}")
    done

    local batch_ok=0 readlink_file="" readlink_output_count=0
    if [[ "${#link_dsts[@]}" -gt 0 ]] && _logfile_create; then
      readlink_file="$REPLY"
      if readlink "${link_dsts[@]}" >"$readlink_file" 2>/dev/null; then
        mapfile -t link_targets <"$readlink_file"
        readlink_output_count="${#link_targets[@]}"
        if [[ "$readlink_output_count" -eq "${#link_dsts[@]}" ]]; then
          batch_ok=1
        fi
      fi
      _dot_cleanup_remove_path "$readlink_file" || true
    fi

    local i actual expected_lexical expected current_target
    for ((i = 0; i < ${#link_dsts[@]}; i++)); do
      rel="${link_rels[$i]}"
      overlay_name="${link_owners[$i]}"
      dst="${link_dsts[$i]}"
      actual=""
      if [[ "$batch_ok" -eq 1 ]]; then
        actual="${link_targets[$i]}"
      else
        actual=$(readlink "$dst" 2>/dev/null || true)
      fi

      expected_lexical="${link_expected[$i]}"
      if ! _overlay_record_link_target "$rel" "$overlay_name" \
        "${overlay_paths[$overlay_name]}" "${overlay_syncs[$overlay_name]}"; then
        ((issue_count++)) || true
        continue
      fi
      current_target="$REPLY"

      # Three-column manifests make the literal link target part of the
      # authority contract, but the current descriptor remains authoritative
      # after a local source path changes. The physical fallback remains only
      # for legacy two-column records created before exact targets were stored.
      if [[ "${link_exact[$i]}" -eq 1 ]]; then
        if [[ "$expected_lexical" != "$current_target" ||
          "$actual" != "$current_target" ]]; then
          ((issue_count++)) || true
        fi
        continue
      fi

      [[ "$actual" == "$current_target" ]] && continue

      expected="${overlay_paths[$overlay_name]}/home/$rel"
      if ! _dr_symlink_points_to "$dst" "$expected"; then
        ((issue_count++)) || true
      fi
    done
    if [[ "$issue_count" -eq 0 ]]; then
      _dr_ok "overlay symlinks healthy"
    else
      _dr_warn "$issue_count overlay symlink issue(s)" "run 'dot update' to re-link"
    fi
  fi
}
