# shellcheck shell=bash

# Dot-specific runtime assertions for the shared smoke-release.sh. Android
# archives are cross-built and receive the shared ELF validation instead.
release_smoke_check() {
  local root=$1

  "$root/dot" version >/dev/null || return 1
  "$root/dot" help >/dev/null || return 1

  # The package intentionally retains public shell APIs, so merely seeing a
  # lib/dot directory does not prove the binary is independent of the engine.
  # Remove that entire lookup root and exercise the native entry points again.
  if [[ -d $root/lib/dot ]]; then
    mv "$root/lib/dot" "$root/public-dot-api-away" || return 1
  fi
  "$root/dot" version >/dev/null || return 1
  "$root/dot" help >/dev/null || return 1
}
