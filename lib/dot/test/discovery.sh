# shellcheck shell=bash
# shellcheck disable=SC2154 # The coordinator owns option and source state.
# Built-in and trusted client-extension inventory for the test coordinator.

# Discover the provider-owned suite plus trusted client extension suites.
_dot_test_local_file_validate() {
  local path=$1
  [[ -f $path && ! -L $path && -x $path ]] || return 1
  _dot_extension_file_stat "$path"
}

_dot_test_suite_label() {
  local identity=${suite_names[$1]}
  if [[ $identity == dot ]]; then
    printf 'dot\n'
  else
    printf '%s-test\n' "$identity"
  fi
}

declare -A suite_names=()
declare -A suite_sources=()
available_scripts=()
available_names=()
if [[ -z ${DOT_TEST_TESTS_DIR:-} ]]; then
  available_scripts+=("$DOT_SOURCE_ROOT/tests/run")
  available_names+=(dot)
  suite_sources["$DOT_SOURCE_ROOT/tests/run"]=provider
fi

if [[ -n $TESTS_DIR && (-e $TESTS_DIR || -L $TESTS_DIR) ]]; then
  if [[ $DOT_TEST_SOURCE_HOME == "$DOT_TEST_HOST_HOME" &&
    -z ${DOT_TEST_TESTS_DIR:-} ]]; then
    if ! _dot_extension_root_validate ||
      ! _dot_extension_directory_validate "$TESTS_DIR"; then
      printf 'dot: unsafe test extension directory: %s\n' "$TESTS_DIR" >&2
      exit 1
    fi
  elif [[ ! -d $TESTS_DIR || -L $TESTS_DIR ]] ||
    ! _dot_extension_directory_stat "$TESTS_DIR"; then
    printf 'dot: unsafe test suite directory: %s\n' "$TESTS_DIR" >&2
    exit 1
  fi

  for f in "$TESTS_DIR"/*-test; do
    [[ -e $f || -L $f ]] || continue
    if [[ $DOT_TEST_SOURCE_HOME == "$DOT_TEST_HOST_HOME" &&
      -z ${DOT_TEST_TESTS_DIR:-} ]]; then
      _dot_extension_file_validate "$f" || {
        printf 'dot: unsafe test extension: %s\n' "$f" >&2
        exit 1
      }
      [[ -x $f ]] || {
        printf 'dot: test extension is not executable: %s\n' "$f" >&2
        exit 1
      }
      suite_sources[$f]=extension
    else
      _dot_test_local_file_validate "$f" || {
        printf 'dot: unsafe test suite: %s\n' "$f" >&2
        exit 1
      }
      suite_sources[$f]=local
    fi
    name=${f##*/}
    name=${name%-test}
    [[ $name =~ ^[a-z][a-z0-9-]*$ && $name != dot ]] || {
      printf 'dot: invalid or reserved test identity: %s\n' "${f##*/}" >&2
      exit 1
    }
    available_scripts+=("$f")
    available_names+=("$name")
  done
fi

_dot_test_suite_revalidate() {
  local script=$1
  case ${suite_sources[$script]:-} in
    provider)
      [[ $script == "$DOT_SOURCE_ROOT/tests/run" && -x $script ]] &&
        _dot_extension_file_stat "$script"
      ;;
    extension)
      _dot_extension_file_validate "$script" && [[ -x $script ]]
      ;;
    local)
      _dot_test_local_file_validate "$script"
      ;;
    *) return 1 ;;
  esac
}

declare -A seen_names=()
for name in "${available_names[@]}"; do
  [[ -z ${seen_names[$name]+x} ]] || {
    printf 'dot: duplicate test identity: %s\n' "$name" >&2
    exit 1
  }
  seen_names[$name]=1
done

if $list_only; then
  printf '%s\n' "${available_names[@]}"
  exit 0
fi

scripts=()
if [[ ${#filter[@]} -gt 0 ]]; then
  declare -A selected=()
  for name in "${filter[@]}"; do
    matched=false
    for i in "${!available_names[@]}"; do
      identity=${available_names[$i]}
      if [[ $identity == "$name" || $identity == "$name-"* ]]; then
        matched=true
        if [[ -z ${selected[$identity]+x} ]]; then
          f=${available_scripts[$i]}
          scripts+=("$f")
          suite_names[$f]=$identity
          selected[$identity]=1
        fi
      fi
    done
    $matched || {
      printf 'unknown test: %s\n' "$name" >&2
      exit 2
    }
  done
else
  for i in "${!available_names[@]}"; do
    if [[ ${available_names[$i]} == dot &&
      ${DOT_TEST_INCLUDE_PROVIDER:-0} != 1 ]]; then
      continue
    fi
    f=${available_scripts[$i]}
    scripts+=("$f")
    suite_names[$f]=${available_names[$i]}
  done

  # A stable partition keeps measured critical-path suites in the first worker
  # wave without encoding any client-specific naming policy in the provider.
  early_scripts=()
  ordinary_scripts=()
  for f in "${scripts[@]}"; do
    if $parallel && _dot_test_runs_early "$f"; then
      early_scripts+=("$f")
    else
      ordinary_scripts+=("$f")
    fi
  done
  scripts=("${early_scripts[@]}" "${ordinary_scripts[@]}")
fi

if [[ ${#scripts[@]} -eq 0 ]]; then
  echo "no tests found in $TESTS_DIR" >&2
  exit 1
fi
