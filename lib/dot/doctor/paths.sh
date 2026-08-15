# shellcheck shell=bash
# dot doctor: Paths checks.
#
# shellcheck disable=SC2088  # tilde strings here are display text.

# Resolve directory indirection without dereferencing the final component. That
# lets doctor compare equivalent targets through symlinked parents or `..` while
# preserving the managed leaf's identity. Fully resolving the leaf could make its
# eventual destination look equivalent to the intended leaf itself. Use portable
# shell splitting plus pwd -P instead of relying on platform-specific realpath or
# readlink -f behavior across macOS and Linux.
_dr_physical_path() {
  local path="$1" dir base
  while [[ "$path" != / && "$path" == */ ]]; do
    path="${path%/}"
  done
  case "$path" in
    /)
      dir=/
      base=/
      ;;
    */*)
      dir="${path%/*}"
      base="${path##*/}"
      [[ -n "$dir" ]] || dir=/
      ;;
    *)
      dir=.
      base="$path"
      ;;
  esac
  [[ -d "$dir" ]] || return 1
  dir=$(cd "$dir" && pwd -P) || return 1
  printf '%s/%s\n' "$dir" "$base"
}
_dr_symlink_target_path() {
  local link="$1"
  local target link_dir
  target=$(readlink "$link") || return 1
  case "$target" in
    /*) _dr_physical_path "$target" ;;
    *)
      case "$link" in
        */*)
          link_dir="${link%/*}"
          [[ -n "$link_dir" ]] || link_dir=/
          ;;
        *) link_dir=. ;;
      esac
      _dr_physical_path "$link_dir/$target"
      ;;
  esac
}
_dr_symlink_points_to() {
  local link="$1" expected="$2"
  local actual expected_physical
  [[ -e "$expected" ]] || return 1
  actual=$(_dr_symlink_target_path "$link") || return 1
  expected_physical=$(_dr_physical_path "$expected") || return 1
  [[ "$actual" == "$expected_physical" ]]
}
_dr_tilde() {
  local p="$1"
  case "$p" in
    "$HOME") echo "~" ;;
    "$HOME"/*) echo "~/${p#"$HOME"/}" ;;
    *) echo "$p" ;;
  esac
}
