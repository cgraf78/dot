# shellcheck shell=bash
# shellcheck disable=SC2034 # Discovery consumes the selected suite directory.
# Source-home and tool environment selection for the test coordinator.

_dot_test_realpath() {
  local path="$1"
  realpath "$path" 2>/dev/null || printf '%s\n' "$path"
}

_dot_test_tree_ok() {
  local root="$1"
  [[ -n "$root" ]] || return 1
  [[ -d "$root/.local/lib/dotfiles/tests" &&
    -f "$root/.local/lib/dotfiles/tests/helpers.sh" ]]
}

_dot_test_matches_base() {
  local dir="$1" candidate_common client_common line registered
  # A copied tests directory in an unrelated repository must not acquire base
  # client authority merely because the command was launched below it.
  _base_repo_exists || return 1
  candidate_common=$(_dot_sanitized_git -C "$dir" rev-parse --git-common-dir) ||
    return 1
  [[ $candidate_common == /* ]] || candidate_common=$dir/$candidate_common
  candidate_common=$(_dot_test_realpath "$candidate_common")
  client_common=$(_base_git rev-parse --git-common-dir 2>/dev/null) || return 1
  [[ $client_common == /* ]] || client_common=$HOME/$client_common
  client_common=$(_dot_test_realpath "$client_common")
  [[ $candidate_common == "$client_common" ]] || return 1

  # A copied linked-worktree .git file still resolves to the original common
  # directory. Require Git's own worktree registry to name this exact path so
  # stale copies cannot masquerade as an authorized source checkout.
  while IFS= read -r line; do
    [[ $line == worktree\ * ]] || continue
    registered=$(_dot_test_realpath "${line#worktree }")
    [[ $registered == "$dir" ]] && return 0
  done < <(_base_git worktree list --porcelain 2>/dev/null)
  return 1
}

_dot_test_pwd_home() {
  local dir
  dir=$(_dot_sanitized_git -C "${PWD:-.}" rev-parse --show-toplevel 2>/dev/null) ||
    return 1
  dir=$(_dot_test_realpath "$dir")
  _dot_test_tree_ok "$dir" || return 1
  _dot_test_matches_base "$dir" || return 1
  printf '%s\n' "$dir"
}

_dot_test_configure_home() {
  local caller_home source_home pwd_home
  caller_home=$(_dot_test_realpath "${HOME:?HOME must be set}")

  if [[ -n "${DOT_TEST_SOURCE_HOME:-}" ]]; then
    source_home=$(_dot_test_realpath "$DOT_TEST_SOURCE_HOME")
  else
    pwd_home=$(_dot_test_pwd_home 2>/dev/null || true)
    if [[ -n "$pwd_home" && "$pwd_home" != "$caller_home" ]]; then
      source_home="$pwd_home"
    else
      source_home="$caller_home"
    fi
  fi

  if [[ $source_home != "$caller_home" ]]; then
    _dot_test_tree_ok "$source_home" || {
      echo "dot test: invalid source home: $source_home" >&2
      exit 2
    }
    _dot_test_matches_base "$source_home" || {
      echo "dot test: source home does not match the configured base repository: $source_home" >&2
      exit 2
    }
  fi

  # The coordinator's actual HOME is the trust anchor. A caller-supplied host
  # value is child metadata, not authority to redirect repository validation.
  DOT_TEST_HOST_HOME=$caller_home
  DOT_TEST_SOURCE_HOME=$source_home
  export DOT_TEST_HOST_HOME DOT_TEST_SOURCE_HOME
}

_dot_test_configure_home

_dot_test_select_system_git() {
  local candidate rejected
  local -a candidates=()

  [[ -z ${DOT_TEST_SYSTEM_GIT:-} ]] || candidates+=("$DOT_TEST_SYSTEM_GIT")
  candidates+=(/usr/bin/git /bin/git /opt/homebrew/bin/git)
  [[ -z ${PREFIX:-} ]] || candidates+=("$PREFIX/bin/git")

  for candidate in "${candidates[@]}"; do
    [[ $candidate == /* && -x $candidate && ! -d $candidate ]] || continue
    for rejected in "$DOT_TEST_SOURCE_HOME/.local/bin/git" \
      "$DOT_TEST_HOST_HOME/.local/bin/git"; do
      [[ ! -e $rejected || ! $candidate -ef $rejected ]] || continue 2
    done
    printf '%s\n' "$candidate"
    return 0
  done

  return 1
}

_dot_test_configure_git_backend() {
  local backend_dir="$1" candidate source_bin

  candidate=$(_dot_test_select_system_git) || {
    echo 'dot test: could not find a system Git executable' >&2
    return 1
  }
  mkdir -m 700 "$backend_dir" || return 1
  ln -s "$candidate" "$backend_dir/git" || return 1

  # Keep the tracked launcher first for its integration coverage, but give it
  # a deterministic host-Git backend before site wrappers. Suites that need
  # raw Git can still move DOT_TEST_SYSTEM_GIT first with the shared helper.
  source_bin=$DOT_TEST_SOURCE_HOME/.local/bin
  case $PATH in
    "$source_bin") PATH="$source_bin:$backend_dir" ;;
    "$source_bin:"*) PATH="$source_bin:$backend_dir:${PATH#"$source_bin:"}" ;;
    *) PATH="$source_bin:$backend_dir:$PATH" ;;
  esac
  DOT_TEST_SYSTEM_GIT=$candidate
  export DOT_TEST_SYSTEM_GIT PATH
}

if [[ -n ${DOT_TEST_TESTS_DIR:-} ]]; then
  TESTS_DIR=$DOT_TEST_TESTS_DIR
elif [[ $DOT_TEST_SOURCE_HOME != "$DOT_TEST_HOST_HOME" ]]; then
  TESTS_DIR=$DOT_TEST_SOURCE_HOME/.local/lib/dotfiles/tests
else
  TESTS_DIR=${DOT_EXTENSIONS_DIR:-}/tests
fi
