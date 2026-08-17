# shellcheck shell=bash
# Sourceable private runtime entry used by tests and the client forwarder.

_DOT_INIT_DIR=${BASH_SOURCE[0]%/*}
DOT_SOURCE_ROOT=$(cd -P -- "$_DOT_INIT_DIR/../.." && pwd -P) || return 1
export DOT_SOURCE_ROOT

# shellcheck source=public/api-version.sh
. "$_DOT_INIT_DIR/public/api-version.sh"
# shellcheck source=public/xdg.sh
. "$_DOT_INIT_DIR/public/xdg.sh"
# shellcheck source=public/ui.sh
. "$_DOT_INIT_DIR/public/ui.sh"
# shellcheck source=config.sh
. "$_DOT_INIT_DIR/config.sh"
dot_config_load || return
# shellcheck source=runtime.sh
. "$_DOT_INIT_DIR/runtime.sh"

unset _DOT_INIT_DIR
