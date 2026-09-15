# shellcheck shell=bash

# Dot-specific runtime assertions for the shared smoke-release.sh. Android
# archives are cross-built and receive the shared ELF validation instead.
release_smoke_check() {
  local root=$1 smoke_home status

  "$root/dot" version >/dev/null || return 1
  "$root/dot" help >/dev/null || return 1

  # version/help bypass the output relay, so they cannot catch a relay that
  # fails to start (musl static binaries never execute .preinit_array, which
  # once bricked every relay command in a release). init --status on a bare
  # HOME is hermetic and must flow bytes through the relay. This runs before
  # the API move below: source-root discovery needs lib/dot present.
  smoke_home=$root/smoke-home
  mkdir -p "$smoke_home" || return 1
  status=$(HOME="$smoke_home" "$root/dot" init --status) || return 1
  [[ $status == initialization:* ]] || return 1

  # The package intentionally retains public shell APIs, so merely seeing a
  # lib/dot directory does not prove the binary is independent of the engine.
  # Remove that entire lookup root and exercise the native entry points again.
  if [[ -d $root/lib/dot ]]; then
    mv "$root/lib/dot" "$root/public-dot-api-away" || return 1
  fi
  "$root/dot" version >/dev/null || return 1
  "$root/dot" help >/dev/null || return 1
}
