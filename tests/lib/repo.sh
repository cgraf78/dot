#!/usr/bin/env bash

fixture_repo() {
  local name=$1 branch=$2
  local seed=$DOT_TEST_TMP/$name-seed origin=$DOT_TEST_TMP/$name.git
  shift 2

  mkdir -p "$seed"
  git -C "$seed" init -q
  git -C "$seed" config user.name fixture
  git -C "$seed" config user.email fixture@example.invalid
  while (($#)); do
    local path=$1 content=$2 mode=${3:-100644}
    shift 3
    if [[ $path == */* ]]; then
      mkdir -p "$seed/${path%/*}"
    fi
    if [[ $mode == 120000 ]]; then
      ln -s "$content" "$seed/$path"
    else
      printf '%s' "$content" >"$seed/$path"
      [[ $mode == 100755 ]] && chmod +x "$seed/$path"
    fi
    git -C "$seed" add "$path"
  done
  git -C "$seed" -c core.hooksPath=/dev/null commit -qm seed
  git -C "$seed" branch -M "$branch"
  git clone -q --bare "$seed" "$origin"
  git -C "$origin" symbolic-ref HEAD "refs/heads/$branch"
  REPLY=$origin
}

fixture_home() {
  local name=$1
  REPLY=$DOT_TEST_TMP/$name-home
  mkdir -p "$REPLY"
}

fixture_push() {
  local name=$1 branch=$2 path=$3 content=$4 mode=${5:-100644}
  local seed=$DOT_TEST_TMP/$name-seed origin=$DOT_TEST_TMP/$name.git
  if [[ $path == */* ]]; then
    mkdir -p "$seed/${path%/*}"
  fi
  rm -rf -- "$seed/${path:?}"
  if [[ $mode == 120000 ]]; then
    ln -s "$content" "$seed/$path"
  else
    printf '%s' "$content" >"$seed/$path"
    [[ $mode == 100755 ]] && chmod +x "$seed/$path"
  fi
  git -C "$seed" add "$path"
  git -C "$seed" -c core.hooksPath=/dev/null commit -qm update
  git -C "$seed" push -q "$origin" "HEAD:$branch"
}

run_dot() {
  local home=$1 state=$2
  shift 2
  HOME=$home XDG_STATE_HOME=$state XDG_CONFIG_HOME='' \
    "$DOT_TEST_ROOT/bin/dot" "$@"
}
