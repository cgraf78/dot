# shellcheck shell=bash
# Private dependency-ordered runtime loader.

_DOT_RUNTIME_DIR=${BASH_SOURCE[0]%/*}

# shellcheck source=constants.sh
. "$_DOT_RUNTIME_DIR/constants.sh"
# shellcheck source=reserved.sh
. "$_DOT_RUNTIME_DIR/reserved.sh"
# shellcheck source=log.sh
. "$_DOT_RUNTIME_DIR/log.sh"
# shellcheck source=progress-ui.sh
. "$_DOT_RUNTIME_DIR/progress-ui.sh"
# shellcheck source=resources.sh
. "$_DOT_RUNTIME_DIR/resources.sh"
# shellcheck source=run.sh
. "$_DOT_RUNTIME_DIR/run.sh"
# shellcheck source=temp.sh
. "$_DOT_RUNTIME_DIR/temp.sh"
# shellcheck source=platform.sh
. "$_DOT_RUNTIME_DIR/platform.sh"
# shellcheck source=overlays.sh
. "$_DOT_RUNTIME_DIR/overlays.sh"
# shellcheck source=merge-block.sh
. "$_DOT_RUNTIME_DIR/merge-block.sh"
# shellcheck source=families.sh
. "$_DOT_RUNTIME_DIR/families.sh"
# shellcheck source=merge-hooks.sh
. "$_DOT_RUNTIME_DIR/merge-hooks.sh"
# shellcheck source=hook-api.sh
. "$_DOT_RUNTIME_DIR/hook-api.sh"
# shellcheck source=init-client.sh
. "$_DOT_RUNTIME_DIR/init-client.sh"
# Initialization may inspect a durable transaction while the prior client Git
# launcher is already missing its tracked helper. Bind one host Git before the
# repository model reads that record, then retain it for the whole process.
if [[ ${DOT_ORIGINAL_ARGV[0]:-} == init ]]; then
  _dot_init_bind_host_git
fi
# shellcheck source=repos/model.sh
. "$_DOT_RUNTIME_DIR/repos/model.sh"
# shellcheck source=repos/api.sh
. "$_DOT_RUNTIME_DIR/repos/api.sh"
# shellcheck source=merges.sh
. "$_DOT_RUNTIME_DIR/merges.sh"
# shellcheck source=pre-sync.sh
. "$_DOT_RUNTIME_DIR/pre-sync.sh"
# shellcheck source=update-lock.sh
. "$_DOT_RUNTIME_DIR/update-lock.sh"
# shellcheck source=providers/shdeps.sh
. "$_DOT_RUNTIME_DIR/providers/shdeps.sh"
# shellcheck source=providers/shdeps-ui.sh
. "$_DOT_RUNTIME_DIR/providers/shdeps-ui.sh"
# shellcheck source=update.sh
. "$_DOT_RUNTIME_DIR/update.sh"
# shellcheck source=doctor-api.sh
. "$_DOT_RUNTIME_DIR/doctor-api.sh"
# shellcheck source=doctor.sh
. "$_DOT_RUNTIME_DIR/doctor.sh"
# shellcheck source=test.sh
. "$_DOT_RUNTIME_DIR/test.sh"
unset _DOT_RUNTIME_DIR
