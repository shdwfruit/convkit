#!/usr/bin/env bash
# Builds this checkout's conv and installs it as conv-BRANCH, so a branch
# can be tried from any terminal next to the released conv:
#
#   scripts/dev-install.sh          # feat/copy-paste -> ~/.local/bin/conv-copy-paste
#   scripts/dev-install.sh --clean  # remove builds of merged or deleted branches
#
# `conv-copy-paste --version` says which commit it was built from:
# `conv 0.6.1 (feat/copy-paste ce31260)`. CONV_DEV_BIN sets the folder.

set -euo pipefail

bin="${CONV_DEV_BIN:-$HOME/.local/bin}"
root="$(git rev-parse --show-toplevel)"

# The branch a dev build was made from, from its --version, or nothing for
# anything else named conv-*.
built_from() {
  "$1" --version 2>/dev/null | sed -n 's/^conv [^ ]* (\(.*\) \([0-9a-f]*\)\(+dirty\)\{0,1\})$/\1 \2/p'
}

if [ "${1:-}" = "--clean" ]; then
  main="$(git -C "$root" rev-parse --verify --quiet main || true)"
  for f in "$bin"/conv-*; do
    [ -x "$f" ] || continue
    read -r branch sha <<<"$(built_from "$f")" || true
    [ -n "${branch:-}" ] || continue
    why=""
    if ! git -C "$root" show-ref --verify --quiet "refs/heads/$branch"; then
      why="its branch is gone"
    elif [ "$branch" != main ] && [ -n "$main" ] &&
      git -C "$root" merge-base --is-ancestor "$branch" "$main" &&
      [ "$(git -C "$root" rev-parse "$branch")" != "$main" ]; then
      why="$branch is merged"
    fi
    if [ -n "$why" ]; then
      rm "$f"
      echo "removed $(basename "$f"): $why"
    fi
  done
  exit 0
fi

branch="$(git -C "$root" rev-parse --abbrev-ref HEAD)"
sha="$(git -C "$root" rev-parse --short HEAD)"
[ "$branch" = HEAD ] && branch="$sha"
dirty=""
[ -n "$(git -C "$root" status --porcelain --untracked-files=no)" ] && dirty="+dirty"
name="conv-${branch##*/}"

CONV_DEV_STAMP="$branch $sha$dirty" cargo build --release -p convkit --manifest-path "$root/Cargo.toml"
mkdir -p "$bin"
install -m 755 "$root/target/release/conv" "$bin/$name"
echo "installed $bin/$name: $("$bin/$name" --version)"
case ":$PATH:" in
  *":$bin:"*) ;;
  *) echo "note: $bin isn't on your PATH; add it to run $name by name" ;;
esac
