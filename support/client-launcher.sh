#!/usr/bin/env bash
# Generated client-owned adapter for deployments that cannot replace the
# historical regular ~/.local/bin/dot file with Shdeps' command symlink.

set -euo pipefail

[[ -n ${HOME:-} ]] || {
  printf 'dot: HOME is not set\n' >&2
  exit 1
}
install_home=${SHDEPS_INSTALL_DIR:-$HOME/.local/share}
while [[ "$install_home" != / && "$install_home" == */ ]]; do
  install_home=${install_home%/}
done
case $install_home in
  '' | *//* | */./* | */. | */../* | */.. | *$'\n'* | *$'\r'*)
    printf 'dot: SHDEPS_INSTALL_DIR must be normalized\n' >&2
    exit 1
    ;;
  /) dot_runtime=/cgraf78/dot/bin/dot ;;
  /*) dot_runtime=$install_home/cgraf78/dot/bin/dot ;;
  *)
    printf 'dot: SHDEPS_INSTALL_DIR must be an absolute path\n' >&2
    exit 1
    ;;
esac

if [[ ! -x "$dot_runtime" ]]; then
  cat >&2 <<EOF
dot: standalone runtime is missing: $dot_runtime
reinstall it with:
  curl -fsSL https://raw.githubusercontent.com/cgraf78/dot/main/install.sh | bash
EOF
  exit 1
fi
exec "$dot_runtime" "$@"
