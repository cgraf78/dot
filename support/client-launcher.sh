#!/usr/bin/env bash
# Permanent client-owned front door. Shdeps owns the checkout and public-link
# topology; this file only binds those two authorities before entering Dot.

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
  install_home=${SHDEPS_INSTALL_DIR:-$HOME/.local/share}
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

runtime=$checkout/bin/dot
public=$checkout/lib/dot/public
public_link=$HOME/.local/lib/dot

[[ -f $runtime && ! -L $runtime && -x $runtime ]] || dot_client_unavailable
[[ -d $public && ! -L $public ]] || dot_client_unavailable
[[ -L $public_link && $public_link -ef $public ]] || dot_client_unavailable

exec "$runtime" "$@"
