#!/usr/bin/env bash
# Legacy client-owned front door. New installs publish the native binary
# directly; existing client repositories may keep this exact file while init
# transitions them to the standalone release layout.

set -euo pipefail
CDPATH=

dot_client_unavailable() {
  printf '%s\n' \
    'dot: standalone runtime is unavailable' \
    'reinstall it with:' \
    '  curl -fsSL https://raw.githubusercontent.com/cgraf78/dot/main/install.sh | bash' \
    >&2
  exit 1
}

checkout=${CGRAF78_CHECKOUT_INSTALL_DIR:-}
if [[ -z $checkout ]]; then
  install_home=${XDG_DATA_HOME:-$HOME/.local/share}
  while [[ $install_home != / && $install_home == */ ]]; do
    install_home=${install_home%/}
  done
  case $install_home in
    /) checkout=/cgraf78/dot ;;
    /*) checkout=$install_home/cgraf78/dot ;;
    *) dot_client_unavailable ;;
  esac
fi

case $checkout in
  '' | / | */ | *//* | */./* | */. | */../* | */.. | *$'\n'* | *$'\r'*)
    dot_client_unavailable
    ;;
  /*) ;;
  *) dot_client_unavailable ;;
esac
case ${HOME:-} in
  '' | / | */ | *//* | */./* | */. | */../* | */.. | *$'\n'* | *$'\r'*)
    dot_client_unavailable
    ;;
  /*) ;;
  *) dot_client_unavailable ;;
esac

runtime=$checkout/dot
public=$checkout/lib/dot/public

[[ -f $runtime && ! -L $runtime && -x $runtime ]] || dot_client_unavailable
[[ -d $public && ! -L $public ]] || dot_client_unavailable

exec "$runtime" "$@"
