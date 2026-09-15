# shellcheck shell=bash
# Read-only repository identity helpers exposed inside hook API workers.

_overlay_is_worktree() {
  local path=$1 checkout_root git_root
  [[ -d $path ]] && [[ -d $path/.git || -f $path/.git ]] || return 1
  checkout_root=$(cd -P -- "$path" 2>/dev/null && pwd -P) || return 1
  git_root=$(git -C "$path" rev-parse --show-toplevel 2>/dev/null) || return 1
  git_root=$(cd -P -- "$git_root" 2>/dev/null && pwd -P) || return 1
  [[ $checkout_root == "$git_root" ]]
}

_overlay_effective_url() {
  local url=$1
  case $url in
    \~) REPLY=$HOME ;;
    \~/*) REPLY=$HOME/${url#\~/} ;;
    /* | [A-Za-z]:[\\/]* | *:*) REPLY=$url ;;
    *) REPLY=$HOME/$url ;;
  esac
}

_overlay_origin_matches() {
  local path=$1 expected=$2
  local -a urls=()
  mapfile -t urls < <(git -C "$path" config --get-all remote.origin.url 2>/dev/null)
  case ${#urls[@]} in
    0)
      REPLY='<missing>'
      return 1
      ;;
    1)
      REPLY=${urls[0]}
      [[ $REPLY == "$expected" ]]
      ;;
    *)
      REPLY='<multiple origin URLs>'
      return 1
      ;;
  esac
}

_overlay_checkout_matches() {
  local path=$1 url=$2 expected
  _overlay_is_worktree "$path" || {
    REPLY='<not a Git worktree>'
    return 1
  }
  _overlay_effective_url "$url"
  expected=$REPLY
  _overlay_origin_matches "$path" "$expected"
}
