# shellcheck shell=bash
# Public source point for dot repository operations.
#
# `dot` intentionally sources one repos API file instead of each implementation
# module. Repository behavior has several cross-cutting invariants (typed base
# topology dispatch, overlay discovery, skip-worktree overlays, and dirty-state
# normalization), and this file is the reviewable dependency order for those
# pieces.
#
# Keep the module dir in a repos-specific variable. `init.sh` sources this file
# in the middle of its own source list, so clobbering its `_dir` variable would
# make later runtime modules resolve relative to `lib/dot/repos/` instead of
# `lib/dot/`.

_DOT_REPOS_DIR="${BASH_SOURCE[0]%/*}"

# Source order matters:
# - config/git define repo policy and base-vs-overlay dispatch primitives.
# - dirty is needed by pull and simple commands before they touch repo state.
# - pull defines progress helpers shared by overlay linking.
# - overlays defines skip-worktree restoration used by pull at runtime.
# - commands is last because it is the public simple-command surface.
. "$_DOT_REPOS_DIR/config.sh"
. "$_DOT_REPOS_DIR/git.sh"
. "$_DOT_REPOS_DIR/dirty.sh"
. "$_DOT_REPOS_DIR/pull.sh"
. "$_DOT_REPOS_DIR/overlays.sh"
. "$_DOT_REPOS_DIR/commands.sh"
