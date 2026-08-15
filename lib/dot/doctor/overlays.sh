# shellcheck shell=bash
# dot doctor: Overlays checks.

_dr_check_overlays() {
  local conf_dir conf_count=0 manifest="$DOT_OVERLAY_MANIFEST"
  conf_dir="$(_overlay_conf_dir)"
  [[ -d "$conf_dir" ]] && conf_count=$(find "$conf_dir" -maxdepth 1 -name '*.conf' -type f 2>/dev/null | wc -l | tr -d ' ')
  _dr_section "Overlays ($conf_count configured)"

  local discovery_invalid=0
  if [[ -n "${DOT_OVERLAY_DISCOVERY_ERROR:-}" ]]; then
    _dr_fail "overlay descriptor invalid" "$DOT_OVERLAY_DISCOVERY_ERROR"
    discovery_invalid=1
  fi

  if [[ "$conf_count" -eq 0 && ! -f "$manifest" ]]; then
    _dr_skip "no overlays to check"
    return 0
  elif [[ "$conf_count" -eq 0 ]]; then
    _dr_skip "no active overlay descriptors"
  fi

  # Walk each conf, check against the parsed OVERLAYS array (filtered set).
  declare -A overlay_paths=() overlay_syncs=()
  local f name want_url descriptor_sync
  for f in "$conf_dir"/*.conf; do
    [[ "$discovery_invalid" -eq 0 ]] || break
    [[ -f "$f" ]] || continue
    descriptor_sync=$(awk -F= '/^sync=/ {sub(/^sync=/, ""); print; exit}' "$f")
    name=$(_overlay_name "$f" "${descriptor_sync:-git}")
    # Extract URL from conf directly (OVERLAYS may have filtered it out).
    want_url=$(awk -F= '/^url=/ {sub(/^url=/, ""); print; exit}' "$f")

    # Is this overlay active for this host?
    local active=0 entry path _conf optional sync
    for entry in "${OVERLAYS[@]+"${OVERLAYS[@]}"}"; do
      IFS='|' read -r n path _ _conf optional sync <<<"$entry"
      sync="${sync:-git}"
      if [[ "$n" == "$name" && "$_conf" == "$f" ]]; then
        active=1
        break
      fi
    done

    if [[ "$active" -eq 0 ]]; then
      _dr_skip "$name" "filtered out for this machine"
      continue
    fi
    overlay_paths["$name"]="$path"
    overlay_syncs["$name"]="$sync"

    if [[ "$sync" == "none" ]]; then
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

    # Origin URL matches conf
    local actual_url expected_url
    _overlay_effective_url "$want_url"
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
